//! Turning the config's `command` string into an argv. Pure — no process, no
//! environment, no I/O.
//!
//! # Two levels of shell, and only one of them is real
//!
//! The template is split into words by [`shlex::split`], and the words are
//! handed to `Command` directly: **there is no local shell** (HLD §5). The only
//! shell in the picture is the remote one, inside the quotes of
//! `'docker logs -f …'`. So `2>&1` written inside those quotes works, and the
//! same `2>&1` written outside them is a literal argv entry — which is exactly
//! what the starter config says.
//!
//! # Substitution happens in the words, never in the string
//!
//! This is the load-bearing detail of the module. Substituting into the
//! template *before* splitting would let the value of an argument change how
//! the template is split, so a value containing a space would silently become
//! two argv entries — and a value containing a quote could re-open a quoted
//! section. Splitting first and substituting into each word makes the shape of
//! the argv a property of the template alone, whatever the arguments are.
//!
//! The one exception proves the rule: `{@}` *does* change the number of argv
//! entries, and it is allowed to because the count comes from how many
//! arguments were typed, never from what is inside them.
//!
//! # What counts as a placeholder
//!
//! Exactly three shapes are ever rewritten, and every other byte of the
//! template survives verbatim:
//!
//! | in the template | becomes | why |
//! |---|---|---|
//! | `{0}`, `{7}` | the argument at that index | the feature |
//! | `{@}` | the arguments no index took | the variadic tail (HLD §5) |
//! | `{{0}}`, `{{@}}` | the literal text `{0}`, `{@}` | the escape hatch |
//! | `{}` | `{}` | `find -exec {} \;` must pass through |
//! | `{{.Names}}` | `{{.Names}}` | `docker --format '{{.Names}}'` must pass through |
//! | `{abc}`, `{ 0 }` | itself | not digits and not `@`, so not a placeholder |
//! | `{01}` | itself | `{01}` and `{1}` would be two spellings of one index |
//! | `{`, `}`, `{{`, `}}` | themselves | an unpaired brace is data |
//!
//! Two shell rules come along with [`shlex::split`] and are worth knowing
//! about, because neither is obvious in a TOML file: a `#` that **starts** a
//! word opens a comment and swallows the rest of the template (`#` inside a
//! word, as in `myapp#1`, is ordinary text), and a backslash escapes the next
//! character — which is why `find -exec {} \;` has to be written in a TOML
//! *literal* string (`'…'`) or with the backslash doubled, since `\;` is not a
//! valid escape in a TOML basic string.
//!
//! The `{{` rule is narrower than a format-string one on purpose, and the
//! difference matters: `{{` escapes to `{` **only** when it wraps a valid index
//! or an `@`, and closes with `}}`. A blanket `{{` → `{` rule would rewrite
//! `docker --format '{{.Names}}'` to `{.Names}`, which HLD §5 and the starter
//! config both promise it will not do.
//!
//! # `{@}`: the remaining arguments
//!
//! One rule (HLD §5): let `N` be the highest index the template uses. Then
//! `{0}…{N}` take `args[0..=N]` and `{@}` is `args[N+1..]`; a template with no
//! indexed placeholder at all gives `{@}` everything. That makes the arity a
//! **minimum** rather than an exact count — a template without `{@}` still
//! demands exactly `N+1`.
//!
//! Where `{@}` sits decides how it expands, and there are exactly two places:
//!
//! * **A word of its own** (`ssh {0} {@}`) splices: one argv entry per
//!   remaining argument, and *no* entry at all when none are left — the
//!   behaviour of a shell's `"$@"`.
//! * **Inside a word** (`ssh -tt {0} 'docker logs -f {@}'`, where `shlex` hands
//!   back the single word `docker logs -f {@}`) joins the remaining arguments
//!   with one space, in place. Re-splitting them is the remote shell's job, and
//!   it is the reason this form is not a poor relation of the first: the
//!   flagship template of HLD §5 is exactly this shape, and "a word of its own"
//!   is not physically available inside quotes. With nothing left over, `{@}`
//!   becomes the empty string, leaving the harmless trailing space the remote
//!   shell discards along with every other run of whitespace.
//!
//! HLD §5 states the rule as "`{@}` обязан быть отдельным словом целиком",
//! which its own flagship example contradicts — `'docker logs -f {@}'` is one
//! shlex word, so `{@}` can never be a separate argv word there. The rule that
//! holds for both examples, and the one implemented here, is the narrower half
//! of the same idea: `{@}` may not be **glued to a non-whitespace neighbour**.
//! `myapp-{@}-1` and `--tail={@}` are refused, because a list of arguments
//! pasted into the middle of one word has no meaning anyone would predict.
//! A second `{@}` is refused too — it could only duplicate the same tail.

use crate::command::validate::SafeArg;

/// A template that cannot be used.
///
/// Every variant is a defect in the config file or in the invocation, never a
/// runtime condition — which is why they are all worth a loud refusal rather
/// than a fallback.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// `shlex::split` returned `None`: a quote or a trailing backslash is left
    /// open. Worth its own message because the raw `None` says nothing, and a
    /// half-written template is a common way to reach it.
    #[error("command template has an unclosed quote or a trailing backslash")]
    Unbalanced,

    /// The template is empty, or nothing but whitespace: there is no program
    /// to run.
    #[error("command template is empty")]
    Empty,

    /// Wrong number of arguments for a template **without** `{@}`. The exact
    /// text of HLD §6.
    #[error("template needs {needed} argument{}, got {given}", plural(*.needed))]
    Arity {
        /// How many arguments the template's highest index implies.
        needed: usize,
        /// How many were given.
        given: usize,
    },

    /// Too few arguments for a template **with** `{@}`, whose arity has a floor
    /// but no ceiling (HLD §5): `{0}` still has to be filled before `{@}` can
    /// take what is left.
    #[error("template needs at least {needed} argument{}, got {given}", plural(*.needed))]
    TooFew {
        /// The floor: one more than the template's highest index.
        needed: usize,
        /// How many were given.
        given: usize,
    },

    /// The template uses `{2}` but never `{1}`, so the second argument could
    /// only ever be typed and thrown away. Reported rather than tolerated for
    /// the same reason HLD §5 rejects a surplus argument: it is a typo.
    #[error("command template uses {{{max}}} but never {{{missing}}}")]
    SkippedIndex {
        /// The lowest index that is never used.
        missing: usize,
        /// The highest index that is.
        max: usize,
    },

    /// `{@}` is glued to something that is not whitespace — `myapp-{@}-1`,
    /// `--tail={@}`, `{0}{@}`.
    ///
    /// Refused rather than given a meaning, because every meaning would be a
    /// surprise: `{@}` stands for a *list*, and pasting a list into the middle
    /// of a word has no answer that is obviously right for both one argument
    /// and three.
    #[error(
        "command template glues {{@}} to its neighbour in {word:?}: \
         it stands for the remaining arguments, so it needs whitespace around it"
    )]
    RestGlued {
        /// The offending word, as `shlex` handed it back.
        word: String,
    },

    /// Two `{@}` in one template. The second could only repeat the first —
    /// there is one tail, and it has already been taken.
    #[error("command template uses {{@}} more than once")]
    DuplicateRest,
}

/// `""` for one, `"s"` for everything else. Referenced from the `#[error]`
/// attributes above, which spell the field `*.needed` so this takes the
/// `usize` by value rather than by reference (`trivially_copy_pass_by_ref`).
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// One piece of one word.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// Text copied through untouched, escapes already resolved. Never empty.
    Literal(String),
    /// `{N}` — the 0-based index of the argument to drop in here.
    Placeholder(usize),
    /// `{@}` — every argument no index took.
    Rest,
}

/// One argv word of the template, classified by how it expands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Word {
    /// The word is nothing but `{@}`, so it splices: one argv entry per
    /// remaining argument, and none at all when there are none.
    Rest,
    /// Everything else, including a word that merely *contains* `{@}`. Always
    /// exactly one argv entry.
    Parts(Vec<Segment>),
}

/// A parsed `command` template: the argv shape, with holes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// One entry per template word. The first produces the program — unless it
    /// is a `{@}` that expands to nothing, which is the one way a template can
    /// still end up with no argv at all.
    words: Vec<Word>,
    /// Every index the template uses, sorted and deduplicated. This is the set
    /// the arity check is made against, which is what lets a gap be spotted.
    used: Vec<usize>,
    /// Whether `{@}` appears anywhere. The difference between an exact arity
    /// and a floor (HLD §5).
    variadic: bool,
}

/// Parses a `command` template into words.
///
/// # Errors
///
/// [`TemplateError::Unbalanced`] for a template the shell rules cannot split,
/// [`TemplateError::Empty`] for one with no words,
/// [`TemplateError::DuplicateRest`] and [`TemplateError::RestGlued`] for the
/// two ways `{@}` can be written wrong, and [`TemplateError::SkippedIndex`] for
/// indices with a hole in them. An arity mismatch is not detected here — it
/// depends on the invocation, not on the template — see
/// [`Template::check_arity`].
pub fn parse(template: &str) -> Result<Template, TemplateError> {
    let raw = shlex::split(template).ok_or(TemplateError::Unbalanced)?;
    if raw.is_empty() {
        return Err(TemplateError::Empty);
    }

    let scanned: Vec<Vec<Segment>> = raw.iter().map(|word| scan(word)).collect();

    // Counted across the whole template before any word is judged on its own,
    // so that `{@}{@}` is reported as the duplicate it is rather than as a
    // gluing accident.
    let rests: usize = scanned
        .iter()
        .map(|segments| {
            segments
                .iter()
                .filter(|s| matches!(s, Segment::Rest))
                .count()
        })
        .sum();
    if rests > 1 {
        return Err(TemplateError::DuplicateRest);
    }

    for (text, segments) in raw.iter().zip(&scanned) {
        if !rest_is_separated(segments) {
            return Err(TemplateError::RestGlued { word: text.clone() });
        }
    }

    let mut used: Vec<usize> = scanned
        .iter()
        .flatten()
        .filter_map(|segment| match segment {
            Segment::Placeholder(index) => Some(*index),
            Segment::Literal(_) | Segment::Rest => None,
        })
        .collect();
    used.sort_unstable();
    used.dedup();

    // A hole in the indices means one of the arguments the arity check will
    // demand could never be used for anything.
    //
    // Written as nested `if let`s rather than as a let chain on purpose: let
    // chains are stable from Rust 1.88, and this package declares MSRV 1.87
    // (HLD §1, and the CI job of §10 builds against it). `clippy::msrv` does not
    // catch a *language* feature, only a std API, so nothing would have flagged
    // it before the MSRV job went red.
    if let Some(&max) = used.last() {
        if let Some(missing) = (0..max).find(|index| used.binary_search(index).is_err()) {
            return Err(TemplateError::SkippedIndex { missing, max });
        }
    }

    Ok(Template {
        words: scanned.into_iter().map(classify).collect(),
        used,
        variadic: rests == 1,
    })
}

impl Template {
    /// The highest `{N}` the template uses, if it uses any.
    ///
    /// Needed by the input-mode table of HLD §6, which has to tell a template
    /// with placeholders (`hog` on a bare terminal is an arity error) from one
    /// without (`hog` on a bare terminal runs it).
    pub fn max_index(&self) -> Option<usize> {
        self.used.last().copied()
    }

    /// How many positional arguments this template requires: one more than
    /// [`max_index`](Self::max_index), or zero when it has no indexed
    /// placeholders.
    ///
    /// Exact for a template without `{@}` — a surplus argument is an error too
    /// (HLD §5) — and a floor for one with it. [`is_variadic`](Self::is_variadic)
    /// says which, and [`check_arity`](Self::check_arity) applies the right one.
    ///
    /// It doubles as the boundary between the two halves of the argument list:
    /// `args[..required_arity()]` belong to the indices, `args[required_arity()..]`
    /// to `{@}`.
    pub fn required_arity(&self) -> usize {
        self.max_index().map_or(0, |max| max.saturating_add(1))
    }

    /// Whether the template uses `{@}`, i.e. whether its arity has a ceiling.
    pub fn is_variadic(&self) -> bool {
        self.variadic
    }

    /// The indices the template uses, ascending. Always `0..required_arity()`:
    /// [`parse`] rejects a set with a hole in it.
    pub fn used_indices(&self) -> &[usize] {
        &self.used
    }

    /// How many argv words the template *is*, counting a standalone `{@}` as
    /// one.
    ///
    /// Equal to the length of the rendered argv for every template without a
    /// standalone `{@}`; with one, the rendered argv is longer or shorter by
    /// however many arguments were left over.
    pub fn word_count(&self) -> usize {
        self.words.len()
    }

    /// Checks the argument count against the template.
    ///
    /// # Errors
    ///
    /// [`TemplateError::Arity`] both when there are too few arguments and when
    /// there are too many. Too many is an error and not a shrug because a
    /// surplus argument is nearly always a typo (HLD §5), and silently
    /// dropping it would run a command the user did not mean to run. With
    /// `{@}` in the template there is no such thing as a surplus argument, and
    /// only the floor is checked — [`TemplateError::TooFew`].
    pub fn check_arity(&self, given: usize) -> Result<(), TemplateError> {
        let needed = self.required_arity();
        if self.variadic {
            if given < needed {
                return Err(TemplateError::TooFew { needed, given });
            }
        } else if given != needed {
            return Err(TemplateError::Arity { needed, given });
        }
        Ok(())
    }

    /// Substitutes the arguments and returns the finished argv.
    ///
    /// Takes `SafeArg`, so the whitelist of HLD §5 has necessarily already run:
    /// there is no way to call this with a string nobody checked.
    ///
    /// # Errors
    ///
    /// [`TemplateError::Arity`] or [`TemplateError::TooFew`] if the count does
    /// not match the template.
    pub fn render(&self, args: &[SafeArg<'_>]) -> Result<Vec<String>, TemplateError> {
        self.check_arity(args.len())?;

        // The whole of the `{@}` rule: the indices take the front of the list,
        // `{@}` is whatever is behind them. `check_arity` has just proved the
        // split point is in range; `get` keeps that proof from being the only
        // thing between a future edit and a panic in a production path.
        let rest: &[SafeArg<'_>] = args.get(self.required_arity()..).unwrap_or(&[]);

        let mut rendered = Vec::with_capacity(self.words.len());
        for word in &self.words {
            match word {
                Word::Rest => rendered.extend(rest.iter().map(|arg| arg.as_str().to_owned())),
                Word::Parts(segments) => rendered.push(self.fill(segments, args, rest)?),
            }
        }
        Ok(rendered)
    }

    /// Builds one argv word from its segments.
    fn fill(
        &self,
        segments: &[Segment],
        args: &[SafeArg<'_>],
        rest: &[SafeArg<'_>],
    ) -> Result<String, TemplateError> {
        let mut word = String::new();
        for segment in segments {
            match segment {
                Segment::Literal(text) => word.push_str(text),
                Segment::Placeholder(index) => {
                    let arg = args
                        .get(*index)
                        // `ok_or_else`, not `ok_or`: this runs once per
                        // placeholder on the success path, and the eager form
                        // would build an error that is never used every time.
                        .ok_or_else(|| self.arity_error(args.len()))?;
                    word.push_str(arg.as_str());
                }
                // Inside a word the remaining arguments are joined with one
                // space and re-split by the remote shell; with none left this
                // adds nothing at all, which is the "empty string in its
                // place" of HLD §5.
                Segment::Rest => {
                    for (position, arg) in rest.iter().enumerate() {
                        if position > 0 {
                            word.push(' ');
                        }
                        word.push_str(arg.as_str());
                    }
                }
            }
        }
        Ok(word)
    }

    /// The arity refusal this template would raise for `given` arguments.
    fn arity_error(&self, given: usize) -> TemplateError {
        let needed = self.required_arity();
        if self.variadic {
            TemplateError::TooFew { needed, given }
        } else {
            TemplateError::Arity { needed, given }
        }
    }
}

/// Decides whether a word splices or merely contains `{@}`.
fn classify(segments: Vec<Segment>) -> Word {
    if matches!(segments.as_slice(), [Segment::Rest]) {
        Word::Rest
    } else {
        Word::Parts(segments)
    }
}

/// Whether every `{@}` in this word has whitespace (or nothing) on both sides.
///
/// The check runs on segments rather than on the raw text so that it sees the
/// word *after* escapes are resolved: `{{@}}{@}` is a literal `{@}` glued to a
/// real one, and the literal `}` is exactly the non-whitespace neighbour this
/// refuses.
fn rest_is_separated(segments: &[Segment]) -> bool {
    segments.iter().enumerate().all(|(at, segment)| {
        if !matches!(segment, Segment::Rest) {
            return true;
        }
        let before = at.checked_sub(1).and_then(|index| segments.get(index));
        let after = segments.get(at.saturating_add(1));
        boundary_is_blank(before, |text| text.chars().next_back())
            && boundary_is_blank(after, |text| text.chars().next())
    })
}

/// Whether the neighbour on one side of a `{@}` leaves it standing alone.
///
/// Nothing at all is fine (the word starts or ends there). A literal is fine
/// only if the character facing the `{@}` is whitespace. Another placeholder
/// never is: a `SafeArg` is non-empty and whitespace-free by construction, so
/// `{0}{@}` would always paste two values together.
fn boundary_is_blank(neighbour: Option<&Segment>, facing: impl Fn(&str) -> Option<char>) -> bool {
    match neighbour {
        None => true,
        Some(Segment::Literal(text)) => facing(text).is_none_or(char::is_whitespace),
        Some(Segment::Placeholder(_) | Segment::Rest) => false,
    }
}

/// Splits one word into literals and placeholders.
///
/// Never fails: anything that is not one of the recognised brace shapes is
/// literal text, which is what keeps `find -exec {} \;` and
/// `docker --format '{{.Names}}'` intact.
fn scan(word: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut rest = word;

    while let Some(at) = rest.find('{') {
        literal.push_str(&rest[..at]);
        let tail = &rest[at..];

        if let Some((body, after)) = strip_escaped(tail) {
            // `{{0}}` -> the literal text `{0}`, `{{@}}` -> `{@}`.
            literal.push('{');
            literal.push_str(body);
            literal.push('}');
            rest = after;
        } else if let Some((segment, after)) = strip_placeholder(tail) {
            if !literal.is_empty() {
                segments.push(Segment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(segment);
            rest = after;
        } else {
            // A brace that starts nothing: data. Copy it and carry on from the
            // next byte, so `{a{0}` still finds the `{0}` inside it.
            literal.push('{');
            rest = &tail[1..];
        }
    }

    literal.push_str(rest);
    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    segments
}

/// Matches `{N}` or `{@}` at the start of `tail`, returning it and the rest.
fn strip_placeholder(tail: &str) -> Option<(Segment, &str)> {
    let body = tail.strip_prefix('{')?;
    if let Some(after) = body.strip_prefix("@}") {
        return Some((Segment::Rest, after));
    }
    let (digits, after) = split_digits(body);
    let after = after.strip_prefix('}')?;
    Some((Segment::Placeholder(index_of(digits)?), after))
}

/// Matches `{{N}}` or `{{@}}` at the start of `tail`, returning the text
/// between the braces and the rest.
fn strip_escaped(tail: &str) -> Option<(&str, &str)> {
    let body = tail.strip_prefix("{{")?;
    if let Some(after) = body.strip_prefix("@}}") {
        return Some(("@", after));
    }
    let (digits, after) = split_digits(body);
    // The same index spelling as a real placeholder, so `{{01}}` is as literal
    // as `{01}` is: one rule, not two.
    index_of(digits)?;
    let after = after.strip_prefix("}}")?;
    Some((digits, after))
}

/// Splits off the leading run of ASCII digits.
fn split_digits(text: &str) -> (&str, &str) {
    let len = text.bytes().take_while(u8::is_ascii_digit).count();
    text.split_at(len)
}

/// The index `digits` spells, if it spells one canonically.
///
/// `None` for the empty string (a bare `{}`, which HLD §5 keeps literal), for a
/// leading zero (`{01}` and `{1}` must not be two names for one index, and
/// `{00}` must not be a second spelling of `{0}`), and for a run of digits too
/// large for a `usize` — that last one cannot be a sane argument index, and
/// leaving it literal keeps the "unrecognised shapes are data" rule whole.
fn index_of(digits: &str) -> Option<usize> {
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a template and renders it with the given arguments, skipping the
    /// whitelist — the tests below are about the *shape* of the argv.
    fn render(template: &str, args: &[&str]) -> Result<Vec<String>, TemplateError> {
        let safe: Vec<SafeArg<'_>> = args.iter().copied().map(SafeArg::unchecked).collect();
        parse(template)?.render(&safe)
    }

    fn words(template: &str) -> Vec<String> {
        render(template, &[]).expect("template must render with no arguments")
    }

    // =============================================================== the split

    #[test]
    fn a_template_splits_the_way_a_shell_would() {
        assert_eq!(words("ssh -tt host"), ["ssh", "-tt", "host"]);
        assert_eq!(words("  ssh   -tt   host  "), ["ssh", "-tt", "host"]);
    }

    /// The whole point of quoting in the template: the remote shell gets one
    /// argument, not four.
    #[test]
    fn a_quoted_section_stays_one_word() {
        assert_eq!(
            words("ssh host 'docker logs -f app'"),
            ["ssh", "host", "docker logs -f app"]
        );
        assert_eq!(
            words("ssh host \"docker logs -f app\""),
            ["ssh", "host", "docker logs -f app"]
        );
    }

    /// HLD §5: `2>&1` inside the quotes is for the remote shell, and outside
    /// them it is a literal argv entry. Both are the caller's business; hog
    /// passes each through unchanged.
    #[test]
    fn redirection_outside_quotes_is_a_literal_word() {
        assert_eq!(
            words("ssh host 'docker logs app 2>&1'"),
            ["ssh", "host", "docker logs app 2>&1"]
        );
        assert_eq!(words("ssh host 2>&1"), ["ssh", "host", "2>&1"]);
    }

    #[test]
    fn an_unclosed_quote_is_a_named_error() {
        assert_eq!(
            parse("ssh host 'docker logs"),
            Err(TemplateError::Unbalanced)
        );
        assert_eq!(parse("ssh host \\"), Err(TemplateError::Unbalanced));
    }

    #[test]
    fn an_empty_template_is_an_error() {
        assert_eq!(parse(""), Err(TemplateError::Empty));
        assert_eq!(parse("   \t "), Err(TemplateError::Empty));
        // `#` starting a word opens a comment, exactly as in a shell, so a
        // template that is nothing but a comment has no words at all.
        assert_eq!(parse("# ssh {0}"), Err(TemplateError::Empty));
    }

    // ========================================================== brace literals

    /// HLD §5, by name: this command must pass through untouched.
    #[test]
    fn find_exec_passes_through_untouched() {
        assert_eq!(
            words("find /var/log -name '*.log' -exec cat {} \\;"),
            [
                "find", "/var/log", "-name", "*.log", "-exec", "cat", "{}", ";"
            ]
        );
    }

    /// HLD §5, by name: a Go template in `--format` must survive. A blanket
    /// `{{` -> `{` rule would turn this into `{.Names}` and break docker.
    #[test]
    fn docker_format_passes_through_untouched() {
        assert_eq!(
            words("docker ps --format '{{.Names}}\t{{.Status}}'"),
            ["docker", "ps", "--format", "{{.Names}}\t{{.Status}}"]
        );
        assert_eq!(
            words("docker ps --format '{{range .}}{{.ID}}{{end}}'"),
            ["docker", "ps", "--format", "{{range .}}{{.ID}}{{end}}"]
        );
    }

    #[test]
    fn unrecognised_brace_shapes_are_literal_text() {
        // (template after `x `, the single word it must produce)
        for (tail, expected) in [
            ("{}", "{}"),
            ("{abc}", "{abc}"),
            ("'{ 0 }'", "{ 0 }"),
            ("{0", "{0"),
            ("0}", "0}"),
            ("{}{}", "{}{}"),
            ("}{", "}{"),
            ("{{}}", "{{}}"),
            ("{-1}", "{-1}"),
            ("{0.0}", "{0.0}"),
            ("{0x1}", "{0x1}"),
            ("'{${N}}'", "{${N}}"),
            // `@` is a placeholder only in exactly `{@}`.
            ("'{ @ }'", "{ @ }"),
            ("{@@}", "{@@}"),
            ("{@1}", "{@1}"),
            ("{1@}", "{1@}"),
            ("{@", "{@"),
            ("@}", "@}"),
        ] {
            let template = format!("x {tail}");
            assert_eq!(words(&template), ["x", expected], "{template:?}");
            let parsed = parse(&template).expect("parses");
            assert_eq!(
                parsed.required_arity(),
                0,
                "{template:?} must use no placeholder"
            );
            assert!(!parsed.is_variadic(), "{template:?} must not be variadic");
        }
    }

    /// `{01}` is not index 1. One index must have exactly one spelling, or the
    /// arity check would be arguing with itself about how many it needs.
    #[test]
    fn a_leading_zero_is_not_an_index() {
        assert_eq!(words("x {01}"), ["x", "{01}"]);
        assert_eq!(words("x {00}"), ["x", "{00}"]);
        assert_eq!(parse("x {01}").expect("parses").required_arity(), 0);
    }

    /// An index that cannot fit in a `usize` is not an index either — it stays
    /// literal instead of overflowing or erroring.
    #[test]
    fn an_absurd_index_stays_literal() {
        let huge = "9".repeat(40);
        assert_eq!(
            words(&format!("x {{{huge}}}")),
            ["x", &format!("{{{huge}}}")]
        );
    }

    /// The escape hatch of HLD §5, and the one case where braces *are*
    /// rewritten: `{{0}}` is how a literal `{0}` is written, `{{@}}` a literal
    /// `{@}`.
    #[test]
    fn double_braces_escape_to_a_literal_placeholder() {
        assert_eq!(words("x {{0}}"), ["x", "{0}"]);
        assert_eq!(words("x {{12}}"), ["x", "{12}"]);
        assert_eq!(words("x {{@}}"), ["x", "{@}"]);
        assert_eq!(parse("x {{0}}").expect("parses").required_arity(), 0);
        // Not an index, so not an escape: still literal, still untouched.
        assert_eq!(words("x {{01}}"), ["x", "{{01}}"]);
    }

    /// An escaped `{@}` is text, so it neither makes the template variadic nor
    /// counts towards the one-`{@}` rule.
    #[test]
    fn an_escaped_rest_is_not_a_placeholder() {
        let parsed = parse("echo {{@}} {{@}} {{@}}").expect("parses");
        assert!(!parsed.is_variadic());
        assert_eq!(parsed.required_arity(), 0);
        assert_eq!(
            words("echo {{@}} {{@}} {{@}}"),
            ["echo", "{@}", "{@}", "{@}"]
        );
    }

    /// The escape and the real thing can share a template: the literal does not
    /// consume the one `{@}` that is allowed.
    #[test]
    fn an_escaped_rest_and_a_real_one_can_share_a_template() {
        assert_eq!(
            render("echo {{@}} is {@}", &["a", "b"]),
            Ok(vec![
                "echo".into(),
                "{@}".into(),
                "is".into(),
                "a".into(),
                "b".into()
            ])
        );
    }

    /// A brace that starts nothing is data, and the scan resumes at the next
    /// byte — so a `{N}` hiding inside a malformed run is still found. Half an
    /// escape is not an escape.
    #[test]
    fn an_unmatched_brace_is_data_and_the_scan_carries_on_past_it() {
        assert_eq!(render("x {{0}", &["v"]), Ok(vec!["x".into(), "{v".into()]));
        assert_eq!(render("x {0}}", &["v"]), Ok(vec!["x".into(), "v}".into()]));
        assert_eq!(
            render("x {a{0}", &["v"]),
            Ok(vec!["x".into(), "{av".into()])
        );
        assert_eq!(parse("x {{0}").expect("parses").required_arity(), 1);
    }

    /// The same half-escape around `{@}` leaves a literal `{` welded to a real
    /// `{@}`, and gluing is exactly what is refused. Worth pinning: it is the
    /// one place where the `{N}` and `{@}` spellings part company.
    #[test]
    fn a_half_escaped_rest_is_a_glued_rest() {
        assert_eq!(
            parse("x {{@}"),
            Err(TemplateError::RestGlued {
                word: "{{@}".to_owned()
            })
        );
        // `{@}}` is the mirror image: a real `{@}` with a literal `}` after it.
        assert_eq!(
            parse("x {@}}"),
            Err(TemplateError::RestGlued {
                word: "{@}}".to_owned()
            })
        );
    }

    #[test]
    fn an_escape_and_a_placeholder_can_share_a_word() {
        assert_eq!(
            render("x {{0}}-{0}", &["v"]),
            Ok(vec!["x".into(), "{0}-v".into()])
        );
    }

    // ============================================================ substitution

    #[test]
    fn placeholders_are_positional() {
        assert_eq!(
            render("ssh {0} 'docker logs -f myapp-{1}-1'", &["prod", "api"]),
            Ok(vec![
                "ssh".into(),
                "prod".into(),
                "docker logs -f myapp-api-1".into()
            ])
        );
    }

    /// Substitution happens inside a word, so a `{0}` that sat inside the
    /// template's quotes stays inside that same argv entry.
    #[test]
    fn a_placeholder_inside_quotes_lands_in_the_same_word() {
        let argv = render("ssh host 'systemctl status {0}.service'", &["api"]).expect("renders");
        assert_eq!(argv.len(), 3, "the quoted section is still one word");
        assert_eq!(argv[2], "systemctl status api.service");
    }

    /// The reason substitution runs after the split and not before it. Under
    /// the whitelist this value can never reach `render` in production, which
    /// is precisely why the property needs a test of its own: it is the
    /// second line of defence, and it holds on its own.
    #[test]
    fn an_argument_containing_a_space_does_not_split_the_word() {
        let argv = render("echo {0} end", &["a b c"]).expect("renders");
        assert_eq!(argv, ["echo", "a b c", "end"]);
        assert_eq!(argv.len(), 3, "one argument stayed one word");
    }

    /// Same property for the other characters that would matter if they ever
    /// got this far: they are inert data, not syntax.
    #[test]
    fn a_hostile_argument_is_inert_once_it_reaches_a_word() {
        let argv = render("ssh {0} 'logs {1}'", &["h'x", "a; rm -rf /"]).expect("renders");
        assert_eq!(argv, ["ssh", "h'x", "logs a; rm -rf /"]);
    }

    /// `{@}` splices by count, never by content: even a value with a space in
    /// it stays one argv entry, because the number of entries comes from how
    /// many arguments were typed.
    #[test]
    fn a_spliced_argument_containing_a_space_is_still_one_word() {
        let argv = render("echo {@}", &["a b", "c"]).expect("renders");
        assert_eq!(argv, ["echo", "a b", "c"]);
    }

    #[test]
    fn one_index_may_be_used_more_than_once() {
        let template = parse("ssh {0} 'systemctl status {0}'").expect("parses");
        assert_eq!(template.required_arity(), 1);
        assert_eq!(
            render("ssh {0} 'systemctl status {0}'", &["api"]),
            Ok(vec![
                "ssh".into(),
                "api".into(),
                "systemctl status api".into()
            ])
        );
    }

    #[test]
    fn a_placeholder_may_be_the_program_word() {
        assert_eq!(
            render("{0} --version", &["kubectl"]),
            Ok(vec!["kubectl".into(), "--version".into()])
        );
    }

    #[test]
    fn indices_may_appear_out_of_order() {
        assert_eq!(
            render("run {1} {0}", &["a", "b"]),
            Ok(vec!["run".into(), "b".into(), "a".into()])
        );
    }

    #[test]
    fn a_word_that_is_only_a_placeholder_becomes_exactly_the_argument() {
        assert_eq!(
            render("echo {0}", &["-n"]),
            Ok(vec!["echo".into(), "-n".into()])
        );
    }

    // ============================================== {@} as a word of its own

    /// The shell's `"$@"`: one argv entry per argument, and the word vanishes
    /// when there are none.
    #[test]
    fn a_standalone_rest_splices_into_separate_words() {
        assert_eq!(words("echo {@}"), ["echo"]);
        assert_eq!(
            render("echo {@}", &["a"]),
            Ok(vec!["echo".into(), "a".into()])
        );
        assert_eq!(
            render("echo {@}", &["a", "b", "c"]),
            Ok(vec!["echo".into(), "a".into(), "b".into(), "c".into()])
        );
    }

    /// The word disappearing is what keeps `kubectl logs {@} -f` from growing
    /// an empty argv entry between two real ones.
    #[test]
    fn a_standalone_rest_with_nothing_left_leaves_no_empty_word() {
        let argv = render("kubectl logs {@} -f", &[]).expect("renders");
        assert_eq!(argv, ["kubectl", "logs", "-f"]);
        assert!(!argv.iter().any(String::is_empty), "{argv:?}");
    }

    /// The indices take the front of the list, `{@}` takes the back — the one
    /// rule of HLD §5, at a word boundary.
    #[test]
    fn a_standalone_rest_starts_after_the_highest_index() {
        assert_eq!(
            render("ssh {0} logs {@}", &["prod", "api", "--tail", "50"]),
            Ok(vec![
                "ssh".into(),
                "prod".into(),
                "logs".into(),
                "api".into(),
                "--tail".into(),
                "50".into()
            ])
        );
    }

    /// A quoted `{@}` is still one shlex word containing nothing else, so it
    /// splices like the bare form. There is no way to tell the two apart by the
    /// time `shlex` is done, and inventing one would be a lie.
    #[test]
    fn a_quoted_standalone_rest_splices_like_the_bare_one() {
        assert_eq!(
            render("echo '{@}'", &["a", "b"]),
            Ok(vec!["echo".into(), "a".into(), "b".into()])
        );
    }

    /// `{@}` may even be the program word. With nothing left over there is no
    /// argv at all, which the caller reports as an empty template rather than
    /// inventing a program name.
    #[test]
    fn a_rest_may_be_the_whole_template() {
        assert_eq!(
            render("{@}", &["ls", "-la"]),
            Ok(vec!["ls".into(), "-la".into()])
        );
        assert_eq!(render("{@}", &[]), Ok(Vec::new()));
    }

    // ================================================== {@} inside a word

    /// The flagship template of HLD §5. `'docker logs -f {@}'` is ONE shlex
    /// word, so `{@}` cannot be a separate argv entry here — the remaining
    /// arguments are joined with one space and re-split by the remote shell.
    /// N = 0, 1 and 3 left over.
    #[test]
    fn the_flagship_template_joins_the_remainder_inside_its_quotes() {
        const TEMPLATE: &str = "ssh -tt {0} 'docker logs -f {@}'";

        for (args, remote) in [
            (&["prod"][..], "docker logs -f "),
            (&["prod", "api"][..], "docker logs -f api"),
            (
                &["prod", "api", "--tail", "50"][..],
                "docker logs -f api --tail 50",
            ),
        ] {
            let argv = render(TEMPLATE, args).expect("renders");
            assert_eq!(
                argv,
                ["ssh", "-tt", "prod", remote],
                "the quoted section must stay one word: {args:?}"
            );
        }
    }

    /// With nothing left over the `{@}` contributes the empty string, so the
    /// word keeps the space that was in front of it. That is deliberate: the
    /// remote shell collapses trailing whitespace, and trimming here would mean
    /// hog editing a string it does not parse.
    #[test]
    fn an_empty_remainder_inside_a_word_is_the_empty_string() {
        let argv = render("ssh {0} 'docker logs -f {@}'", &["prod"]).expect("renders");
        assert_eq!(argv[2], "docker logs -f ");
        assert_eq!(argv.len(), 3, "the word is still there, merely shorter");
    }

    /// The join is exactly one space, whatever the arguments are and however
    /// many of them there are.
    #[test]
    fn the_join_inside_a_word_is_one_space() {
        assert_eq!(
            render("sh -c 'run {@} ; echo done'", &["a", "b", "c"]),
            Ok(vec![
                "sh".into(),
                "-c".into(),
                "run a b c ; echo done".into()
            ])
        );
    }

    /// `{@}` at the very end of a quoted word, with nothing after it: the
    /// word-boundary side of the separation rule.
    #[test]
    fn a_rest_at_either_end_of_a_word_is_separated_enough() {
        assert_eq!(
            render("sh -c '{@} | head'", &["ls"]),
            Ok(vec!["sh".into(), "-c".into(), "ls | head".into()])
        );
        assert_eq!(
            render("sh -c 'head {@}'", &["-n5"]),
            Ok(vec!["sh".into(), "-c".into(), "head -n5".into()])
        );
    }

    /// A tab counts as whitespace too — the check asks `char::is_whitespace`,
    /// not "is it a space".
    #[test]
    fn a_tab_beside_a_rest_is_whitespace() {
        assert_eq!(
            render("sh -c 'run\t{@}\tdone'", &["x"]),
            Ok(vec!["sh".into(), "-c".into(), "run\tx\tdone".into()])
        );
    }

    // ========================================================= {@} refusals

    /// HLD §5 by name: `myapp-{@}-1` has no meaning that would not surprise
    /// someone, so it is refused at parse time rather than given one.
    #[test]
    fn a_rest_glued_to_a_neighbour_is_refused() {
        for template in [
            "ssh {0} myapp-{@}-1",
            "ssh {0} --tail={@}",
            "ssh {0} 'docker logs -f myapp-{@}-1'",
            "ssh {0} 'docker logs --tail={@}'",
            "ssh {0}{@}",
            "ssh {@}{0}",
            "ssh x{@}",
            "ssh {@}x",
            "ssh '{@}.log'",
        ] {
            assert!(
                matches!(parse(template), Err(TemplateError::RestGlued { .. })),
                "{template:?} must be refused: {:?}",
                parse(template)
            );
        }
    }

    #[test]
    fn the_glued_refusal_names_the_word_and_says_what_is_wrong() {
        let err = parse("ssh {0} 'docker logs myapp-{@}-1'").expect_err("must fail");
        assert_eq!(
            err,
            TemplateError::RestGlued {
                word: "docker logs myapp-{@}-1".to_owned()
            }
        );
        let message = err.to_string();
        assert!(
            message.contains("docker logs myapp-{@}-1"),
            "the word must be in the message: {message}"
        );
        assert!(message.contains("whitespace around it"), "{message}");
    }

    /// A second `{@}` could only repeat the first, so it is a mistake by
    /// construction (HLD §5).
    #[test]
    fn a_second_rest_is_refused() {
        for template in [
            "echo {@} {@}",
            "ssh {0} 'logs {@}' 'more {@}'",
            "sh -c 'a {@} b {@}'",
            "echo {@}{@}",
        ] {
            assert_eq!(
                parse(template),
                Err(TemplateError::DuplicateRest),
                "{template:?}"
            );
        }
        assert_eq!(
            TemplateError::DuplicateRest.to_string(),
            "command template uses {@} more than once"
        );
    }

    /// Two `{@}` glued together are reported as the duplicate they are: the
    /// count is taken across the whole template before any word is judged on
    /// its own, because "you wrote it twice" is the more useful of the two
    /// true statements.
    #[test]
    fn a_duplicate_outranks_a_gluing_complaint() {
        assert_eq!(parse("echo a{@}{@}"), Err(TemplateError::DuplicateRest));
    }

    // ==================================================================== arity

    #[test]
    fn arity_counts_the_highest_index_not_the_number_of_placeholders() {
        let template = parse("ssh {0} 'logs {1} {1}'").expect("parses");
        assert_eq!(template.max_index(), Some(1));
        assert_eq!(template.required_arity(), 2);
        assert_eq!(template.used_indices(), [0, 1]);
        assert!(!template.is_variadic());
    }

    #[test]
    fn a_template_without_placeholders_needs_no_arguments() {
        let template = parse("kubectl logs -f -l app=api").expect("parses");
        assert_eq!(template.max_index(), None);
        assert_eq!(template.required_arity(), 0);
        assert!(template.check_arity(0).is_ok());
        assert_eq!(
            template.check_arity(1),
            Err(TemplateError::Arity {
                needed: 0,
                given: 1
            })
        );
    }

    /// HLD §6 spells this message out; it is the one a bare `hog` on a
    /// terminal produces when the template has placeholders.
    #[test]
    fn too_few_arguments_is_an_error_with_both_numbers() {
        let err = render("ssh {0} 'logs {1}'", &["prod"]).expect_err("must fail");
        assert_eq!(
            err,
            TemplateError::Arity {
                needed: 2,
                given: 1
            }
        );
        assert_eq!(err.to_string(), "template needs 2 arguments, got 1");

        let none = render("ssh {0} 'logs {1}'", &[]).expect_err("must fail");
        assert_eq!(none.to_string(), "template needs 2 arguments, got 0");
    }

    /// A surplus argument is a typo, not something to ignore (HLD §5).
    #[test]
    fn too_many_arguments_is_an_error_too() {
        let err = render("ssh {0}", &["prod", "api"]).expect_err("must fail");
        assert_eq!(
            err,
            TemplateError::Arity {
                needed: 1,
                given: 2
            }
        );
        assert_eq!(err.to_string(), "template needs 1 argument, got 2");
    }

    #[test]
    fn the_argument_word_is_singular_for_one() {
        assert_eq!(
            TemplateError::Arity {
                needed: 1,
                given: 0
            }
            .to_string(),
            "template needs 1 argument, got 0"
        );
        assert_eq!(
            TemplateError::Arity {
                needed: 0,
                given: 3
            }
            .to_string(),
            "template needs 0 arguments, got 3"
        );
        assert_eq!(
            TemplateError::TooFew {
                needed: 1,
                given: 0
            }
            .to_string(),
            "template needs at least 1 argument, got 0"
        );
    }

    /// The second row of the arity table of HLD §5: with `{@}` and no index,
    /// any count at all is fine, including zero.
    #[test]
    fn a_rest_alone_accepts_any_number_of_arguments() {
        let template = parse("echo {@}").expect("parses");
        assert!(template.is_variadic());
        assert_eq!(template.required_arity(), 0);
        assert_eq!(template.max_index(), None);
        for given in [0, 1, 2, 7, 1000] {
            assert!(template.check_arity(given).is_ok(), "{given}");
        }
    }

    /// The third row: `{0}` sets a floor of one, and nothing sets a ceiling.
    #[test]
    fn an_index_with_a_rest_has_a_floor_and_no_ceiling() {
        let template = parse("ssh -tt {0} 'docker logs -f {@}'").expect("parses");
        assert!(template.is_variadic());
        assert_eq!(template.required_arity(), 1);
        assert_eq!(
            template.check_arity(0),
            Err(TemplateError::TooFew {
                needed: 1,
                given: 0
            })
        );
        for given in [1, 2, 5, 50] {
            assert!(template.check_arity(given).is_ok(), "{given}");
        }
    }

    /// Two indices and a `{@}`: the floor is two, and the tail starts at the
    /// third argument.
    #[test]
    fn the_floor_is_one_above_the_highest_index() {
        let template = parse("run {0} {1} {@}").expect("parses");
        assert_eq!(template.required_arity(), 2);
        assert_eq!(
            template.check_arity(1),
            Err(TemplateError::TooFew {
                needed: 2,
                given: 1
            })
        );
        assert_eq!(
            render("run {0} {1} {@}", &["a", "b", "c", "d"]),
            Ok(vec![
                "run".into(),
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into()
            ])
        );
    }

    /// The message a variadic template gives when the floor is not met — the
    /// "at least" is the whole difference, and it is what stops the user from
    /// reading "needs 1" as "takes exactly 1".
    #[test]
    fn the_variadic_shortfall_says_at_least() {
        let err = render("ssh {0} 'logs {@}'", &[]).expect_err("must fail");
        assert_eq!(err.to_string(), "template needs at least 1 argument, got 0");
    }

    /// An index used *after* the tail's starting point does not move it: the
    /// boundary is the highest index, not the last one written.
    #[test]
    fn the_boundary_follows_the_highest_index_not_the_writing_order() {
        assert_eq!(
            render("run {1} {0} {@}", &["a", "b", "c"]),
            Ok(vec!["run".into(), "b".into(), "a".into(), "c".into()])
        );
    }

    /// `{0}` may be repeated and still only consume one argument, so the tail
    /// starts at the second.
    #[test]
    fn a_repeated_index_does_not_widen_the_head() {
        assert_eq!(
            render("ssh {0} 'on {0}: logs {@}'", &["prod", "api", "db"]),
            Ok(vec![
                "ssh".into(),
                "prod".into(),
                "on prod: logs api db".into()
            ])
        );
    }

    /// `{0}` and `{2}` with no `{1}`: the second argument could only ever be
    /// typed and discarded, so the template is wrong.
    #[test]
    fn a_gap_in_the_indices_is_rejected_at_parse_time() {
        assert_eq!(
            parse("ssh {0} 'logs {2}'"),
            Err(TemplateError::SkippedIndex { missing: 1, max: 2 })
        );
        assert_eq!(
            TemplateError::SkippedIndex { missing: 1, max: 2 }.to_string(),
            "command template uses {2} but never {1}"
        );
    }

    #[test]
    fn a_template_that_starts_at_one_is_a_gap_as_well() {
        assert_eq!(
            parse("ssh 'logs {1}'"),
            Err(TemplateError::SkippedIndex { missing: 0, max: 1 })
        );
    }

    /// `{@}` does not fill a gap. It takes what is *left*, so it can never be
    /// the reason an index in the middle went unused.
    #[test]
    fn a_rest_does_not_close_a_gap_in_the_indices() {
        assert_eq!(
            parse("ssh {0} {2} {@}"),
            Err(TemplateError::SkippedIndex { missing: 1, max: 2 })
        );
    }

    // ================================================================== shapes

    #[test]
    fn word_count_is_the_template_shape_not_the_rendered_length() {
        let template = parse("ssh -tt {0} logs {@}").expect("parses");
        assert_eq!(template.word_count(), 5);

        let safe: Vec<SafeArg<'_>> = ["prod", "a", "b"].map(SafeArg::unchecked).into();
        assert_eq!(template.render(&safe).expect("renders").len(), 6);
    }

    #[test]
    fn the_starter_config_template_parses_and_needs_two_arguments() {
        let template =
            parse("ssh -tt -o ServerAliveInterval=15 {0} 'docker logs -f --since 1h myapp-{1}-1'")
                .expect("the shipped template must parse");
        assert_eq!(template.required_arity(), 2);
        assert_eq!(template.word_count(), 6);
        assert!(!template.is_variadic());
        assert_eq!(
            render(
                "ssh -tt -o ServerAliveInterval=15 {0} 'docker logs -f --since 1h myapp-{1}-1'",
                &["prod", "api"]
            ),
            Ok(vec![
                "ssh".into(),
                "-tt".into(),
                "-o".into(),
                "ServerAliveInterval=15".into(),
                "prod".into(),
                "docker logs -f --since 1h myapp-api-1".into(),
            ])
        );
    }
}
