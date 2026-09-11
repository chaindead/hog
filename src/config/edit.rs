//! `DocumentMut` surgery for `hog config -e` / `-d`.
//!
//! The hard requirement is that editing the file does not cost the user their
//! comments, their blank lines or their formatting — the starter config *is*
//! the documentation of the format (HLD §3), and a round-trip through
//! `toml::to_string` would delete all of it. Hence `toml_edit` on both sides:
//! the same crate parses and writes, and everything it did not touch comes back
//! byte for byte.
//!
//! # Array style
//!
//! "Preserving the style" is spelled out here once, because it is the part a
//! naive `array.push(value)` gets wrong — it appends with no decor, so a
//! multi-line array grows a stray `"new"` glued to the previous entry's comma:
//!
//! | before | after `append_exclude(doc, "c")` |
//! |---|---|
//! | `exclude = []` | `exclude = ["c"]` |
//! | `exclude = ["a", "b"]` | `exclude = ["a", "b", "c"]` |
//! | `exclude = [\n  "a",\n]` | `exclude = [\n  "a",\n  "c",\n]` |
//!
//! The rule behind all three rows: the new entry copies the **style of the
//! entry it follows** — that entry's leading whitespace, which is what carries
//! the newline and the indent — and it takes over the padding that used to sit
//! before the `]`, so `[ "a" ]` grows into `[ "a", "c" ]` and not into
//! `[ "a" , "c"]`. Two special cases fall out of that:
//!
//! * an array of exactly one entry has no separator to copy, because that
//!   entry's prefix is the array's opening padding — `["a"]` therefore grows
//!   the default `, ` rather than a bare `,`;
//! * an empty array has nothing at all to copy from and falls back to the
//!   single-line form, which is what the starter file's `exclude = []` turns
//!   into after one `hog config -e`.
//!
//! The trailing comma is a property of the array rather than of any entry, so
//! it survives a push untouched: `["a",]` becomes `["a", "c",]`.
//!
//! # Comments inside the array
//!
//! Copying the neighbouring decor *verbatim* is not good enough, because that
//! decor can contain somebody's comment:
//!
//! ```toml
//! exclude = [
//!   "a",     # the loud one
//!   "b",
//! ]
//! ```
//!
//! `# the loud one` is not attached to `"a"` — in the parse tree it sits in the
//! **prefix of `"b"`**, together with the newline and the indent. Copying that
//! prefix onto a new entry would print the comment a second time. So only the
//! indent is copied: everything after the last newline of the prefix, which is
//! whitespace by construction (a `#` on that line would have swallowed the
//! value itself).
//!
//! The same fact decides where the comma goes. In an array with no trailing
//! comma the text before the `]` belongs to the last value, comment and all,
//! and the comma an append adds is printed *after* it — so `[\n  "a"  # x\n]`
//! has to become `[\n  "a",  # x\n  "c"\n]`. Leaving the comment where it was
//! would put the comma inside it and produce a file that no longer parses.
//!
//! The mirror image governs removal. An entry's prefix holds the comment
//! written *before* it, so dropping the entry would drop a comment that was
//! never about it. [`remove_exclude`] therefore hands any comment in a removed
//! entry's decor to the entry that follows it, or to the array's trailing decor
//! when nothing follows, and hands back the closing padding the same way.
//! Nothing a user typed is deleted by an edit that was only asked to delete a
//! field name.

use anyhow::{anyhow, bail};
use toml_edit::{Array, DocumentMut, Item, RawString, Value};

/// The commented starter config, compiled into the binary.
///
/// This is what hog writes the first time it finds no config file
/// ([`ensure_default`](super::ensure_default)), and it doubles as the reference
/// documentation of the format: every key hog understands appears in it, with
/// the reasoning beside it. Keep it in sync with [`model`](super::model) — a
/// key documented here but missing there would warn about itself on every fresh
/// install.
///
/// Every value in it is the built-in default, and `command` is commented out.
/// That is load-bearing rather than tidy: hog creates this file unasked, so
/// creating it must change nothing about how the next run behaves.
pub const STARTER: &str = include_str!("starter.toml");

/// The top-level key holding the persistent exclude list.
pub const EXCLUDE_KEY: &str = "exclude";

/// Indent given to a new entry in a multi-line array that does not say what its
/// indent is — an empty `[\n]`, or one whose entries all sit on the first line.
///
/// Two spaces, matching the commented example in [`STARTER`].
const DEFAULT_INDENT: &str = "  ";

/// What goes between two entries of a single-line array that has not already
/// shown a preference — the `, ` of `["a", "b"]`.
const DEFAULT_SEPARATOR: &str = " ";

/// What an edit actually did.
///
/// Returned instead of `bool` because both callers report it to the user, and
/// "already there" is a different message from "added" — `hog config -e trace_id`
/// twice should say so rather than rewriting the file for nothing. It is also
/// the signal that lets `hog config` skip the write entirely when nothing
/// changed, which keeps the file's mtime honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The document was modified and has to be written back.
    Applied,
    /// The document already said what was asked; there is nothing to write.
    Unchanged,
}

impl Change {
    /// Does this change need [`write::save`](super::write::save)?
    pub fn is_applied(self) -> bool {
        matches!(self, Self::Applied)
    }
}

/// Parses [`STARTER`] into an editable document.
///
/// This is where `hog config -e field` starts when there is no config file yet:
/// the user gets the fully commented starter with their field already in it,
/// rather than a one-line file that explains nothing.
///
/// The error is unreachable in a shipped binary — the text is a compile-time
/// constant — but it is still returned rather than unwrapped, because a
/// `expect()` here would be a panic in a production path and the test suite
/// covers the same ground without one.
pub fn starter_document() -> Result<DocumentMut, toml_edit::TomlError> {
    STARTER.parse()
}

/// The exclude list as the document currently spells it.
///
/// Used by `hog config` to print the effective list and to decide whether an
/// append would be a no-op. Entries are returned in file order, untrimmed and
/// undeduplicated — this reports what is written, and normalising it is
/// `ExcludeSet::new`'s job at resolve time.
///
/// An `exclude` that is present but not an array is an error, not an empty
/// list: the user wrote something that hog cannot honour, and pretending the
/// list is empty would hide it until the next render. An array holding a
/// non-string entry fails the same way and for the same reason — the loader
/// deserializes this key as `Vec<String>`, so reporting `[1]` as an empty list
/// here would disagree with the error the very next run prints.
pub fn exclude_list(document: &DocumentMut) -> anyhow::Result<Vec<String>> {
    let Some(item) = document.get(EXCLUDE_KEY) else {
        return Ok(Vec::new());
    };
    let array = item
        .as_array()
        .ok_or_else(|| non_array_error(item.type_name()))?;
    array
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                anyhow!(
                    "`{EXCLUDE_KEY}` entry {} is a {}, expected a dotted field path in quotes",
                    index + 1,
                    value.type_name()
                )
            })
        })
        .collect()
}

/// Adds `field` to the top-level `exclude` array, preserving the array's style.
///
/// Creates the key when it is missing: a bare key inserted into the root table
/// is rendered ahead of every `[table]` header, which is also the only place a
/// top-level `exclude` can legally go.
///
/// `field` is trimmed before it is compared or inserted, and an empty field is
/// rejected — `hog config -e ""` is a mistake, and an empty entry in the list
/// would match no path while looking like it matched every one.
///
/// Returns [`Change::Unchanged`] when the field is already listed.
pub fn append_exclude(document: &mut DocumentMut, field: &str) -> anyhow::Result<Change> {
    let field = checked_field(field)?;

    if exclude_array_mut(document)?.is_none() {
        document.insert(EXCLUDE_KEY, Item::Value(Value::Array(Array::new())));
    }
    let array = exclude_array_mut(document)?
        .ok_or_else(|| anyhow!("`{EXCLUDE_KEY}` disappeared right after it was created"))?;

    if array
        .iter()
        .filter_map(Value::as_str)
        .any(|entry| entry.trim() == field)
    {
        return Ok(Change::Unchanged);
    }

    let decor = append_decor(array);
    if let Some(last) = array
        .len()
        .checked_sub(1)
        .and_then(|index| array.get_mut(index))
    {
        // The closing padding was the old last entry's only because it was
        // last; it now belongs to the new one, or `[ "a" ]` would grow into
        // `[ "a" , "c"]`. Nothing is lost: `append_decor` has already put
        // whatever that suffix said into the new entry's decor.
        last.decor_mut().set_suffix("");
    }
    array.push_formatted(Value::from(field).decorated(decor.prefix, decor.suffix));
    if let Some(trailing) = decor.trailing {
        array.set_trailing(trailing);
    }
    Ok(Change::Applied)
}

/// Removes `field` from the top-level `exclude` array.
///
/// Removes every occurrence, not just the first: a list that somehow grew a
/// duplicate should come back clean from one `hog config -d`. Removing the last
/// entry leaves `exclude = []` rather than deleting the key, so the commented
/// explanation above it keeps something to explain.
///
/// The surviving entries keep their own decor untouched. Three things move
/// rather than staying with the entry that carried them:
///
/// * a comment in the removed entry's decor, which goes to whatever comes after
///   it — the module docs explain why it was never that entry's own comment;
/// * the opening padding of a single-line array, when the first entry is the
///   one removed, so `["a", "b"]` loses `"a"` as `["b"]` and not as `[ "b"]`;
/// * the padding before the `]`, when the last entry is the one removed, so
///   `[ "a", "b" ]` loses `"b"` as `[ "a" ]` and `[\n  "a",\n  "b"\n]` keeps
///   the line its bracket was on.
///
/// Returns [`Change::Unchanged`] when the field was not listed, which `hog
/// config` reports rather than treating as success.
pub fn remove_exclude(document: &mut DocumentMut, field: &str) -> anyhow::Result<Change> {
    let field = checked_field(field)?;
    let Some(array) = exclude_array_mut(document)? else {
        return Ok(Change::Unchanged);
    };

    let mut change = Change::Unchanged;
    let mut index = 0;
    while index < array.len() {
        let matched = array
            .get(index)
            .and_then(Value::as_str)
            .is_some_and(|entry| entry.trim() == field);
        if !matched {
            index += 1;
            continue;
        }

        let removed = array.remove(index);
        change = Change::Applied;
        let prefix = prefix_of(&removed).to_owned();
        let suffix = suffix_of(&removed).to_owned();
        // The two halves are salvaged separately, so a comment written after
        // the value keeps its own indentation instead of inheriting the one in
        // front of it.
        let carry: String = [prefix.as_str(), suffix.as_str()]
            .into_iter()
            .filter_map(comment_carry)
            .collect();
        let carry = Some(carry).filter(|carry| !carry.is_empty());
        // Did a comment end up in the array's trailing decor? That decor is
        // printed immediately before the `]`, and [`comment_carry`] always ends
        // it with a newline, so the closing line break is already accounted for
        // and the transfer below must not add a second one.
        let mut carried_to_trailing = false;
        match (carry, array.get_mut(index)) {
            // A comment from the removed entry moves onto its successor.
            (Some(carry), Some(next)) => {
                let kept = prefix_of(next).trim_start_matches('\n').to_owned();
                next.decor_mut().set_prefix(format!("{carry}{kept}"));
            }
            // Nothing follows it, so the comment joins the closing bracket.
            (Some(carry), None) => {
                let kept = text(array.trailing()).trim_start_matches('\n').to_owned();
                array.set_trailing(format!("{carry}{kept}"));
                carried_to_trailing = true;
            }
            // The first entry of a single-line array owns the opening padding.
            (None, Some(next)) if index == 0 && !prefix.contains('\n') => {
                next.decor_mut().set_prefix(prefix);
            }
            (None, _) => {}
        }

        // Taking the last entry away takes the padding before `]` with it —
        // the mirror of the transfer [`append_exclude`] does — so `[ "a", "b" ]`
        // comes back as `[ "a" ]` and `[\n  "a"\n]` keeps its closing line.
        //
        // Skipped when the carry above already moved a comment in front of the
        // `]`. Doing both would print the newline first and push the comment
        // onto a line of its own, so `[\n  "a",  # note\n  "b"\n]` would lose
        // `# note` off the end of `"a"`'s line — the comment survives either
        // way, but it stops meaning what it meant, and in a dotfiles repo that
        // is a diff nobody asked for.
        //
        // Not a `let` chain: the package declares MSRV 1.87 and those landed in
        // 1.88 (`proj-msrv-declare`). `filter` carries the last condition.
        let padding = closing_padding(&suffix);
        if !carried_to_trailing && index == array.len() && !padding.is_empty() {
            let last = index
                .checked_sub(1)
                .and_then(|last| array.get_mut(last))
                .filter(|last| !suffix_of(last).contains('#'));
            if let Some(last) = last {
                last.decor_mut().set_suffix(padding);
            }
        }
    }

    // An array emptied by the last removal collapses back to `[]`, unless its
    // trailing decor is carrying a comment that has to stay on screen.
    if array.is_empty() {
        let trailing = text(array.trailing());
        if !trailing.is_empty() && !trailing.contains('#') {
            array.set_trailing("");
        }
    }
    Ok(change)
}

/// The top-level `exclude` array, borrowed for editing.
///
/// `Ok(None)` when the key is absent — the caller decides whether to create it.
/// An error when the key exists with a non-array value: `exclude = "grpc"` is
/// close enough to right that overwriting it silently would lose data the user
/// meant to keep.
pub fn exclude_array_mut(document: &mut DocumentMut) -> anyhow::Result<Option<&mut Array>> {
    let Some(item) = document.get_mut(EXCLUDE_KEY) else {
        return Ok(None);
    };
    let type_name = item.type_name();
    match item.as_array_mut() {
        Some(array) => Ok(Some(array)),
        None => Err(non_array_error(type_name)),
    }
}

/// Is this array written across several lines?
///
/// Decided by looking for a newline in the decor of the entries and in the
/// array's trailing decor — not by the number of entries, because
/// `exclude = [\n  "a",\n]` is one entry and still multi-line, and a long
/// single-line array is many entries and still single-line.
pub fn is_multiline(array: &Array) -> bool {
    array
        .iter()
        .any(|value| prefix_of(value).contains('\n') || suffix_of(value).contains('\n'))
        || text(array.trailing()).contains('\n')
}

/// The complaint about an `exclude` that is not an array of strings.
fn non_array_error(type_name: &str) -> anyhow::Error {
    anyhow!("`{EXCLUDE_KEY}` is a {type_name}, expected an array of dotted field paths")
}

/// Trims a field name and rejects the empty one.
fn checked_field(field: &str) -> anyhow::Result<&str> {
    let field = field.trim();
    if field.is_empty() {
        bail!("an excluded field cannot be empty");
    }
    Ok(field)
}

/// What a new last entry has to wear to look like it was always there.
///
/// There are three fields rather than one because the text between the last
/// value and the `]` is not one blob: it is that value's suffix, then the
/// optional trailing comma, then the array's trailing decor. An append lands in
/// the middle of all that, so both halves have to be re-cut around it.
struct AppendDecor {
    /// Whitespace — and, ahead of it, any comment that has to stay on the old
    /// last line — printed before the new entry.
    prefix: String,
    /// Padding printed after it, taken over from the entry it displaced as the
    /// last one: the ` ` of `[ "a" ]`, the `\n` of `[\n  "a"\n]`.
    suffix: String,
    /// A replacement for the array's trailing decor, when the append had to cut
    /// it in two. `None` leaves the trailing decor alone.
    trailing: Option<String>,
}

/// Works out the [`AppendDecor`] for an entry appended at the end of `array`.
///
/// The one question that decides everything is whether the new entry goes on a
/// line of its own. It does when the entry it follows is on a line of its own,
/// when the array is empty but written across lines, or when a comment closes
/// the last line and leaves no room on it. Then the indent is copied from the
/// entry above — and only the indent, because the rest of that entry's prefix
/// can be somebody's comment (module docs).
///
/// Otherwise the entry joins the line, copying the separator its neighbours
/// use. The exception is an array of exactly one entry, whose prefix is the
/// array's opening padding and not a separator at all: `["a"]` has to grow a
/// `, ` and not a `,`.
///
/// Either way the new entry inherits the padding that used to sit before the
/// `]`, which is why `[ "a" ]` does not turn into `[ "a" , "c"]`, and the old
/// last entry's suffix is cleared by [`append_exclude`].
fn append_decor(array: &Array) -> AppendDecor {
    let last_index = array.len().checked_sub(1);
    let last = last_index.and_then(|index| array.get(index));
    let last_prefix = last.map_or("", prefix_of);
    let last_suffix = last.map_or("", suffix_of);

    // Everything printed between the last value and the `]`, comma aside. A
    // comment in its first line ends that line, so the new entry cannot share
    // it however short the array looks.
    let closing = format!("{last_suffix}{}", text(array.trailing()));
    let comment_closes_the_line = closing
        .split_once('\n')
        .is_some_and(|(head, _)| head.contains('#'));

    let indent = if let Some((_, indent)) = last_prefix.rsplit_once('\n') {
        Some(indent.to_owned())
    } else if (last.is_none() && is_multiline(array)) || comment_closes_the_line {
        Some(DEFAULT_INDENT.to_owned())
    } else {
        None
    };

    let Some(indent) = indent else {
        let prefix = if last_index == Some(0) && last_prefix.is_empty() {
            DEFAULT_SEPARATOR.to_owned()
        } else {
            last_prefix.to_owned()
        };
        return AppendDecor {
            prefix,
            suffix: last_suffix.to_owned(),
            trailing: None,
        };
    };

    match closing.split_once('\n') {
        // What was written before that first newline belonged to the old last
        // line and follows the comma back onto it — comment included.
        Some((head, rest)) => AppendDecor {
            prefix: format!("{head}\n{indent}"),
            suffix: String::new(),
            trailing: Some(format!("\n{rest}")),
        },
        // Nothing but padding before the `]`, so it stays at the end.
        None => AppendDecor {
            prefix: format!("\n{indent}"),
            suffix: last_suffix.to_owned(),
            trailing: None,
        },
    }
}

/// The part of a removed entry's decor that has to survive it: everything up to
/// and including the last newline, when there is a comment in there at all.
///
/// The tail after that newline is the removed entry's own indent and goes with
/// it. `None` means the decor was pure whitespace and nothing needs saving.
fn comment_carry(decor: &str) -> Option<String> {
    if !decor.contains('#') {
        return None;
    }
    let end = decor.rfind('\n')?;
    Some(decor[..=end].to_owned())
}

/// The whitespace half of a removed entry's suffix: everything a comment in it
/// did not claim, which is what used to sit between the last value and the `]`.
fn closing_padding(suffix: &str) -> &str {
    if suffix.contains('#') {
        suffix.rsplit_once('\n').map_or("", |(_, padding)| padding)
    } else {
        suffix
    }
}

/// A raw decor string as text, treating "not available" as empty.
///
/// `as_str` returns `None` only for a span into an input this document no
/// longer holds; every `DocumentMut` is despanned by `Document::into_mut`, and
/// `FromStr` goes through exactly that, so parsed documents always answer.
fn text(raw: &RawString) -> &str {
    raw.as_str().unwrap_or_default()
}

/// Whitespace and comments written before this value.
fn prefix_of(value: &Value) -> &str {
    value.decor().prefix().map_or("", text)
}

/// Whitespace and comments written after this value, before its comma.
fn suffix_of(value: &Value) -> &str {
    value.decor().suffix().map_or("", text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses test TOML, failing the test rather than the process.
    fn doc(text: &str) -> DocumentMut {
        text.parse::<DocumentMut>().expect("test input parses")
    }

    /// `append_exclude`, then the whole document back as text.
    fn appended(text: &str, field: &str) -> String {
        let mut document = doc(text);
        let change = append_exclude(&mut document, field).expect("append succeeds");
        assert_eq!(change, Change::Applied, "appending {field:?} to {text:?}");
        document.to_string()
    }

    /// `remove_exclude`, then the whole document back as text.
    fn removed(text: &str, field: &str) -> String {
        let mut document = doc(text);
        let change = remove_exclude(&mut document, field).expect("remove succeeds");
        assert_eq!(change, Change::Applied, "removing {field:?} from {text:?}");
        document.to_string()
    }

    #[test]
    fn the_starter_config_parses_and_is_reproduced_byte_for_byte() {
        let document = starter_document().expect("the compiled-in starter parses");
        assert_eq!(document.to_string(), STARTER);
    }

    /// The starter ships `exclude = []` **active** with fourteen fields
    /// commented out underneath. The commented block belongs to the decor of
    /// the next key, so an append has to land in the live array and leave the
    /// block alone — this is the shape every first `hog config -e` hits.
    #[test]
    fn appending_to_the_starter_hits_the_live_array_and_keeps_the_commented_block() {
        let mut document = starter_document().expect("the compiled-in starter parses");
        append_exclude(&mut document, "trace_id").expect("append succeeds");
        let text = document.to_string();

        assert!(text.contains("\nexclude = [\"trace_id\"]\n"), "{text}");
        assert!(
            text.contains("# exclude = [\n#   \"serviceName\", \"serviceVersion\","),
            "the commented example survived: {text}"
        );
        // Everything except the one edited line is untouched.
        let before = STARTER.lines().filter(|line| *line != "exclude = []");
        let after = text
            .lines()
            .filter(|line| *line != "exclude = [\"trace_id\"]");
        assert!(before.eq(after), "{text}");
    }

    /// The table from the module docs, plus the rows that table is shorthand
    /// for. Every row is `(input, output)` for one `append_exclude(_, "c")`.
    #[test]
    fn append_preserves_the_array_style() {
        let rows = [
            ("exclude = []\n", "exclude = [\"c\"]\n"),
            ("exclude = [\"a\"]\n", "exclude = [\"a\", \"c\"]\n"),
            (
                "exclude = [\"a\", \"b\"]\n",
                "exclude = [\"a\", \"b\", \"c\"]\n",
            ),
            // A trailing comma is a property of the array and survives a push.
            ("exclude = [\"a\",]\n", "exclude = [\"a\", \"c\",]\n"),
            // Single-line with padding inside the brackets.
            ("exclude = [ \"a\" ]\n", "exclude = [ \"a\", \"c\" ]\n"),
            // The multi-line shapes, with and without the trailing comma.
            (
                "exclude = [\n  \"a\",\n]\n",
                "exclude = [\n  \"a\",\n  \"c\",\n]\n",
            ),
            (
                "exclude = [\n  \"a\"\n]\n",
                "exclude = [\n  \"a\",\n  \"c\"\n]\n",
            ),
            // A four-space indent is copied, not normalised.
            (
                "exclude = [\n    \"a\",\n]\n",
                "exclude = [\n    \"a\",\n    \"c\",\n]\n",
            ),
            // Several entries per line: the new one joins the last line.
            (
                "exclude = [\n  \"a\", \"b\"\n]\n",
                "exclude = [\n  \"a\", \"b\", \"c\"\n]\n",
            ),
            // A compact separator is a preference, not a mistake.
            (
                "exclude = [\"a\",\"b\"]\n",
                "exclude = [\"a\",\"b\",\"c\"]\n",
            ),
            // No trailing comma, and a comment where the comma would go: the
            // comma has to land before the comment or it is commented out.
            (
                "exclude = [\n  \"a\"  # x\n]\n",
                "exclude = [\n  \"a\",  # x\n  \"c\"\n]\n",
            ),
            // An empty array written across lines stays across lines.
            ("exclude = [\n]\n", "exclude = [\n  \"c\"\n]\n"),
        ];
        for (before, after) in rows {
            assert_eq!(appended(before, "c"), after, "input {before:?}");
        }
    }

    /// The case the naive implementation gets wrong twice over: the comment
    /// lives in the *next* entry's prefix, so copying that prefix would print
    /// it again, and appending after the array's trailing decor would drag the
    /// last line's comment down onto the new entry.
    #[test]
    fn append_does_not_duplicate_or_move_entry_comments() {
        let before = "\
exclude = [
  \"a\",  # the loud one
  \"b\",  # the other one
]
";
        let after = "\
exclude = [
  \"a\",  # the loud one
  \"b\",  # the other one
  \"c\",
]
";
        assert_eq!(appended(before, "c"), after);
    }

    /// A comment on a line of its own, below the last entry, is a note about
    /// what comes next — the new entry goes above it.
    #[test]
    fn append_goes_above_a_dangling_comment() {
        let before = "exclude = [\n  \"a\",\n  # add more here\n]\n";
        let after = "exclude = [\n  \"a\",\n  \"c\",\n  # add more here\n]\n";
        assert_eq!(appended(before, "c"), after);
    }

    #[test]
    fn append_creates_the_key_when_the_file_has_none() {
        let before = "# a config with no exclude key\n[output]\ncolor = \"never\"\n";
        let after = appended(before, "c");
        assert_eq!(
            after,
            "exclude = [\"c\"]\n# a config with no exclude key\n[output]\ncolor = \"never\"\n"
        );
        // The bare key has to land ahead of the `[output]` header, or it would
        // silently become `output.exclude`.
        assert!(
            after.find("exclude") < after.find("[output]"),
            "the bare key landed under the header: {after}"
        );
    }

    #[test]
    fn append_is_idempotent_and_reports_it() {
        let mut document = doc("exclude = [\"a\", \"b\"]\n");
        assert_eq!(
            append_exclude(&mut document, "b").expect("append succeeds"),
            Change::Unchanged
        );
        assert_eq!(document.to_string(), "exclude = [\"a\", \"b\"]\n");
    }

    #[test]
    fn append_trims_the_field_and_matches_trimmed_entries() {
        assert_eq!(appended("exclude = []\n", "  c  "), "exclude = [\"c\"]\n");

        let mut document = doc("exclude = [\n  \"a\",\n]\n");
        assert_eq!(
            append_exclude(&mut document, " a ").expect("append succeeds"),
            Change::Unchanged
        );
    }

    #[test]
    fn an_empty_field_is_rejected_by_both_edits() {
        let mut document = doc("exclude = []\n");
        assert!(append_exclude(&mut document, "   ").is_err());
        assert!(remove_exclude(&mut document, "").is_err());
        assert_eq!(document.to_string(), "exclude = []\n");
    }

    #[test]
    fn remove_preserves_the_formatting_of_what_is_left() {
        let rows = [
            ("exclude = [\"a\", \"b\"]\n", "b", "exclude = [\"a\"]\n"),
            // Dropping the first entry must not leave the padding behind.
            ("exclude = [\"a\", \"b\"]\n", "a", "exclude = [\"b\"]\n"),
            ("exclude = [ \"a\", \"b\" ]\n", "a", "exclude = [ \"b\" ]\n"),
            (
                "exclude = [\"a\", \"b\", \"c\"]\n",
                "b",
                "exclude = [\"a\", \"c\"]\n",
            ),
            (
                "exclude = [\n  \"a\",\n  \"b\",\n]\n",
                "a",
                "exclude = [\n  \"b\",\n]\n",
            ),
            (
                "exclude = [\n  \"a\",\n  \"b\",\n]\n",
                "b",
                "exclude = [\n  \"a\",\n]\n",
            ),
            // Without a trailing comma the newline before `]` belongs to the
            // last entry, so it has to be handed back when that entry goes.
            (
                "exclude = [\n  \"a\",\n  \"b\"\n]\n",
                "b",
                "exclude = [\n  \"a\"\n]\n",
            ),
            ("exclude = [ \"a\", \"b\" ]\n", "b", "exclude = [ \"a\" ]\n"),
            // Every occurrence goes, not just the first.
            (
                "exclude = [\"a\", \"b\", \"a\"]\n",
                "a",
                "exclude = [\"b\"]\n",
            ),
            // The last entry leaves the key behind as an empty array.
            ("exclude = [\"a\"]\n", "a", "exclude = []\n"),
            ("exclude = [\n  \"a\",\n]\n", "a", "exclude = []\n"),
        ];
        for (before, field, after) in rows {
            assert_eq!(removed(before, field), after, "input {before:?}");
        }
    }

    /// The comment written on the line of a removed entry is not that entry's
    /// to take away — it belongs to the line above it.
    #[test]
    fn remove_hands_a_comment_to_the_next_entry() {
        let before = "\
exclude = [
  \"a\",  # about b
  \"b\",
  \"c\",
]
";
        let after = "\
exclude = [
  \"a\",  # about b
  \"c\",
]
";
        assert_eq!(removed(before, "b"), after);
    }

    /// Same rule with nothing left to hand it to: the comment joins the
    /// closing bracket rather than disappearing with the entry.
    #[test]
    fn remove_hands_a_last_entrys_comment_to_the_bracket() {
        let before = "exclude = [\n  \"a\",  # about b\n  \"b\",\n]\n";
        let after = "exclude = [\n  \"a\",  # about b\n]\n";
        assert_eq!(removed(before, "b"), after);
    }

    /// The same gap, but with no comma before the `]`: the carry and the
    /// closing-padding transfer both want that gap, and only the carry may have
    /// it.
    ///
    /// This is the shape a `hog config -e new` leaves behind, so the CLI hits
    /// it on the very next `-d new`. Handing both on printed the newline first
    /// and dropped `# note` onto a line of its own, detaching it from `"a"`.
    #[test]
    fn remove_keeps_a_last_entrys_comment_on_the_line_it_was_written_on() {
        let before = "exclude = [\n  \"a\",  # note\n  \"b\"\n]\n";
        let after = removed(before, "b");
        assert_eq!(after, "exclude = [\n  \"a\"  # note\n]\n");
        assert_eq!(
            exclude_list(&doc(&after)).expect("the result still parses"),
            vec!["a".to_owned()]
        );
    }

    /// A comment block written above an entry survives the entry.
    #[test]
    fn remove_keeps_a_standalone_comment_block() {
        let before = "\
exclude = [
  \"a\",
  # two lines
  # about b
  \"b\",
  \"c\",
]
";
        let after = "\
exclude = [
  \"a\",
  # two lines
  # about b
  \"c\",
]
";
        assert_eq!(removed(before, "b"), after);
    }

    /// The removed entry is last, has no comma after it, and carries a comment:
    /// the comment has to end up in front of the `]` and still on its own line,
    /// or the bracket ends up inside the comment and the file stops parsing.
    #[test]
    fn remove_keeps_a_trailing_comment_out_of_the_brackets_way() {
        let before = "exclude = [\n  \"a\",\n  \"b\"  # about b\n]\n";
        let after = removed(before, "b");
        assert_eq!(after, "exclude = [\n  \"a\"  # about b\n]\n");
        assert_eq!(
            exclude_list(&doc(&after)).expect("the result still parses"),
            vec!["a".to_owned()]
        );
    }

    /// An array emptied of everything but a comment keeps the comment, so the
    /// `[]` grows a line rather than eating one.
    #[test]
    fn remove_keeps_a_comment_in_an_emptied_array() {
        let before = "exclude = [\n  \"a\",\n  # add more here\n]\n";
        assert_eq!(removed(before, "a"), "exclude = [\n  # add more here\n]\n");
    }

    #[test]
    fn removing_something_that_is_not_there_changes_nothing() {
        let mut document = doc("exclude = [\"a\"]\n");
        assert_eq!(
            remove_exclude(&mut document, "zzz").expect("remove succeeds"),
            Change::Unchanged
        );
        assert_eq!(document.to_string(), "exclude = [\"a\"]\n");

        let mut document = doc("[output]\ncolor = \"never\"\n");
        assert_eq!(
            remove_exclude(&mut document, "a").expect("remove succeeds"),
            Change::Unchanged
        );
        assert_eq!(document.to_string(), "[output]\ncolor = \"never\"\n");
    }

    /// The whole point of the round-trip: an edit rewrites one line and leaves
    /// every comment, blank line and alignment in the file alone.
    #[test]
    fn an_edit_touches_nothing_but_the_array() {
        let before = "\
# top comment

exclude = [\"a\"]   # trailing

# about fields
[fields]
ts    = [\"ts\", \"time\"]

[output]
color       = \"never\"   # aligned
";
        let after = before.replace("[\"a\"]", "[\"a\", \"c\"]");
        assert_eq!(appended(before, "c"), after);
        assert_eq!(removed(&after, "c"), before);
    }

    /// Every array shape these functions have to survive, spelled the way a
    /// config file spells it. The last three carry comments *inside* the array,
    /// which is where a naive edit produces TOML that no longer parses.
    const STYLES: [&str; 14] = [
        "[]",
        "[\"a\"]",
        "[\"a\", \"b\"]",
        "[\"a\",\"b\"]",
        "[ \"a\" ]",
        "[\"a\",]",
        "[\n  \"a\",\n]",
        "[\n  \"a\"\n]",
        "[\n  \"a\",\n  \"b\",\n]",
        "[\n    \"a\",\n]",
        "[\n  \"a\", \"b\"\n]",
        "[\n  \"a\",  # x\n  \"b\",\n]",
        "[\n  \"a\"  # x\n]",
        "[\n  \"a\",\n  # note\n]",
    ];

    /// The property that matters more than any single expected string: whatever
    /// the style, the edited file still parses and says exactly what it should.
    ///
    /// `[\n]` is in the matrix too, because an empty array written across lines
    /// is the one shape that does not survive a round-trip unchanged — it comes
    /// back as `[]`, which is why it is checked here and not below.
    #[test]
    fn every_style_still_parses_after_an_edit() {
        for style in STYLES.into_iter().chain(["[\n]"]) {
            let before = format!("exclude = {style}\n");
            let mut document = doc(&before);
            let listed = exclude_list(&document).expect("the fixture is a list of strings");

            append_exclude(&mut document, "zz").expect("append succeeds");
            let reparsed = doc(&document.to_string());
            let mut expected = listed.clone();
            expected.push("zz".to_owned());
            assert_eq!(
                exclude_list(&reparsed).expect("the appended file is still a list"),
                expected,
                "style {style:?} became {document}"
            );

            let mut document = reparsed;
            remove_exclude(&mut document, "zz").expect("remove succeeds");
            let reparsed = doc(&document.to_string());
            assert_eq!(
                exclude_list(&reparsed).expect("the edited file is still a list"),
                listed,
                "style {style:?} became {document}"
            );
        }
    }

    /// Stronger than "it parses": adding a field and taking it away again gives
    /// back the file that was there, byte for byte, in every style.
    #[test]
    fn an_append_and_its_removal_cancel_out() {
        for style in STYLES {
            let before = format!("exclude = {style}\n");
            let mut document = doc(&before);
            append_exclude(&mut document, "zz").expect("append succeeds");
            remove_exclude(&mut document, "zz").expect("remove succeeds");
            assert_eq!(document.to_string(), before, "style {style:?}");
        }
    }

    /// The fourteen fields of the real-world list, uncommented, edited the way
    /// `hog config -e` and `-d` edit them.
    #[test]
    fn the_real_world_list_survives_both_edits() {
        let before = "\
exclude = [
  \"serviceName\", \"serviceVersion\", \"log_id\", \"x_forwarded_for\", \"trace_id\",
  \"grpc.start_time\", \"grpc.time_ms\", \"grpc.code\", \"grpc.method\", \"grpc.service\",
  \"grpc.request.deadline\", \"span.kind\", \"system\", \"logger\",
]
";
        // The list packs several entries per line, so the new one joins the
        // last line rather than inventing a line of its own.
        let added = appended(before, "span.id");
        assert_eq!(
            added,
            before.replace("\"logger\",\n]", "\"logger\", \"span.id\",\n]")
        );
        assert_eq!(removed(&added, "span.id"), before);

        // Removing from the middle of a line closes the gap and leaves the
        // three lines as three lines.
        let dropped = removed(before, "grpc.code");
        assert_eq!(dropped, before.replace(" \"grpc.code\",", ""));
        assert_eq!(
            exclude_list(&doc(&dropped)).expect("still a list").len(),
            13
        );
    }

    #[test]
    fn exclude_list_reports_the_file_order() {
        let document = doc("exclude = [\n  \"b\",\n  \"a\",\n]\n");
        assert_eq!(
            exclude_list(&document).expect("list reads"),
            vec!["b".to_owned(), "a".to_owned()]
        );
        assert_eq!(
            exclude_list(&doc("[output]\n")).expect("list reads"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_exclude_that_is_not_an_array_of_strings_is_an_error() {
        let message = exclude_list(&doc("exclude = \"grpc\"\n"))
            .expect_err("a string is not a list")
            .to_string();
        assert!(message.contains("expected an array"), "{message}");

        let message = exclude_list(&doc("exclude = [\"a\", 2]\n"))
            .expect_err("an integer is not a field path")
            .to_string();
        assert!(message.contains("entry 2"), "{message}");

        let mut document = doc("exclude = \"grpc\"\n");
        assert!(append_exclude(&mut document, "c").is_err());
        assert!(remove_exclude(&mut document, "grpc").is_err());
        assert_eq!(document.to_string(), "exclude = \"grpc\"\n");
    }

    #[test]
    fn multiline_is_about_newlines_not_about_length() {
        let cases = [
            ("exclude = []\n", false),
            ("exclude = [\"a\", \"b\", \"c\", \"d\", \"e\"]\n", false),
            ("exclude = [\n]\n", true),
            ("exclude = [\n  \"a\",\n]\n", true),
            ("exclude = [\"a\",\n]\n", true),
        ];
        for (text, expected) in cases {
            let mut document = doc(text);
            let array = exclude_array_mut(&mut document)
                .expect("exclude is an array")
                .expect("exclude is present");
            assert_eq!(is_multiline(array), expected, "input {text:?}");
        }
    }
}
