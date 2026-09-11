//! Turning one input line into one output line.
//!
//! # Output shape
//!
//! ```text
//! <timestamp> [LVL] <message> key=value key=value …
//! ```
//!
//! Every part is optional: a line with no timestamp field simply starts at the
//! level tag. **One input line always produces exactly one output line** — the
//! invariant `hog | grep` and `hog | head` depend on, which is why multi-line
//! value rendering was rejected for v1 (HLD §8).
//!
//! Parts are joined by exactly one space, and a part that is absent takes its
//! separator with it. hulog wrote a trailing space after the timestamp and the
//! level tag and another before every tail key, so a line without a message
//! came out as `10:32:01 [INF]  port=8080` with a double space. That is a
//! defect of the concatenation, not a format anyone chose.
//!
//! "Absent" is decided by what a part actually *printed*, not by whether its
//! field was in the line: `{"ts":"","msg":"","a":1}` carries both fields, both
//! render as zero characters, and the line is `a=1` — not `  a=1`. An empty
//! string in a tail value still prints as `k=""`, because there the key makes
//! the field visible and losing it would lose a fact.
//!
//! # Pass-through
//!
//! A line is echoed byte for byte, with no parsing at all, when:
//!
//! * it is not valid UTF-8 (see [`Renderer::render_bytes`]);
//! * its first non-whitespace byte is not `{`;
//! * `serde_json` rejects it;
//! * it is a valid JSON value that is not an object (`[1,2]`, `"str"`, `42`);
//! * it is the memberless object `{}`, which would otherwise render as a blank
//!   line and lose the only thing the line said.
//!
//! That keeps `hog` usable on a stream that mixes JSON with a stack trace or a
//! startup banner.
//!
//! # Contract (HLD §6, "Инварианты рендера")
//!
//! 1. **The tail is sorted alphabetically by full dotted path**, with
//!    `sort_unstable_by`, so two lines line up column-wise by eye and the
//!    output is stable under `diff`. `sort_keys = false` keeps JSON order.
//! 2. **A key's colour is `FNV-1a(full dotted path) % palette.len()`** and
//!    depends on nothing else — not position, not neighbours, not order of
//!    appearance. `trace_id` is the same colour today, tomorrow, and on another
//!    machine, so the eye finds it without reading. `grpc.code` and `code` hash
//!    differently and get different colours.
//! 3. **An exclusion prunes the whole subtree**, because the dotted path is
//!    tested *before* descending into the node.
//!
//! # Hot path rules
//!
//! `format!` is banned in here (`anti-format-hot-path`). Write into the
//! reusable buffers with `write!`, clear them per line, and never allocate a
//! `String` to hand to `out` (`mem-write-over-format`, `mem-reuse-collections`).
//!
//! # Value rendering
//!
//! * logfmt quoting: a value is wrapped in `"` when it contains a space, `=`,
//!   or a `"`. Empty values are quoted too, so `b=""` is visible rather than
//!   trailing off the line as hulog's `b=` did.
//! * `\n`, `\t` and ESC are escaped **always**, quoted or not. ESC in
//!   particular: a log line is attacker-controlled data, and an unescaped ESC
//!   lets it repaint the reader's terminal. Escaping is applied to keys, to the
//!   message and to an unparsed timestamp as well, because all three come from
//!   the same untrusted line.
//! * `{}`, `[]`, `null` and `""` are printed as themselves. hulog dropped
//!   `{"empty":{}}` entirely; silently losing a field is a bug, not brevity.
//! * duplicate keys are **all** printed, in document order. hulog collapsed
//!   them into a map and kept the last one.

pub(crate) mod flatten;
pub(crate) mod theme;
pub(crate) mod time;

use std::io::{self, Write};

pub use theme::ColorLevel;

use crate::error::Error;
use crate::settings::Settings;

use flatten::{Flattener, Parsed};
use theme::Theme;
use time::TimeFormatter;

/// Renders log lines. One per run; every buffer inside is reused across lines.
///
/// `&mut self` on [`Renderer::render`] is not incidental — it is what lets the
/// flattening arena, the quoting scratch and the timestamp layout cache survive
/// from one line to the next.
pub struct Renderer {
    settings: Settings,
    theme: Theme,
    time: TimeFormatter,
    /// Reusable key/value table; cleared at the start of every line.
    flat: Flattener,
    /// Reusable scratch for quoting and for the formatted timestamp.
    scratch: String,
}

impl Renderer {
    /// Builds a renderer.
    ///
    /// Fails on a bad `--ts-format` or an unknown time zone. Both are checked
    /// here, once, rather than per line.
    pub fn new(settings: Settings, color: ColorLevel) -> Result<Self, Error> {
        let time = TimeFormatter::new(&settings.time)?;
        Ok(Self {
            theme: Theme::new(color),
            time,
            settings,
            flat: Flattener::default(),
            scratch: String::new(),
        })
    }

    /// Renders one line of already-validated UTF-8 into `out`.
    ///
    /// `line` has no trailing newline; `render` writes one. This is the entry
    /// point tests and benches use, so its shape is fixed.
    ///
    /// The only errors are `out`'s. A malformed line is not an error — it is
    /// echoed (see the module docs).
    pub fn render(&mut self, line: &str, out: &mut impl Write) -> io::Result<()> {
        if self.flat.flatten(line, &self.settings.exclude) != Parsed::Object {
            return verbatim(out, line.as_bytes());
        }

        // Sort first: it moves pairs, so it would invalidate the indices
        // `take_first` hands back. Nothing else here depends on the order.
        if self.settings.sort_keys {
            self.flat.sort();
        }

        // The timestamp is claimed even when the column is switched off, so
        // `time_format = "none"` drops it instead of demoting it to `ts=…` in
        // the tail — "drop the column" is what the setting says.
        let ts = self.flat.take_first(&self.settings.fields.ts);
        let level = self.flat.take_first(&self.settings.fields.level);
        let msg = self.flat.take_first(&self.settings.fields.msg);

        // Tracks whether a separator is owed, so an absent part costs no space.
        // "Absent" means *printed nothing*, not "the field was missing":
        // `{"ts":"","msg":""}` has both fields and renders no characters for
        // either, and a separator owed to an invisible part is the very double
        // space this renderer exists not to emit.
        let mut written = false;

        if let Some(index) = ts {
            if self.time.is_enabled() {
                self.scratch.clear();
                self.time
                    .format(self.flat.value_at(index), &mut self.scratch);
                if !self.scratch.is_empty() {
                    let style = self.theme.timestamp_style();
                    style.write_to(&mut *out)?;
                    // An unparsed timestamp reaches here verbatim, so it is as
                    // untrusted as any other value.
                    escaped(out, &self.scratch, false)?;
                    style.write_reset_to(&mut *out)?;
                    written = true;
                }
            }
        }

        if let Some(index) = level {
            if written {
                out.write_all(b" ")?;
            }
            // `[output.levels]` first, then the theme's own table: pino and
            // bunyan send `30`/`50`, and the config is where a producer's
            // spelling is mapped onto a level hog knows. An empty table — the
            // overwhelmingly common case — costs one `is_empty` check.
            let raw = self.settings.levels.resolve(self.flat.value_at(index));
            let level = self.theme.level(raw);
            level.style.write_to(&mut *out)?;
            out.write_all(b"[")?;
            match level.tag {
                Some(tag) => out.write_all(tag.as_bytes())?,
                // An unknown level keeps its own text, upper-cased in place.
                None => upper_escaped(out, raw)?,
            }
            out.write_all(b"]")?;
            level.style.write_reset_to(&mut *out)?;
            written = true;
        }

        if let Some(index) = msg {
            let text = self.flat.value_at(index);
            if !text.is_empty() {
                if written {
                    out.write_all(b" ")?;
                }
                let style = self.theme.message_style();
                style.write_to(&mut *out)?;
                // Not logfmt-quoted: a message is prose and holds spaces by
                // nature. Control characters are still neutralised.
                escaped(out, text, false)?;
                style.write_reset_to(&mut *out)?;
                written = true;
            }
        }

        for index in 0..self.flat.len() {
            let Some(entry) = self.flat.entry(index) else {
                continue;
            };
            if written {
                out.write_all(b" ")?;
            }
            written = true;

            let style = self.theme.key_style(entry.key);
            style.write_to(&mut *out)?;
            field(out, entry.key)?;
            style.write_reset_to(&mut *out)?;
            out.write_all(b"=")?;
            field(out, entry.value)?;
        }

        out.write_all(b"\n")
    }

    /// Renders one line of raw bytes into `out`. This is what the pipeline calls.
    ///
    /// Valid UTF-8 goes to [`Renderer::render`]. Anything else is written
    /// verbatim, followed by a newline: a log line is not required to be UTF-8,
    /// and mangling it with a lossy conversion would destroy the bytes the user
    /// is most likely trying to look at. A single bad byte therefore costs the
    /// formatting of that one line and nothing else.
    pub fn render_bytes(&mut self, line: &[u8], out: &mut impl Write) -> io::Result<()> {
        match std::str::from_utf8(line) {
            Ok(text) => self.render(text, out),
            Err(_) => verbatim(out, line),
        }
    }
}

/// Echoes a line we could not or should not format, plus its newline.
fn verbatim(out: &mut impl Write, line: &[u8]) -> io::Result<()> {
    out.write_all(line)?;
    out.write_all(b"\n")
}

/// Writes a key or a value with logfmt quoting.
///
/// Quoted only when it has to be: a bare `port=8080` stays readable, while
/// `msg="a b"` stays machine-parseable. An empty text is quoted so the field
/// does not look truncated.
fn field(out: &mut impl Write, text: &str) -> io::Result<()> {
    if needs_quoting(text) {
        out.write_all(b"\"")?;
        escaped(out, text, true)?;
        out.write_all(b"\"")
    } else {
        out.write_all(text.as_bytes())
    }
}

/// A text needs quotes if reading it back unquoted would be ambiguous: any
/// whitespace or control byte, the `=` that separates a pair, the `"` that
/// would otherwise look like a quote, or nothing at all.
///
/// A lone backslash does **not** force quoting: unquoted text carries no escape
/// syntax, so `path=C:\tmp` is unambiguous as written. Inside quotes, where a
/// backslash does mean something, it is doubled.
fn needs_quoting(text: &str) -> bool {
    text.is_empty()
        || text
            .bytes()
            .any(|byte| byte <= b' ' || byte == b'=' || byte == b'"' || byte == 0x7f)
}

/// Writes `text`, escaping every control character; inside quotes, also `"` and
/// `\`.
///
/// Runs between escapes are written in one go, so the common case — no escape
/// at all — is a single `write_all` of a borrowed slice.
fn escaped(out: &mut impl Write, text: &str, quoted: bool) -> io::Result<()> {
    let bytes = text.as_bytes();
    let mut copied = 0usize;
    for (index, &byte) in bytes.iter().enumerate() {
        let escape: &[u8] = match byte {
            b'\n' => br"\n",
            b'\t' => br"\t",
            b'\r' => br"\r",
            b'"' if quoted => br#"\""#,
            b'\\' if quoted => br"\\",
            // ESC lands here: a log line must not be able to repaint the
            // reader's terminal, whatever the producer put in it.
            _ if byte < 0x20 || byte == 0x7f => {
                out.write_all(&bytes[copied..index])?;
                out.write_all(&hex_escape(byte))?;
                copied = index + 1;
                continue;
            }
            _ => continue,
        };
        out.write_all(&bytes[copied..index])?;
        out.write_all(escape)?;
        copied = index + 1;
    }
    out.write_all(&bytes[copied..])
}

/// Writes `text` upper-cased, escaping control characters. Used for the tag of
/// a level `hog` does not know, which keeps its own spelling.
///
/// No allocation: ASCII is upper-cased byte-wise and anything else goes through
/// [`char::to_uppercase`] into a four-byte stack buffer.
fn upper_escaped(out: &mut impl Write, text: &str) -> io::Result<()> {
    let mut buf = [0u8; 4];
    for ch in text.chars() {
        if ch.is_ascii() {
            let byte = (ch as u8).to_ascii_uppercase();
            match byte {
                b'\n' => out.write_all(br"\n")?,
                b'\t' => out.write_all(br"\t")?,
                b'\r' => out.write_all(br"\r")?,
                _ if byte < 0x20 || byte == 0x7f => out.write_all(&hex_escape(byte))?,
                _ => out.write_all(&[byte])?,
            }
        } else {
            for upper in ch.to_uppercase() {
                out.write_all(upper.encode_utf8(&mut buf).as_bytes())?;
            }
        }
    }
    Ok(())
}

/// `\x1b` for ESC, and so on. Two hex digits always suffice: only bytes below
/// 0x20 and 0x7f get here.
fn hex_escape(byte: u8) -> [u8; 4] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [
        b'\\',
        b'x',
        HEX[usize::from(byte >> 4)],
        HEX[usize::from(byte & 0x0f)],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ExcludeSet, TimeFormat, TimeZoneSpec};

    /// Settings with the timestamp left alone, so these tests assert on
    /// rendering rather than on the machine's time zone.
    fn settings() -> Settings {
        Settings {
            time: crate::settings::TimeSettings {
                format: TimeFormat::Raw,
                zone: TimeZoneSpec::Utc,
            },
            ..Settings::default()
        }
    }

    /// Renders one line and strips the styling, so an assertion reads as the
    /// user would see it. Colour itself is `theme.rs`'s contract, not this
    /// module's.
    fn render_with(settings: Settings, line: &str) -> String {
        let mut renderer = Renderer::new(settings, ColorLevel::None).expect("settings are valid");
        let mut out = Vec::new();
        renderer.render(line, &mut out).expect("a Vec never fails");
        let text = String::from_utf8(out).expect("output stays UTF-8");
        anstream::adapter::strip_str(&text).to_string()
    }

    fn render(line: &str) -> String {
        render_with(settings(), line)
    }

    fn render_excluding(line: &str, exclude: &[&str]) -> String {
        let mut settings = settings();
        settings.exclude = ExcludeSet::new(exclude.iter().map(|path| (*path).to_owned()));
        render_with(settings, line)
    }

    // ------------------------------------------------------------- shape

    #[test]
    fn the_reference_line_from_the_hld() {
        assert_eq!(
            render(
                r#"{"ts":"2025-06-15T10:32:01Z","level":"info","msg":"server started","port":8080,"grpc":{"code":"OK","time_ms":1.5}}"#
            ),
            "2025-06-15T10:32:01Z [INF] server started grpc.code=OK grpc.time_ms=1.5 port=8080\n"
        );
    }

    #[test]
    fn every_column_is_optional() {
        assert_eq!(render(r#"{"msg":"hi"}"#), "hi\n");
        assert_eq!(render(r#"{"level":"warn"}"#), "[WRN]\n");
        assert_eq!(render(r#"{"a":1}"#), "a=1\n");
        assert_eq!(render(r#"{"level":"error","a":1}"#), "[ERR] a=1\n");
    }

    #[test]
    fn a_missing_message_does_not_leave_a_double_space() {
        // hulog printed `10:32:01 [INF]  port=8080` here.
        assert_eq!(
            render(r#"{"ts":"10:32:01","level":"info","port":8080}"#),
            "10:32:01 [INF] port=8080\n"
        );
    }

    /// The same rule, for a column that is *present* but renders nothing.
    /// A field holding `""` is not a reason to print a space.
    #[test]
    fn an_empty_column_does_not_leave_a_space_behind() {
        assert_eq!(render(r#"{"ts":"","msg":"hi"}"#), "hi\n");
        assert_eq!(render(r#"{"level":"info","msg":""}"#), "[INF]\n");
        assert_eq!(render(r#"{"ts":"","msg":"","port":8080}"#), "port=8080\n");
        assert_eq!(render(r#"{"ts":"","msg":""}"#), "\n");
        // A one-space message is data, and it keeps its separator.
        assert_eq!(render(r#"{"level":"info","msg":" "}"#), "[INF]  \n");
        // An empty value in the tail still prints: the key makes it visible.
        assert_eq!(render(r#"{"level":"info","note":""}"#), "[INF] note=\"\"\n");
    }

    #[test]
    fn one_input_line_is_exactly_one_output_line() {
        for line in [
            r#"{"msg":"a\nb"}"#,
            r#"{"a":"x\ny"}"#,
            "not json",
            "{}",
            r#"{"a":1}"#,
        ] {
            let out = render(line);
            assert_eq!(out.matches('\n').count(), 1, "line: {line:?} -> {out:?}");
            assert!(out.ends_with('\n'), "line: {line:?}");
        }
    }

    // ------------------------------------------------------------- columns

    #[test]
    fn the_first_present_candidate_wins_and_the_loser_stays_in_the_tail() {
        assert_eq!(render(r#"{"time":"B","ts":"A","msg":"m"}"#), "A m time=B\n");
    }

    #[test]
    fn a_later_candidate_is_used_when_the_first_is_absent() {
        assert_eq!(
            render(r#"{"severity":"error","message":"boom"}"#),
            "[ERR] boom\n"
        );
    }

    #[test]
    fn a_nested_field_never_becomes_a_column() {
        assert_eq!(render(r#"{"a":{"msg":"nested"}}"#), "a.msg=nested\n");
    }

    #[test]
    fn an_unknown_level_keeps_its_own_text_upper_cased() {
        assert_eq!(render(r#"{"level":"weird","msg":"hi"}"#), "[WEIRD] hi\n");
        assert_eq!(render(r#"{"level":"30","msg":"hi"}"#), "[30] hi\n");
    }

    /// `[output.levels]`: the config maps a producer's own spelling onto a
    /// level hog knows, in front of the theme's table. pino and bunyan send
    /// numbers, which is the whole reason the key exists (HLD §3).
    #[test]
    fn configured_level_aliases_are_applied_before_the_level_table() {
        let mut settings = settings();
        settings.levels = crate::settings::LevelAliases::new([("30", "info"), ("50", "error")]);

        assert_eq!(
            render_with(settings.clone(), r#"{"level":"30"}"#),
            "[INF]\n"
        );
        assert_eq!(
            render_with(settings.clone(), r#"{"level":"50"}"#),
            "[ERR]\n"
        );
        // A value no alias covers is untouched, and so is a real level name.
        assert_eq!(render_with(settings.clone(), r#"{"level":"40"}"#), "[40]\n");
        assert_eq!(render_with(settings, r#"{"level":"warn"}"#), "[WRN]\n");
    }

    /// An alias pointing at a name hog does not know cannot invent a tag: the
    /// aliased spelling is what prints, upper-cased like any unknown level.
    #[test]
    fn an_alias_onto_an_unknown_level_prints_the_alias() {
        let mut settings = settings();
        settings.levels = crate::settings::LevelAliases::new([("30", "chatty")]);
        assert_eq!(render_with(settings, r#"{"level":"30"}"#), "[CHATTY]\n");
    }

    #[test]
    fn level_matching_ignores_case() {
        assert_eq!(render(r#"{"level":"INFO"}"#), "[INF]\n");
        assert_eq!(render(r#"{"level":"Warning"}"#), "[WRN]\n");
    }

    #[test]
    fn time_format_none_drops_the_column_without_leaving_the_field_behind() {
        let mut settings = settings();
        settings.time.format = TimeFormat::Hidden;
        assert_eq!(
            render_with(
                settings,
                r#"{"ts":"2025-06-15T10:32:01Z","msg":"hi","a":1}"#
            ),
            "hi a=1\n"
        );
    }

    #[test]
    fn a_strftime_format_reformats_the_timestamp() {
        let mut settings = settings();
        settings.time.format = TimeFormat::Strftime("%H:%M:%S".to_owned());
        assert_eq!(
            render_with(settings, r#"{"ts":"2025-06-15T10:32:01Z","msg":"hi"}"#),
            "10:32:01 hi\n"
        );
    }

    #[test]
    fn an_unparsable_timestamp_is_printed_as_it_came() {
        let mut settings = settings();
        settings.time.format = TimeFormat::Strftime("%H:%M:%S".to_owned());
        assert_eq!(
            render_with(settings, r#"{"ts":"nonsense","msg":"hi"}"#),
            "nonsense hi\n"
        );
    }

    // ---------------------------------------------------------------- tail

    #[test]
    fn the_tail_is_sorted_by_full_dotted_path() {
        assert_eq!(
            render(r#"{"z":1,"grpc":{"time_ms":2,"code":3},"a":4}"#),
            "a=4 grpc.code=3 grpc.time_ms=2 z=1\n"
        );
    }

    #[test]
    fn sort_keys_false_keeps_the_order_of_the_json() {
        let mut settings = settings();
        settings.sort_keys = false;
        assert_eq!(
            render_with(settings, r#"{"z":1,"grpc":{"time_ms":2,"code":3},"a":4}"#),
            "z=1 grpc.time_ms=2 grpc.code=3 a=4\n"
        );
    }

    #[test]
    fn duplicate_keys_are_all_printed() {
        assert_eq!(render(r#"{"a":1,"a":2}"#), "a=1 a=2\n");
    }

    #[test]
    fn exclusion_prunes_the_subtree_but_spares_a_look_alike_key() {
        assert_eq!(
            render_excluding(
                r#"{"msg":"hi","grpc":{"code":"OK","request":{"deadline":"1s"}},"grpcStatus":2}"#,
                &["grpc"],
            ),
            "hi grpcStatus=2\n"
        );
    }

    // -------------------------------------------------- quoting & escaping

    #[test]
    fn values_are_quoted_only_when_they_have_to_be() {
        assert_eq!(
            render(r#"{"plain":"abc","wide":"a b","eq":"x=y","quote":"a\"b"}"#),
            "eq=\"x=y\" plain=abc quote=\"a\\\"b\" wide=\"a b\"\n"
        );
    }

    #[test]
    fn a_backslash_alone_does_not_force_quotes_but_is_doubled_inside_them() {
        assert_eq!(render(r#"{"p":"C:\\tmp"}"#), "p=C:\\tmp\n");
        assert_eq!(render(r#"{"p":"C:\\my dir"}"#), "p=\"C:\\\\my dir\"\n");
    }

    #[test]
    fn nothing_disappears() {
        // hulog rendered this as ` a= b= c=[]` — `empty` gone, `null` and the
        // empty string indistinguishable from a missing value.
        assert_eq!(
            render(r#"{"empty":{},"a":null,"b":"","c":[],"d":[1,2]}"#),
            "a=null b=\"\" c=[] d=[1,2] empty={}\n"
        );
    }

    #[test]
    fn a_key_that_needs_quoting_gets_it() {
        assert_eq!(
            render(r#"{"two words":1,"":2}"#),
            "\"\"=2 \"two words\"=1\n"
        );
    }

    #[test]
    fn newlines_and_tabs_in_a_value_are_escaped_not_emitted() {
        assert_eq!(
            render(r#"{"a":"x\ny","b":"x\ty"}"#),
            "a=\"x\\ny\" b=\"x\\ty\"\n"
        );
    }

    #[test]
    fn an_escape_sequence_in_the_data_cannot_reach_the_terminal() {
        // The whole point: a raw ESC would otherwise repaint everything the
        // reader sees from here on.
        let out = render(r#"{"msg":"boom","evil":"\u001b[31mRED"}"#);
        assert_eq!(out, "boom evil=\"\\x1b[31mRED\"\n");
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn an_escape_sequence_in_a_key_or_a_message_is_escaped_too() {
        let out = render(r#"{"msg":"a\u001bb","\u001b[31m":1,"level":"\u001bx"}"#);
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert_eq!(out, "[\\x1bX] a\\x1bb \"\\x1b[31m\"=1\n");
    }

    #[test]
    fn other_control_characters_are_escaped_as_hex() {
        assert_eq!(render(r#"{"a":"x\u0000y\u0007"}"#), "a=\"x\\x00y\\x07\"\n");
    }

    // --------------------------------------------------------- pass-through

    #[test]
    fn a_line_that_is_not_a_json_object_is_echoed_untouched() {
        for line in [
            "plain text",
            "goroutine 1 [running]:",
            "  indented, not json",
            "[1,2]",
            r#""just a string""#,
            "42",
            r#"{"a":1"#,
            "",
        ] {
            assert_eq!(render(line), format!("{line}\n"), "line: {line:?}");
        }
    }

    #[test]
    fn an_empty_object_is_echoed_rather_than_rendered_as_a_blank_line() {
        assert_eq!(render("{}"), "{}\n");
    }

    #[test]
    fn a_line_whose_every_field_is_excluded_renders_empty() {
        // Not pass-through: echoing here would show the very field the user
        // asked to hide.
        assert_eq!(render_excluding(r#"{"a":1}"#, &["a"]), "\n");
    }

    #[test]
    fn invalid_utf8_passes_through_byte_for_byte() {
        let mut renderer = Renderer::new(settings(), ColorLevel::None).expect("settings are valid");
        let mut out = Vec::new();
        let line = b"\xff\xfe not utf-8";
        renderer
            .render_bytes(line, &mut out)
            .expect("a Vec never fails");

        let mut expected = line.to_vec();
        expected.push(b'\n');
        assert_eq!(out, expected);
    }

    #[test]
    fn render_bytes_formats_valid_utf8_like_render_does() {
        let mut renderer = Renderer::new(settings(), ColorLevel::None).expect("settings are valid");
        let mut out = Vec::new();
        renderer
            .render_bytes(br#"{"msg":"hi","a":1}"#, &mut out)
            .expect("a Vec never fails");
        assert_eq!(
            anstream::adapter::strip_str(&String::from_utf8(out).expect("utf-8")).to_string(),
            "hi a=1\n"
        );
    }

    // --------------------------------------------------------------- reuse

    #[test]
    fn consecutive_lines_do_not_bleed_into_each_other() {
        let mut renderer = Renderer::new(settings(), ColorLevel::None).expect("settings are valid");
        let mut out = Vec::new();
        for line in [
            r#"{"ts":"T1","level":"info","msg":"first","a":1}"#,
            r#"{"b":2}"#,
            "plain",
            r#"{"level":"error","msg":"third"}"#,
        ] {
            renderer.render(line, &mut out).expect("a Vec never fails");
        }
        assert_eq!(
            anstream::adapter::strip_str(&String::from_utf8(out).expect("utf-8")).to_string(),
            "T1 [INF] first a=1\nb=2\nplain\n[ERR] third\n"
        );
    }

    // ------------------------------------------------------------- helpers

    #[test]
    fn quoting_predicate_covers_the_documented_triggers() {
        for text in [
            "", " ", "a b", "a=b", "a\"b", "a\nb", "a\u{1b}b", "a\u{7f}b",
        ] {
            assert!(needs_quoting(text), "should quote {text:?}");
        }
        for text in ["a", "8080", "[1,2]", "{}", "null", "C:\\tmp", "ключ"] {
            assert!(!needs_quoting(text), "should not quote {text:?}");
        }
    }

    #[test]
    fn hex_escapes_are_lower_case_and_two_digits() {
        assert_eq!(&hex_escape(0x1b), br"\x1b");
        assert_eq!(&hex_escape(0x00), br"\x00");
        assert_eq!(&hex_escape(0x7f), br"\x7f");
    }
}
