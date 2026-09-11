//! Flattening a JSON object into dotted key/value pairs.
//!
//! `{"grpc":{"code":"OK","time_ms":1.5}}` becomes `grpc.code=OK`,
//! `grpc.time_ms=1.5`.
//!
//! # Why the arena
//!
//! The obvious signature returns `Vec<(Cow<'a, str>, Cow<'a, str>)>` borrowing
//! from the input line. It cannot be reused across lines: the `Vec` would have
//! to outlive a lifetime that changes every iteration, and making that compile
//! needs `unsafe`. So both keys and values are copied into one `String` arena
//! owned by the [`Flattener`], and pairs are `(Span, Span)` into it. The arena
//! keeps its capacity across lines, so after a handful of lines the steady state
//! is zero allocations per line (`mem-reuse-collections`) — up to [`ARENA_KEEP`],
//! past which one outsized line would otherwise hold its peak for the whole run.
//!
//! Nested keys have to be built anyway (`grpc` + `.` + `code` exists nowhere in
//! the input), so the arena costs one extra memcpy of the values and buys a
//! type with no lifetime parameter.
//!
//! # How the line is read
//!
//! Two passes, both linear:
//!
//! 1. `serde_json` validates the whole line through `&RawValue`. Nothing is
//!    materialised — no `Value` tree, no `Map`, no `String` per member — so the
//!    pass costs a scan and no allocation. A line it rejects is [`Parsed::NotAnObject`].
//! 2. [`Flattener::walk_object`] walks that already-validated text. Because the
//!    grammar is known to hold, the walk only has to *navigate*: skip a string,
//!    skip a balanced `{}`/`[]`, find the next `,`. It never has to decide
//!    whether `01` is a number or `\q` is an escape — pass 1 settled that.
//!
//! The alternative — a `DeserializeSeed` walk — needs `serde` as a direct
//! dependency (it is not one) and still re-parses every nested object to
//! recover the verbatim text of arrays and numbers. The walk below keeps
//! duplicate keys, keeps document order, and keeps raw text, none of which
//! survive `serde_json::Map`.
//!
//! Defensive posture: every index into the line is bounds-checked and every
//! unexpected shape returns [`Parsed::NotAnObject`] instead of panicking. A
//! hostile line is data, and the walk must not be the thing that crashes on it.
//! The same goes for memory: flattening can amplify a line by three orders of
//! magnitude, so [`ARENA_LIMIT`] bounds the output and a line past it is echoed
//! verbatim rather than reformatted.
//!
//! # What ends up in `value`
//!
//! * JSON string — the **unescaped** text (`"aA"` -> `aA`).
//! * number, `true`, `false`, `null` — the JSON source, verbatim.
//! * array — the JSON source, verbatim, including nested objects inside it.
//!   Arrays are never descended into; hulog did the same, and `a.0.b` keys
//!   would be worse than the raw array.
//! * object — only ever an **empty** one, as `{}`. A non-empty object is
//!   expanded into its children instead. Emitting `{}` is a deliberate fix:
//!   hulog recursed into an empty object, produced nothing, and lost the field.
//!
//! # Duplicate keys
//!
//! `{"a":1,"a":2}` yields **both** pairs, in document order. hulog collected
//! members into a `map[string]string`, so the last one silently won. Printing
//! both is the only answer that loses nothing, and a duplicate key in a log
//! line is exactly the kind of producer bug a human wants to see.
//!
//! # Exclusion
//!
//! The dotted path of every object member is tested against the exclude set
//! **before** recursing. A match means the node is never expanded, which is
//! what makes `-e grpc` prune the entire subtree. Matching is exact per path,
//! so `grpcStatus` is untouched by `-e grpc`.

use serde_json::value::RawValue;

use crate::settings::ExcludeSet;

/// Deepest object nesting the walk will follow, matching `serde_json`'s own
/// recursion limit. Anything deeper is treated as a line we do not understand
/// and echoed verbatim, which bounds the stack against a hostile input.
const MAX_DEPTH: u32 = 128;

/// The value text emitted for an empty object, and the whole reason this
/// constant is named: hulog dropped such a field entirely.
const EMPTY_OBJECT: &str = "{}";

/// Largest flattened text one line may produce, 64× the 1 MiB line cap.
///
/// The line cap bounds what the *reader* holds, not what flattening produces:
/// every member of a nested object copies the whole dotted path, so a long
/// outer key multiplies out. Measured before this limit existed, a legal 1 MiB
/// line of the shape `{"<20 KB key>":{"m":1,"m":1,…}}` drove resident memory
/// to 3.2 GB — a 3400× amplification that made the reader's cap meaningless.
///
/// 64× is far past anything a real logger emits: to exceed it the average
/// dotted path would have to be ~16 times the source text of the member that
/// carries it, i.e. eight levels of ten-character keys around single-character
/// leaves, for a whole megabyte. A line that does exceed it is echoed verbatim
/// — nothing is lost, it is simply not reformatted.
const ARENA_LIMIT: usize = 64 * crate::input::MAX_LINE;

/// Arena capacity kept between lines. One pathological line must not hold its
/// peak for the rest of the run, so anything past a whole line's worth is
/// released (`mem-reuse-collections` still holds for every normal line).
const ARENA_KEEP: usize = crate::input::MAX_LINE;

/// What [`Flattener::flatten`] made of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// A JSON object with at least one member; the pair table is built.
    Object,
    /// A valid but memberless object (`{}`). There is nothing to render, and
    /// rendering it as a blank line would throw away the one fact the line
    /// carries, so the caller echoes the source instead.
    Empty,
    /// Not a JSON object: invalid JSON, or a valid array / string / number /
    /// bool / null. The caller echoes the line verbatim. The pair table is
    /// empty.
    NotAnObject,
}

/// A half-open byte range inside [`Flattener`]'s arena.
///
/// `u32` rather than `usize`: [`ARENA_LIMIT`] is 64 MiB, so every offset fits
/// with three bytes to spare. The conversions below are still written as
/// `try_from` rather than `as`, because a truncating cast would silently
/// mis-slice every pair if that constant ever grew.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: u32,
    end: u32,
}

/// What kind of JSON node a value came from.
///
/// The renderer does not need this to quote correctly — quoting is decided by
/// the characters present — but it keeps the "was this a string or the literal
/// text `null`?" question answerable, which v2's level mapping and `error`
/// field handling will want.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueKind {
    String,
    Number,
    Bool,
    Null,
    /// Always the empty object `{}`.
    Object,
    Array,
}

#[derive(Debug, Clone, Copy)]
struct Pair {
    key: Span,
    value: Span,
    kind: ValueKind,
    /// Set by [`Flattener::take_first`] once the pair has been promoted to one
    /// of the three special columns, so the tail loop skips it.
    taken: bool,
}

/// One flattened key/value pair.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry<'a> {
    /// Full dotted path, e.g. `grpc.time_ms`. This is the string that gets
    /// hashed for the key colour and compared for sorting.
    pub(crate) key: &'a str,
    /// See the module docs for what this holds per [`ValueKind`].
    pub(crate) value: &'a str,
    /// Read by the tests today. The renderer does not need it — quoting is
    /// decided by the characters present — but v2's level remapping and
    /// `error`-field handling do, and recovering it later would mean a second
    /// pass over the line.
    #[allow(dead_code, reason = "v2 consumer; see the field docs")]
    pub(crate) kind: ValueKind,
}

/// Flattens JSON objects, reusing its buffers between lines.
///
/// # Borrowing note for callers
///
/// [`Flattener::entry`] returns a borrow of the flattener, so call it on the
/// **field** (`self.flat.entry(i)`) rather than through a method on the
/// enclosing struct. Field-level borrows keep `self.scratch` and `self.theme`
/// free while an [`Entry`] is alive.
#[derive(Debug, Default)]
pub(crate) struct Flattener {
    /// Every key path and value text for the current line, concatenated.
    arena: String,
    /// Pairs in JSON document order, until [`Flattener::sort`] reorders them.
    pairs: Vec<Pair>,
    /// The dotted path being built during the walk. Grown and truncated, never
    /// reallocated after the first few lines.
    path: String,
}

impl Flattener {
    /// Parses `json` and rebuilds the pair table.
    ///
    /// Resets all state first, so the caller never has to. On anything but
    /// [`Parsed::Object`] the table is empty and the caller echoes the line
    /// verbatim — a line that is not a JSON object is not a program error.
    ///
    /// The skeleton typed this as `Result<(), serde_json::Error>`. It returns
    /// [`Parsed`] instead for two reasons: "valid JSON, but an array" has no
    /// natural `serde_json::Error` to report (manufacturing one costs an
    /// allocation on what is otherwise the cheapest path in the crate), and the
    /// empty-object case needs a third answer that a `Result` cannot carry.
    pub(crate) fn flatten(&mut self, json: &str, exclude: &ExcludeSet) -> Parsed {
        self.reset();

        let trimmed = json.trim_start();
        // The cheap rejection first: a stack trace or a startup banner never
        // reaches the parser at all. It also means every line that *does* reach
        // `from_str` and survives is an object, since a JSON value starting
        // with `{` can be nothing else.
        if !trimmed.starts_with('{') {
            return Parsed::NotAnObject;
        }
        // One validating pass. `&RawValue` borrows the line and materialises
        // nothing, and `from_str` additionally rejects trailing garbage.
        let Ok(raw) = serde_json::from_str::<&RawValue>(trimmed) else {
            return Parsed::NotAnObject;
        };

        match self.walk_object(raw.get(), exclude, 0) {
            Some(0) => Parsed::Empty,
            Some(_) => Parsed::Object,
            None => {
                // Unreachable for input serde_json just validated, but a walk
                // that cannot make sense of a line degrades to pass-through
                // rather than rendering half of it.
                self.reset();
                Parsed::NotAnObject
            }
        }
    }

    /// Walks one object, emitting a pair per leaf. Returns its member count, or
    /// `None` if the text does not have the shape of an object.
    ///
    /// `text` starts at the `{` and is already known to be valid JSON.
    fn walk_object(&mut self, text: &str, exclude: &ExcludeSet, depth: u32) -> Option<usize> {
        if depth > MAX_DEPTH {
            return None;
        }
        let bytes = text.as_bytes();
        if bytes.first() != Some(&b'{') {
            return None;
        }

        let mut at = skip_ws(bytes, 1);
        let mut members = 0usize;
        if bytes.get(at) == Some(&b'}') {
            return Some(0);
        }

        loop {
            let (key, after_key) = scan_string(text, at)?;
            members += 1;

            at = skip_ws(bytes, after_key);
            if bytes.get(at) != Some(&b':') {
                return None;
            }
            at = skip_ws(bytes, at + 1);
            let value_end = skip_value(bytes, at)?;
            let value = text.get(at..value_end)?;
            at = skip_ws(bytes, value_end);

            // The dotted path is built for every member, excluded or not: the
            // exclusion test needs it, and it is the key of every leaf below.
            let base = self.path.len();
            if base != 0 {
                self.path.push('.');
            }
            push_json_str(&mut self.path, key)?;

            // HLD §6, invariant 3: tested *before* descending, which is what
            // makes `-e grpc` prune `grpc.request.deadline` too.
            if !exclude.contains(&self.path) {
                self.push_value(value, exclude, depth)?;
            }
            self.path.truncate(base);

            match bytes.get(at)? {
                b',' => at = skip_ws(bytes, at + 1),
                b'}' => return Some(members),
                _ => return None,
            }
        }
    }

    /// Emits one member's value: a leaf becomes a pair, a non-empty object is
    /// walked instead.
    fn push_value(&mut self, value: &str, exclude: &ExcludeSet, depth: u32) -> Option<()> {
        match value.as_bytes().first()? {
            b'{' => {
                if object_is_empty(value) {
                    self.push_pair(EMPTY_OBJECT, ValueKind::Object)?;
                } else {
                    self.walk_object(value, exclude, depth + 1)?;
                }
            }
            // Arrays keep their source text; see the module docs.
            b'[' => self.push_pair(value, ValueKind::Array)?,
            b'"' => self.push_string(value)?,
            b't' | b'f' => self.push_pair(value, ValueKind::Bool)?,
            b'n' => self.push_pair(value, ValueKind::Null)?,
            // Numbers keep their source text too, so `1.50` and `1e3` survive
            // as written rather than being round-tripped through `f64`.
            _ => self.push_pair(value, ValueKind::Number)?,
        }
        Some(())
    }

    /// Does `extra` more bytes still fit under [`ARENA_LIMIT`]?
    ///
    /// Checked *before* the copy, which is the whole point: the amplification
    /// this guards against is a memory-exhaustion vector, so noticing it after
    /// the allocation would be too late.
    ///
    /// Every caller keeps `arena.len() <= ARENA_LIMIT`, so the subtraction
    /// cannot underflow — `saturating_sub` rather than `-` anyway, because a
    /// wrapped subtraction here would silently turn the guard off.
    fn arena_fits(&self, extra: usize) -> bool {
        extra <= ARENA_LIMIT.saturating_sub(self.arena.len())
    }

    /// Copies the current dotted path into the arena and returns its span.
    ///
    /// `None` once the line would push the arena past [`ARENA_LIMIT`]; the
    /// caller unwinds to [`Parsed::NotAnObject`] and the line is echoed
    /// verbatim. One comparison per pair buys a hard bound on how much memory
    /// a single log line can cost.
    fn push_key(&mut self) -> Option<Span> {
        if !self.arena_fits(self.path.len()) {
            return None;
        }
        let start = u32::try_from(self.arena.len()).ok()?;
        // `arena` and `path` are distinct fields, so this borrows cleanly.
        self.arena.push_str(&self.path);
        let end = u32::try_from(self.arena.len()).ok()?;
        Some(Span { start, end })
    }

    fn push_pair(&mut self, value: &str, kind: ValueKind) -> Option<()> {
        let key = self.push_key()?;
        if !self.arena_fits(value.len()) {
            return None;
        }
        let start = u32::try_from(self.arena.len()).ok()?;
        self.arena.push_str(value);
        let end = u32::try_from(self.arena.len()).ok()?;
        self.pairs.push(Pair {
            key,
            value: Span { start, end },
            kind,
            taken: false,
        });
        Some(())
    }

    /// Emits a JSON string value, unescaped straight into the arena.
    fn push_string(&mut self, quoted: &str) -> Option<()> {
        let key = self.push_key()?;
        // Unescaping only ever shrinks — `\n` is two bytes in, one out, and the
        // two quotes go away — so the quoted length is a safe upper bound.
        if !self.arena_fits(quoted.len()) {
            return None;
        }
        let start = u32::try_from(self.arena.len()).ok()?;
        push_json_str(&mut self.arena, quoted)?;
        let end = u32::try_from(self.arena.len()).ok()?;
        self.pairs.push(Pair {
            key,
            value: Span { start, end },
            kind: ValueKind::String,
            taken: false,
        });
        Some(())
    }

    fn reset(&mut self) {
        self.arena.clear();
        // Buffers are reused between lines, but a line that legitimately needed
        // tens of megabytes must not make the process hold them forever.
        if self.arena.capacity() > ARENA_KEEP {
            self.arena.shrink_to(ARENA_KEEP);
        }
        self.pairs.clear();
        self.path.clear();
    }

    /// Sorts the tail alphabetically by full dotted path, with
    /// `sort_unstable_by` (`coll-map-choice`: a sorted `Vec`, never a
    /// `BTreeMap` allocating per node).
    ///
    /// Duplicate keys keep an unspecified order relative to each other; they
    /// are all still printed.
    ///
    /// Sorting moves pairs, so an index returned by [`Flattener::take_first`]
    /// does not survive it. Callers sort first, then take.
    pub(crate) fn sort(&mut self) {
        // Moved out so the comparator can borrow the arena immutably while the
        // `Vec` is borrowed mutably. The `Vec` keeps its buffer across the trip.
        let mut pairs = std::mem::take(&mut self.pairs);
        pairs.sort_unstable_by(|a, b| span(&self.arena, a.key).cmp(span(&self.arena, b.key)));
        self.pairs = pairs;
    }

    /// Promotes the first pair matching a candidate to a special column.
    ///
    /// Candidates are tried **in order**, so `["ts", "time"]` prefers `ts`.
    /// Returns the index of the promoted pair, which stays readable through
    /// [`Flattener::value_at`] but is skipped by [`Flattener::entry`]. Losing
    /// candidates are not consumed and still print in the tail — and so does
    /// the second of two duplicate `msg` keys.
    pub(crate) fn take_first(&mut self, candidates: &[String]) -> Option<usize> {
        for candidate in candidates {
            let found = self
                .pairs
                .iter()
                .position(|pair| !pair.taken && span(&self.arena, pair.key) == candidate.as_str());
            if let Some(index) = found {
                self.pairs[index].taken = true;
                return Some(index);
            }
        }
        None
    }

    /// Number of pairs, taken ones included. Index space for [`Flattener::entry`].
    pub(crate) fn len(&self) -> usize {
        self.pairs.len()
    }

    #[allow(dead_code, reason = "asserted by the flatten tests")]
    pub(crate) fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// The pair at `index`, or `None` if it was promoted to a column.
    ///
    /// The tail loop is `for i in 0..flat.len() { let Some(e) = flat.entry(i) else { continue }; … }`.
    pub(crate) fn entry(&self, index: usize) -> Option<Entry<'_>> {
        let pair = self.pairs.get(index)?;
        if pair.taken {
            return None;
        }
        Some(Entry {
            key: span(&self.arena, pair.key),
            value: span(&self.arena, pair.value),
            kind: pair.kind,
        })
    }

    /// Value text of any pair, promoted or not. Used to read the timestamp,
    /// level and message back out after [`Flattener::take_first`].
    ///
    /// An out-of-range index yields `""` rather than a panic; the only indices
    /// in play come from [`Flattener::take_first`] and are always in range.
    pub(crate) fn value_at(&self, index: usize) -> &str {
        self.pairs
            .get(index)
            .map_or("", |pair| span(&self.arena, pair.value))
    }

    /// Key path of any pair, promoted or not.
    #[allow(dead_code, reason = "asserted by the flatten tests")]
    pub(crate) fn key_at(&self, index: usize) -> &str {
        self.pairs
            .get(index)
            .map_or("", |pair| span(&self.arena, pair.key))
    }
}

fn span(arena: &str, span: Span) -> &str {
    arena
        .get(span.start as usize..span.end as usize)
        .unwrap_or_default()
}

/// Is this object text `{}` (with any whitespace inside)?
fn object_is_empty(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.get(skip_ws(bytes, 1)) == Some(&b'}')
}

fn skip_ws(bytes: &[u8], mut at: usize) -> usize {
    while let Some(b' ' | b'\t' | b'\n' | b'\r') = bytes.get(at) {
        at += 1;
    }
    at
}

/// Returns the quoted string starting at `at` — quotes included — and the index
/// just past its closing quote.
///
/// Multi-byte UTF-8 needs no special case: no continuation byte can be `"` or
/// `\`, and the quotes are ASCII, so every slice boundary lands on a character
/// boundary.
fn scan_string(text: &str, at: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(at) != Some(&b'"') {
        return None;
    }
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            // Two-byte skip is enough even for `\uXXXX`: the four hex digits
            // cannot be a quote, so the scan cannot end early inside one.
            b'\\' => i += 2,
            b'"' => return Some((text.get(at..=i)?, i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// Returns the index just past the value starting at `at`.
fn skip_value(bytes: &[u8], at: usize) -> Option<usize> {
    match *bytes.get(at)? {
        b'"' => skip_string(bytes, at),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = at;
            while i < bytes.len() {
                match bytes[i] {
                    // Strings are skipped whole, so a brace inside one — the
                    // `[{"a":"}"}]` case — cannot unbalance the count.
                    b'"' => i = skip_string(bytes, i)?,
                    b'{' | b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        i += 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    _ => i += 1,
                }
            }
            None
        }
        // A number, `true`, `false` or `null`: runs to the first delimiter.
        _ => {
            let mut i = at;
            while let Some(byte) = bytes.get(i) {
                if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                    break;
                }
                i += 1;
            }
            (i != at).then_some(i)
        }
    }
}

fn skip_string(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Appends the decoded contents of a quoted JSON string to `out`.
///
/// Three tiers, cheapest first: no backslash at all is a plain `push_str` of a
/// borrowed slice; the eight one-letter escapes are decoded in place; `\uXXXX`
/// hands the whole string to `serde_json`, which already knows how to pair
/// surrogates. Only that last tier allocates, and only for the string that
/// needs it.
fn push_json_str(out: &mut String, quoted: &str) -> Option<()> {
    let inner = quoted.get(1..quoted.len().checked_sub(1)?)?;
    if !inner.as_bytes().contains(&b'\\') {
        out.push_str(inner);
        return Some(());
    }

    let mark = out.len();
    if unescape_simple(inner, out).is_none() {
        out.truncate(mark);
        let decoded: String = serde_json::from_str(quoted).ok()?;
        out.push_str(&decoded);
    }
    Some(())
}

/// Decodes the escapes that need no lookahead. Returns `None` on `\u`, leaving
/// the caller to fall back; `out` may hold a partial result, which is why the
/// caller marks and truncates.
fn unescape_simple(inner: &str, out: &mut String) -> Option<()> {
    let bytes = inner.as_bytes();
    let mut copied = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        out.push_str(inner.get(copied..i)?);
        let decoded = match *bytes.get(i + 1)? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            // `\uXXXX`, surrogate pairs and all.
            _ => return None,
        };
        out.push(decoded);
        i += 2;
        copied = i;
    }
    out.push_str(inner.get(copied..)?);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fixtures below build JSON by hand, and `format_push_string` is
    // `deny` for this crate (HLD §7: the renderer writes into a buffer it
    // reuses, never into a fresh `format!`). The rule is not relaxed here even
    // though a fixture is not a hot path: a `cfg_attr(test, allow(...))` at the
    // crate root would also switch it off for the renderer's *own* unit tests,
    // which is precisely where a per-line allocation would first be written.
    // `write!` into the buffer costs one line and keeps the deny total.
    use std::fmt::Write as _;

    fn excludes(paths: &[&str]) -> ExcludeSet {
        ExcludeSet::new(paths.iter().map(|path| (*path).to_owned()))
    }

    /// Flattens with no exclusions and renders the table as `k=v k=v` so a test
    /// can assert on one string. Document order, not sorted.
    fn flat(json: &str) -> String {
        flat_excluding(json, &[])
    }

    fn flat_excluding(json: &str, exclude: &[&str]) -> String {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(json, &excludes(exclude)),
            Parsed::Object,
            "expected {json} to flatten as an object"
        );
        dump(&flattener)
    }

    fn dump(flattener: &Flattener) -> String {
        let mut out = String::new();
        for index in 0..flattener.len() {
            let Some(entry) = flattener.entry(index) else {
                continue;
            };
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(entry.key);
            out.push('=');
            out.push_str(entry.value);
        }
        out
    }

    fn parse(json: &str) -> Parsed {
        Flattener::default().flatten(json, &ExcludeSet::default())
    }

    fn kinds(json: &str) -> Vec<ValueKind> {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(json, &ExcludeSet::default()),
            Parsed::Object
        );
        (0..flattener.len())
            .filter_map(|index| flattener.entry(index))
            .map(|entry| entry.kind)
            .collect()
    }

    // ----------------------------------------------------------- structure

    #[test]
    fn flat_object() {
        assert_eq!(flat(r#"{"a":1,"b":"x"}"#), "a=1 b=x");
    }

    #[test]
    fn nested_objects_become_dotted_paths() {
        assert_eq!(
            flat(r#"{"grpc":{"code":"OK","time_ms":1.5}}"#),
            "grpc.code=OK grpc.time_ms=1.5"
        );
    }

    #[test]
    fn nesting_goes_all_the_way_down() {
        assert_eq!(flat(r#"{"a":{"b":{"c":{"d":7}}}}"#), "a.b.c.d=7");
    }

    #[test]
    fn document_order_is_preserved_before_sorting() {
        assert_eq!(flat(r#"{"z":1,"a":2,"m":3}"#), "z=1 a=2 m=3");
    }

    #[test]
    fn whitespace_between_tokens_is_tolerated() {
        assert_eq!(flat("{ \"a\" : 1 , \"b\" : { \"c\" : 2 } }"), "a=1 b.c=2");
    }

    #[test]
    fn leading_whitespace_before_the_object_is_tolerated() {
        assert_eq!(flat("   {\"a\":1}"), "a=1");
    }

    // --------------------------------------------------------------- values

    #[test]
    fn numbers_keep_their_source_text() {
        // Round-tripping through f64 would print `1`, `100000`, `1.5` — and
        // would quietly mangle an integer wider than 53 bits.
        assert_eq!(
            flat(r#"{"a":1.50,"b":1e5,"c":-0.0,"d":12345678901234567890}"#),
            "a=1.50 b=1e5 c=-0.0 d=12345678901234567890"
        );
    }

    #[test]
    fn literals_keep_their_source_text() {
        assert_eq!(
            flat(r#"{"t":true,"f":false,"n":null}"#),
            "t=true f=false n=null"
        );
    }

    #[test]
    fn arrays_are_not_descended_into() {
        assert_eq!(
            flat(r#"{"a":[1,2,{"b":3}],"c":4}"#),
            r#"a=[1,2,{"b":3}] c=4"#
        );
    }

    #[test]
    fn array_containing_a_brace_in_a_string_stays_balanced() {
        assert_eq!(flat(r#"{"a":[{"x":"}"}],"b":1}"#), r#"a=[{"x":"}"}] b=1"#);
    }

    #[test]
    fn strings_are_unescaped() {
        assert_eq!(flat(r#"{"a":"say \"hi\"\n"}"#), "a=say \"hi\"\n");
    }

    #[test]
    fn all_one_letter_escapes_decode() {
        assert_eq!(
            flat(r#"{"a":"\"\\\/\b\f\n\r\t"}"#),
            "a=\"\\/\u{8}\u{c}\n\r\t"
        );
    }

    #[test]
    fn unicode_escapes_fall_back_to_serde() {
        assert_eq!(flat(r#"{"a":"\u0041\u00e9"}"#), "a=Aé");
    }

    #[test]
    fn surrogate_pairs_decode() {
        assert_eq!(flat(r#"{"a":"\ud83d\udca9"}"#), "a=💩");
    }

    #[test]
    fn a_simple_escape_before_a_unicode_one_does_not_duplicate_output() {
        // The simple decoder writes `x\n` into the arena, hits `\u` and bails;
        // the fallback must truncate that partial write, not append to it.
        assert_eq!(flat(r#"{"a":"x\n\u0041y"}"#), "a=x\nAy");
    }

    #[test]
    fn multibyte_text_survives_the_walk() {
        assert_eq!(flat(r#"{"ключ":"значение","b":2}"#), "ключ=значение b=2");
    }

    // -------------------------------------------------- nothing disappears

    #[test]
    fn empty_containers_and_empty_strings_stay_visible() {
        // hulog printed `a= c= d=` and dropped `b` entirely.
        assert_eq!(
            flat(r#"{"a":"","b":{},"c":[],"d":null}"#),
            "a= b={} c=[] d=null"
        );
    }

    #[test]
    fn a_deeply_nested_empty_object_still_shows() {
        assert_eq!(flat(r#"{"a":{"b":{}}}"#), "a.b={}");
    }

    #[test]
    fn duplicate_keys_are_all_kept_in_document_order() {
        assert_eq!(flat(r#"{"a":1,"b":2,"a":3}"#), "a=1 b=2 a=3");
    }

    #[test]
    fn an_empty_key_is_still_a_key() {
        assert_eq!(flat(r#"{"":1,"a":{"":2}}"#), "=1 a.=2");
    }

    #[test]
    fn kinds_are_reported_per_value() {
        assert_eq!(
            kinds(r#"{"s":"x","n":1,"b":true,"z":null,"o":{},"a":[]}"#),
            vec![
                ValueKind::String,
                ValueKind::Number,
                ValueKind::Bool,
                ValueKind::Null,
                ValueKind::Object,
                ValueKind::Array,
            ]
        );
    }

    // ------------------------------------------------------------ exclusion

    #[test]
    fn exclusion_prunes_the_whole_subtree() {
        assert_eq!(
            flat_excluding(
                r#"{"grpc":{"code":"OK","request":{"deadline":"1s"}},"msg":"hi"}"#,
                &["grpc"],
            ),
            "msg=hi"
        );
    }

    #[test]
    fn exclusion_matches_whole_segments_not_string_prefixes() {
        // The column that matters in the HLD §6 table: `grpcStatus` begins with
        // the same letters but is a different node, so it survives.
        assert_eq!(
            flat_excluding(
                r#"{"grpc":{"code":"OK"},"grpcStatus":2,"grpc_id":3}"#,
                &["grpc"],
            ),
            "grpcStatus=2 grpc_id=3"
        );
    }

    #[test]
    fn exclusion_of_an_inner_node_keeps_its_siblings() {
        let json = r#"{"grpc":{"code":"OK","request":{"deadline":"1s","id":7}}}"#;
        assert_eq!(flat_excluding(json, &["grpc.request"]), "grpc.code=OK");
        assert_eq!(
            flat_excluding(json, &["grpc.request.deadline"]),
            "grpc.code=OK grpc.request.id=7"
        );
    }

    #[test]
    fn exclusion_of_a_leaf_by_short_name_does_not_reach_into_a_subtree() {
        // `-e code` hides the top-level `code`, never `grpc.code`: the set is
        // matched against full dotted paths only.
        assert_eq!(
            flat_excluding(r#"{"code":1,"grpc":{"code":2}}"#, &["code"]),
            "grpc.code=2"
        );
    }

    #[test]
    fn excluding_every_member_leaves_an_empty_table() {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(r#"{"a":1}"#, &excludes(&["a"])),
            // Still `Object`: the line *had* a member, the user hid it. Only a
            // genuinely memberless `{}` is `Empty`.
            Parsed::Object
        );
        assert!(flattener.is_empty());
    }

    #[test]
    fn an_object_whose_children_are_all_excluded_vanishes_rather_than_printing_braces() {
        assert_eq!(
            flat_excluding(r#"{"grpc":{"code":1},"a":2}"#, &["grpc.code"]),
            "a=2"
        );
    }

    // ----------------------------------------------------------- pass-through

    #[test]
    fn non_json_lines_are_not_objects() {
        for line in [
            "",
            "   ",
            "plain text",
            "goroutine 1 [running]:",
            "not json {\"a\":1}",
        ] {
            assert_eq!(parse(line), Parsed::NotAnObject, "line: {line:?}");
        }
    }

    #[test]
    fn valid_json_that_is_not_an_object_is_not_an_object() {
        for line in ["[1,2]", r#""str""#, "42", "true", "null"] {
            assert_eq!(parse(line), Parsed::NotAnObject, "line: {line:?}");
        }
    }

    #[test]
    fn malformed_json_objects_are_not_objects() {
        for line in [
            r#"{"a":1"#,
            r#"{"a":}"#,
            r#"{"a" 1}"#,
            r#"{"a":1,}"#,
            "{a:1}",
            r#"{"a":1} trailing"#,
            r#"{"a":"unterminated}"#,
        ] {
            assert_eq!(parse(line), Parsed::NotAnObject, "line: {line:?}");
        }
    }

    #[test]
    fn an_empty_object_is_its_own_answer() {
        assert_eq!(parse("{}"), Parsed::Empty);
        assert_eq!(parse("{ }"), Parsed::Empty);
        assert_eq!(parse("  {}  "), Parsed::Empty);
    }

    #[test]
    fn a_failed_parse_leaves_no_stale_pairs_behind() {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(r#"{"a":1}"#, &ExcludeSet::default()),
            Parsed::Object
        );
        assert_eq!(flattener.len(), 1);

        assert_eq!(
            flattener.flatten("nonsense", &ExcludeSet::default()),
            Parsed::NotAnObject
        );
        assert!(flattener.is_empty());
        assert_eq!(dump(&flattener), "");
    }

    #[test]
    fn nesting_past_the_depth_limit_is_refused_instead_of_overflowing_the_stack() {
        let depth = 4096;
        let mut json = String::new();
        for _ in 0..depth {
            json.push_str(r#"{"a":"#);
        }
        json.push('1');
        json.push_str(&"}".repeat(depth));
        // serde_json's own recursion limit rejects this first; the walk's limit
        // is the backstop if that ever changes.
        assert_eq!(parse(&json), Parsed::NotAnObject);
    }

    /// The line cap bounds what the reader holds, not what flattening produces:
    /// every member of a nested object copies the whole dotted path, so a long
    /// outer key multiplies out. Before [`ARENA_LIMIT`] existed, a legal 1 MiB
    /// line of exactly this shape took 3.2 GB of resident memory.
    ///
    /// The line is refused rather than rendered, so the caller echoes it — a
    /// megabyte of source instead of gigabytes of reformatted source.
    #[test]
    fn a_line_that_would_explode_the_arena_is_refused_not_rendered() {
        let key = "K".repeat(20_000);
        let members = 4_000;
        let mut json = String::with_capacity(key.len() + members * 6 + 8);
        let _ = write!(json, r#"{{"{key}":{{"#);
        for index in 0..members {
            if index != 0 {
                json.push(',');
            }
            json.push_str(r#""m":1"#);
        }
        json.push_str("}}");

        // 4000 members * ~20 KB of dotted path = ~80 MB, past the 64 MiB cap,
        // from a source line of only ~44 KB.
        assert!(json.len() < ARENA_LIMIT / 64, "the source must stay small");
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(&json, &ExcludeSet::default()),
            Parsed::NotAnObject
        );
        assert!(
            flattener.is_empty(),
            "a refused line leaves no pairs behind"
        );
        assert!(
            flattener.arena.len() <= ARENA_LIMIT,
            "the arena grew past its own cap"
        );

        // And the flattener still works on the next line.
        assert_eq!(
            flattener.flatten(r#"{"a":1}"#, &ExcludeSet::default()),
            Parsed::Object
        );
        assert_eq!(dump(&flattener), "a=1");
        assert!(
            flattener.arena.capacity() <= ARENA_KEEP,
            "one huge line must not hold its buffer for the rest of the run"
        );
    }

    /// The other side of the cap: a legitimately deep, legitimately large line
    /// renders. Eight levels of long keys amplify under 2×, so the 64× headroom
    /// is not something a real logger can trip over.
    #[test]
    fn deep_but_realistic_nesting_stays_well_inside_the_cap() {
        let mut json = String::from("{");
        for branch in 0..200 {
            if branch != 0 {
                json.push(',');
            }
            let _ = write!(json, r#""branch_{branch:05}":"#);
            for level in 0..8 {
                let _ = write!(json, r#"{{"segment_{level:02}":"#);
            }
            json.push_str("42");
            json.push_str(&"}".repeat(8));
        }
        json.push('}');

        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(&json, &ExcludeSet::default()),
            Parsed::Object
        );
        assert_eq!(flattener.len(), 200);
        assert!(
            flattener.arena.len() < json.len() * 4,
            "realistic nesting amplified {}x",
            flattener.arena.len() / json.len()
        );
    }

    // -------------------------------------------------------------- reuse

    #[test]
    fn buffers_are_reused_and_state_does_not_leak_between_lines() {
        let mut flattener = Flattener::default();
        let exclude = excludes(&["drop"]);

        assert_eq!(
            flattener.flatten(r#"{"a":{"b":1},"drop":9}"#, &exclude),
            Parsed::Object
        );
        assert_eq!(dump(&flattener), "a.b=1");

        assert_eq!(flattener.flatten(r#"{"c":2}"#, &exclude), Parsed::Object);
        assert_eq!(dump(&flattener), "c=2");

        assert_eq!(
            flattener.flatten(r#"{"a":{"b":1}}"#, &exclude),
            Parsed::Object
        );
        assert_eq!(dump(&flattener), "a.b=1");
    }

    // --------------------------------------------------------------- sort

    #[test]
    fn sort_orders_by_full_dotted_path() {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(
                r#"{"z":1,"grpc":{"time_ms":2,"code":3},"a":4}"#,
                &ExcludeSet::default()
            ),
            Parsed::Object
        );
        flattener.sort();
        assert_eq!(dump(&flattener), "a=4 grpc.code=3 grpc.time_ms=2 z=1");
    }

    #[test]
    fn sorting_is_by_byte_order_of_the_whole_path() {
        // `grpc.code` sorts before `grpcStatus` because `.` (0x2e) < `S` (0x53).
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(
                r#"{"grpcStatus":1,"grpc":{"code":2}}"#,
                &ExcludeSet::default()
            ),
            Parsed::Object
        );
        flattener.sort();
        assert_eq!(dump(&flattener), "grpc.code=2 grpcStatus=1");
    }

    // ----------------------------------------------------------- take_first

    #[test]
    fn take_first_prefers_the_earlier_candidate() {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(r#"{"time":"T","ts":"S","msg":"m"}"#, &ExcludeSet::default()),
            Parsed::Object
        );
        let candidates = vec!["ts".to_owned(), "time".to_owned()];
        let taken = flattener.take_first(&candidates).expect("ts is present");

        assert_eq!(flattener.key_at(taken), "ts");
        assert_eq!(flattener.value_at(taken), "S");
        // The losing candidate is untouched and still prints.
        assert_eq!(dump(&flattener), "time=T msg=m");
    }

    #[test]
    fn take_first_falls_through_to_a_later_candidate() {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(r#"{"time":"T"}"#, &ExcludeSet::default()),
            Parsed::Object
        );
        let candidates = vec!["ts".to_owned(), "time".to_owned()];
        let taken = flattener.take_first(&candidates).expect("time is present");
        assert_eq!(flattener.key_at(taken), "time");
        assert_eq!(dump(&flattener), "");
    }

    #[test]
    fn take_first_returns_none_when_no_candidate_matches() {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(r#"{"a":1}"#, &ExcludeSet::default()),
            Parsed::Object
        );
        assert_eq!(flattener.take_first(&["ts".to_owned()]), None);
        assert_eq!(dump(&flattener), "a=1");
    }

    #[test]
    fn take_first_consumes_only_one_of_two_duplicates() {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(r#"{"msg":"one","msg":"two"}"#, &ExcludeSet::default()),
            Parsed::Object
        );
        let taken = flattener
            .take_first(&["msg".to_owned()])
            .expect("msg is present");
        assert_eq!(flattener.value_at(taken), "one");
        assert_eq!(dump(&flattener), "msg=two");
    }

    #[test]
    fn take_first_does_not_match_a_nested_path_by_its_leaf_name() {
        let mut flattener = Flattener::default();
        assert_eq!(
            flattener.flatten(r#"{"a":{"msg":"nested"}}"#, &ExcludeSet::default()),
            Parsed::Object
        );
        assert_eq!(flattener.take_first(&["msg".to_owned()]), None);
    }

    #[test]
    fn out_of_range_reads_do_not_panic() {
        let flattener = Flattener::default();
        assert_eq!(flattener.value_at(7), "");
        assert_eq!(flattener.key_at(7), "");
        assert!(flattener.entry(7).is_none());
    }
}
