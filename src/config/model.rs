//! Serde types mirroring the TOML file, and the list of keys hog understands.
//!
//! Everything is `Option`-shaped and nothing carries a `#[serde(default)]`
//! value. That is deliberate: this is the *middle* layer of
//! `defaults < config file < CLI flags`, so it has to be able to say "the file
//! did not mention this" as distinct from "the file set it to the default".
//! A `serde(default)` here would quietly promote every missing key into a
//! decision that then outranks nothing — and would make the file's `color`
//! indistinguishable from the built-in one.
//!
//! `#[serde(deny_unknown_fields)]` is **not** used, which is the one deliberate
//! departure from the `serde-deny-unknown-fields` rule (HLD §7.2). Denying
//! would break forward compatibility: a config carrying a key from a newer hog
//! would stop working on an older one. Both halves of the benefit are kept
//! instead — typos are still caught, by [`crate::config::load::audit`], which
//! reports them as warnings with a line number.
//!
//! # Validation
//!
//! An unknown *key* is a warning; an unknown *value* under a known key is an
//! error. The two are not the same bet: a key hog has never heard of can only
//! have come from a newer version, while `color = "pink"` is a request hog
//! understands the shape of and cannot honour, and carrying on would silently
//! render the whole stream the wrong way.
//!
//! [`Model::validate`] runs that check, and [`load::parse`](super::load::parse)
//! calls it while the document still carries spans, so the failure arrives with
//! the line the value is written on. The two rules that already have a home in
//! `settings.rs` — `time_format` and `time_zone` — are checked *through*
//! [`TimeFormat::parse`] and [`TimeZoneSpec::parse`] rather than restated here,
//! so `--ts-format raw` and `time_format = "raw"` cannot drift apart.
//!
//! Those two parsers only judge hog's own vocabulary, though: whether
//! `Europe/Moscow` is in the tzdb and whether `%J` is a specifier are questions
//! only jiff can answer. So the check finishes in
//! [`render::time::check`](crate::render::time::check), which is
//! `TimeFormatter::new` without the formatter. Asking here rather than letting
//! `Renderer::new` fail is the whole point — from the renderer the same value
//! produces `error: unknown time zone "Mars/Olympus"` and names neither the
//! file nor the line, and `hog config` would have called that file fine.

use std::collections::BTreeMap;
use std::fmt;
use std::slice;

use serde::Deserialize;

use crate::cli::ColorChoiceArg;
// The one call out of the config layer, and it is deliberate: jiff still lives
// in exactly one module (HLD §2), and this is how a value only jiff can judge
// gets reported with the line it is written on instead of as a bare start-up
// failure. See [`Output::validate`].
use crate::render::time;
use crate::settings::{TimeFormat, TimeSettings, TimeZoneSpec};

/// The config file as hog understands it.
///
/// Turning this into a base `Settings` is `settings.rs`'s job, not this
/// module's: the layering rules live in exactly one place. What this module
/// owes it is the parsed shape and the validation vocabulary —
/// `TimeFormat::parse` and `TimeZoneSpec::parse` already exist there and are
/// what `output.time_format` / `output.time_zone` must go through.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Model {
    /// Dotted paths hidden from the output; the base the CLI's `-e` adds to.
    pub exclude: Option<Vec<String>>,

    /// The command that produces the log stream.
    ///
    /// Split into an argv by shell rules and run **without** a local shell
    /// (HLD §5). It is a **top-level** key, so in the file it has to sit above the first
    /// `[table]` header — a bare key written after `[output]` would land as
    /// `output.command`.
    pub command: Option<String>,

    /// `[fields]` — which JSON keys carry the three special columns.
    pub fields: Option<Fields>,

    /// `[output]` — how the rendered line looks.
    pub output: Option<Output>,
}

impl Model {
    /// Checks every value hog will act on, stopping at the first bad one.
    ///
    /// # Errors
    ///
    /// [`InvalidValue`] naming the dotted key, so the caller can turn it into a
    /// line number. Only values are checked here — an unknown *key* is
    /// [`load::audit`](super::load::audit)'s business and only ever a warning.
    pub fn validate(&self) -> Result<(), InvalidValue> {
        match &self.output {
            Some(output) => output.validate(),
            None => Ok(()),
        }
    }
}

/// A value the config file sets that hog cannot honour.
///
/// Carries the dotted key rather than a line number: this type is produced by
/// the serde model, which has no idea where anything was written.
/// [`load::parse`](super::load::parse) looks the key up in the still-spanned
/// document and adds the position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidValue {
    /// The dotted key the bad value sits under, e.g. `output.color`. Always one
    /// of [`KNOWN_KEYS`] — hog only validates what it understands.
    pub key: &'static str,
    /// What is wrong, phrased for the user and already lowercase so it composes
    /// into `path:line: <message>`.
    pub message: String,
}

impl InvalidValue {
    fn new(key: &'static str, message: impl fmt::Display) -> Self {
        Self {
            key,
            message: message.to_string(),
        }
    }
}

impl fmt::Display for InvalidValue {
    /// Just the message: the key is already named in it or implied by the line
    /// number the caller prints.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// `[fields]`: candidate key names for the timestamp, level and message
/// columns.
///
/// Each entry is a **list of candidates**, and the first one present in the
/// line wins — decided separately for every line, because one stream can carry
/// two producers. A candidate that loses is not dropped: it prints in the tail
/// like any other key.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Fields {
    pub ts: Option<Candidates>,
    pub level: Option<Candidates>,
    pub msg: Option<Candidates>,
}

/// One name or a list of them: `ts = "ts"` and `ts = ["ts", "time"]` both
/// parse.
///
/// The bare-string form is accepted because it is what people write first, and
/// rejecting it would fail the file over punctuation. `#[serde(untagged)]` is
/// safe here in a way it usually is not — the two variants are different TOML
/// types, so the match is unambiguous and the error message cannot degrade into
/// serde's "data did not match any variant".
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Candidates {
    /// `ts = "ts"`
    One(String),
    /// `ts = ["ts", "time"]`
    Many(Vec<String>),
}

impl Candidates {
    /// The candidates in priority order.
    ///
    /// Borrowing rather than cloning: `settings::resolve` runs once per process
    /// and owns the result, so the copy belongs there, not here.
    pub fn as_slice(&self) -> &[String] {
        match self {
            // The single name is already a `String` sitting in this enum, so
            // the one-element slice is a borrow and not an allocation.
            Self::One(name) => slice::from_ref(name),
            Self::Many(names) => names,
        }
    }

    /// The candidates as an owned list, ready to move into `Settings`.
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(name) => vec![name],
            Self::Many(names) => names,
        }
    }
}

/// `[output]`: how the rendered line looks.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Output {
    /// A jiff strftime pattern, or one of the two reserved words `raw` / `none`.
    ///
    /// Kept as a `String` and validated by `settings::TimeFormat::parse`, so
    /// the rule that "a value without `%` must be `raw` or `none`" is stated
    /// once and applies to `--ts-format` and the file alike. A typo such as
    /// `HH:MM:SS` is a config error, not a literal printed once per line.
    pub time_format: Option<String>,

    /// `"local"`, `"utc"`, or an IANA name such as `Europe/Moscow`. Validated
    /// by `settings::TimeZoneSpec::parse`; an unknown IANA name only surfaces
    /// when `render::time` asks the tzdb for it.
    pub time_zone: Option<String>,

    /// `"auto"` | `"always"` | `"never"`.
    ///
    /// A `String` rather than `cli::ColorChoiceArg`: deriving `Deserialize` on
    /// the clap enum would put a serde dependency on `cli.rs`, which is meant
    /// to be declaration-only, and the file's spelling is not required to match
    /// clap's. [`parse_color`] maps it, and [`Output::validate`] rejects
    /// anything else.
    ///
    /// Note the layering trap this key carries: `--color` has a clap default,
    /// so "not given" and "given as `auto`" are the same value on the CLI side.
    /// `resolve` has to consult `ArgMatches::value_source` before letting the
    /// flag win over this key, or the file could never change the colour.
    pub color: Option<String>,

    /// `false` keeps the key order from the JSON instead of sorting the tail.
    pub sort_keys: Option<bool>,

    /// Ten-ish `#rrggbb` strings replacing the built-in key palette.
    ///
    /// Parsed here so that uncommenting the block the starter file documents
    /// does not produce an "unknown key" warning. Applying it is v2 (HLD §8);
    /// until then the value is accepted, validated, and unused.
    pub key_colors: Option<Vec<String>>,

    /// `[output.levels]` — raw level value → known level name, for producers
    /// that send numbers (`"30" = "info"`, pino and bunyan).
    ///
    /// A `BTreeMap` rather than a `HashMap`: the table is a handful of entries
    /// read once, and a deterministic order makes the diagnostics reproducible.
    /// The keys are **user data**, so [`OPEN_TABLES`] stops the unknown-key
    /// audit from walking into it.
    pub levels: Option<BTreeMap<String, String>>,
}

impl Output {
    /// Checks the four values that can be spelled wrong.
    ///
    /// `sort_keys` and `levels` are not here: TOML has already decided that a
    /// bool is a bool, and every string is a legal level name.
    ///
    /// # Errors
    ///
    /// The first [`InvalidValue`] found, in the order the keys are listed in
    /// the starter file. Reporting one at a time is deliberate — a config with
    /// two mistakes is fixed one line at a time anyway, and a list of failures
    /// would need a second diagnostic format nothing else in hog uses.
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if let Some(pattern) = &self.time_format {
            // Through `settings`, never re-implemented: `--ts-format` and this
            // key have to accept exactly the same strings.
            let format = TimeFormat::parse(pattern)
                .map_err(|err| InvalidValue::new(KEY_TIME_FORMAT, err))?;
            // And then through jiff, because `%J` is not a specifier and only
            // the formatter can say so. Paired with `utc` so that the failure
            // this reports can only be about the pattern.
            time::check(&TimeSettings {
                format,
                zone: TimeZoneSpec::Utc,
            })
            .map_err(|err| InvalidValue::new(KEY_TIME_FORMAT, err))?;
        }

        if let Some(spec) = &self.time_zone {
            // `TimeZoneSpec::parse` is infallible by design — anything that is
            // not `local` or `utc` is taken for an IANA name, and only the tzdb
            // can say otherwise. The empty name is worth its own message
            // because "unknown time zone \"\"" reads like a bug in hog.
            let zone = TimeZoneSpec::parse(spec);
            if matches!(&zone, TimeZoneSpec::Named(name) if name.trim().is_empty()) {
                return Err(InvalidValue::new(
                    KEY_TIME_ZONE,
                    format!(
                        "empty time zone {spec:?}: expected `local`, `utc`, or an IANA name like `Europe/Moscow`"
                    ),
                ));
            }
            // The tzdb lookup happens here rather than in `Renderer::new` so
            // that it arrives with the file and line it is written on. The
            // renderer still checks: `--timezone` has no line to point at.
            // Paired with `raw`, which skips the pattern check entirely.
            time::check(&TimeSettings {
                format: TimeFormat::Raw,
                zone,
            })
            .map_err(|err| InvalidValue::new(KEY_TIME_ZONE, err))?;
        }

        if let Some(color) = &self.color {
            // Not a let-chain: the package declares MSRV 1.87 and `let` chains
            // landed in 1.88 (`proj-msrv-declare`).
            if parse_color(color).is_none() {
                return Err(InvalidValue::new(
                    KEY_COLOR,
                    format!(
                        "invalid colour choice {color:?}: expected `auto`, `always` or `never`"
                    ),
                ));
            }
        }

        if let Some(colors) = &self.key_colors {
            // A key's colour is `FNV-1a(name) % len(palette)`, so an empty
            // palette is a division by zero waiting for v2 to apply it.
            if colors.is_empty() {
                return Err(InvalidValue::new(
                    KEY_KEY_COLORS,
                    "empty palette: a key's colour is chosen modulo the palette length, \
                     so it needs at least one colour",
                ));
            }
            if let Some(bad) = colors.iter().find(|color| !is_hex_color(color)) {
                return Err(InvalidValue::new(
                    KEY_KEY_COLORS,
                    format!("invalid colour {bad:?}: expected a hex colour like `#ff3366`"),
                ));
            }
        }

        Ok(())
    }
}

/// Dotted key of `[output]`'s timestamp format, as [`InvalidValue`] reports it.
pub const KEY_TIME_FORMAT: &str = "output.time_format";
/// Dotted key of `[output]`'s time zone.
pub const KEY_TIME_ZONE: &str = "output.time_zone";
/// Dotted key of `[output]`'s colour choice.
pub const KEY_COLOR: &str = "output.color";
/// Dotted key of `[output]`'s key palette.
pub const KEY_KEY_COLORS: &str = "output.key_colors";

/// Maps `output.color` onto the same enum `--color` parses into.
///
/// `None` for anything else, which [`Output::validate`] turns into a config
/// error. The comparison is ASCII-case-insensitive, matching `TimeFormat::parse`
/// and `TimeZoneSpec::parse`: a config file is hand-written, and `"Always"`
/// failing over one capital letter would be a bad joke.
pub fn parse_color(value: &str) -> Option<ColorChoiceArg> {
    if value.eq_ignore_ascii_case("auto") {
        Some(ColorChoiceArg::Auto)
    } else if value.eq_ignore_ascii_case("always") {
        Some(ColorChoiceArg::Always)
    } else if value.eq_ignore_ascii_case("never") {
        Some(ColorChoiceArg::Never)
    } else {
        None
    }
}

/// Is this a `#rrggbb` colour?
///
/// Exactly the form the starter file documents: a `#` and six hex digits. The
/// three-digit CSS shorthand is **not** accepted — taking it here and choking on
/// it in v2, when the palette is finally applied, would move the error a year
/// away from the edit that caused it.
pub fn is_hex_color(value: &str) -> bool {
    match value.strip_prefix('#') {
        Some(digits) => digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

/// Every key this version of hog understands, as a dotted path.
///
/// This is the schema the unknown-key audit checks against, and it is the one
/// place to update when a key is added — the serde types above cannot be
/// enumerated at runtime.
///
/// Table headers (`fields`, `output`) are listed too: a `[fields]` line is a
/// key in its own right as far as the walk is concerned.
pub const KNOWN_KEYS: &[&str] = &[
    "exclude",
    "command",
    "fields",
    "fields.ts",
    "fields.level",
    "fields.msg",
    "output",
    "output.time_format",
    "output.time_zone",
    "output.color",
    "output.sort_keys",
    "output.key_colors",
    "output.levels",
];

/// Tables whose *keys* are user data rather than schema.
///
/// The audit stops at these instead of reporting every entry inside them as
/// unknown — `[output.levels]` holds whatever level names the producer emits.
pub const OPEN_TABLES: &[&str] = &["output.levels"];

/// Does hog know this dotted key path?
///
/// `true` for anything in [`KNOWN_KEYS`] and for anything **under** an entry of
/// [`OPEN_TABLES`]; `false` otherwise, which is what earns a warning.
///
/// "Under" means *on a segment boundary*, the same rule the exclude list uses:
/// `output.levels.30` is inside the open table, `output.levelsets` is a
/// different key that happens to start with the same letters.
pub fn is_known(dotted_path: &str) -> bool {
    KNOWN_KEYS.contains(&dotted_path)
        || OPEN_TABLES.iter().any(|table| is_under(table, dotted_path))
}

/// Is `path` a key nested inside the table `table`?
fn is_under(table: &str, path: &str) -> bool {
    path.strip_prefix(table)
        .is_some_and(|rest| rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses TOML into the model the way [`load`](super::super::load) does.
    fn model(text: &str) -> Model {
        toml_edit::de::from_str(text).expect("test config must deserialize")
    }

    fn output(text: &str) -> Output {
        model(text).output.expect("test config must have [output]")
    }

    #[test]
    fn an_empty_file_sets_nothing() {
        let model = model("");
        assert!(model.exclude.is_none());
        assert!(model.command.is_none());
        assert!(model.fields.is_none());
        assert!(model.output.is_none());
    }

    // The reason `deny_unknown_fields` is off: a config written for a newer hog
    // still has to load here (HLD §7.2).
    #[test]
    fn an_unknown_key_does_not_fail_deserialization() {
        let model = model("from_the_future = 1\n[output]\ntime_fmt = \"%H\"\n");
        assert!(model.output.is_some());
    }

    #[test]
    fn candidates_accept_a_bare_string_and_a_list() {
        let fields = model("[fields]\nts = \"at\"\nlevel = [\"lvl\", \"severity\"]\n")
            .fields
            .expect("[fields] must parse");
        let ts = fields.ts.expect("ts must parse");
        let level = fields.level.expect("level must parse");
        assert_eq!(ts.as_slice(), ["at"]);
        assert_eq!(level.as_slice(), ["lvl", "severity"]);
        assert!(fields.msg.is_none(), "an absent key stays absent");
    }

    #[test]
    fn candidates_convert_to_an_owned_list() {
        assert_eq!(Candidates::One("at".to_owned()).into_vec(), ["at"]);
        assert_eq!(
            Candidates::Many(vec!["a".to_owned(), "b".to_owned()]).into_vec(),
            ["a", "b"]
        );
    }

    #[test]
    fn candidates_borrow_rather_than_allocate() {
        let one = Candidates::One("at".to_owned());
        let slice = one.as_slice();
        assert_eq!(slice.len(), 1);
        // The borrow points into the enum itself, which is the whole point of
        // `slice::from_ref` over `vec![name.clone()]`.
        let Candidates::One(name) = &one else {
            unreachable!("constructed as One")
        };
        assert!(std::ptr::eq(
            std::ptr::from_ref(&slice[0]),
            std::ptr::from_ref(name)
        ));
    }

    #[test]
    fn exclude_and_command_are_top_level() {
        let model = model("exclude = [\"a\", \"b\"]\ncommand = \"ssh {0}\"\n");
        assert_eq!(
            model.exclude.as_deref(),
            Some(["a".to_owned(), "b".to_owned()].as_slice())
        );
        assert_eq!(model.command.as_deref(), Some("ssh {0}"));
    }

    #[test]
    fn levels_is_read_as_a_map() {
        let levels = output("[output.levels]\n\"30\" = \"info\"\n\"50\" = \"error\"\n")
            .levels
            .expect("[output.levels] must parse");
        assert_eq!(levels.get("30").map(String::as_str), Some("info"));
        assert_eq!(levels.get("50").map(String::as_str), Some("error"));
    }

    // ---------------------------------------------------------------- is_known

    #[test]
    fn every_known_key_is_known() {
        for key in KNOWN_KEYS {
            assert!(is_known(key), "{key} should be known");
        }
    }

    #[test]
    fn every_open_table_is_also_a_known_key() {
        for table in OPEN_TABLES {
            assert!(
                KNOWN_KEYS.contains(table),
                "{table} is open but not listed as a key, so its header would warn"
            );
        }
    }

    #[test]
    fn keys_inside_an_open_table_are_known() {
        assert!(is_known("output.levels.30"));
        assert!(is_known("output.levels.TRACE"));
        assert!(is_known("output.levels.a.b"));
    }

    #[test]
    fn a_typo_is_not_known() {
        assert!(!is_known("output.time_fmt"));
        assert!(!is_known("outpout"));
        assert!(!is_known("time_format"), "no table prefix, no match");
        assert!(!is_known(""));
    }

    // The segment-boundary rule, and the reason `starts_with` alone is wrong.
    #[test]
    fn an_open_table_matches_on_segment_boundaries() {
        assert!(!is_known("output.levelsets"));
        assert!(!is_known("output.levels_extra"));
    }

    #[test]
    fn every_validated_key_is_a_known_key() {
        for key in [KEY_TIME_FORMAT, KEY_TIME_ZONE, KEY_COLOR, KEY_KEY_COLORS] {
            assert!(KNOWN_KEYS.contains(&key), "{key} is validated but unknown");
        }
    }

    // -------------------------------------------------------------- validation

    fn invalid(text: &str) -> InvalidValue {
        model(text).validate().expect_err("value must be rejected")
    }

    fn valid(text: &str) {
        model(text).validate().expect("value must be accepted");
    }

    #[test]
    fn a_model_without_output_validates() {
        valid("");
        valid("exclude = [\"a\"]\n[fields]\nts = \"at\"\n");
    }

    #[test]
    fn the_documented_output_values_validate() {
        valid(
            "[output]\n\
             time_format = \"%H:%M:%S\"\n\
             time_zone = \"local\"\n\
             color = \"auto\"\n\
             sort_keys = true\n",
        );
        valid("[output]\ntime_format = \"raw\"\ntime_zone = \"Europe/Moscow\"\n");
        valid("[output]\ntime_format = \"none\"\ntime_zone = \"utc\"\n");
        valid("[output]\ncolor = \"Always\"\n");
    }

    // The same rule as `--ts-format`, stated once in `settings::TimeFormat`.
    #[test]
    fn a_time_format_without_a_percent_must_be_raw_or_none() {
        let invalid = invalid("[output]\ntime_format = \"HH:MM:SS\"\n");
        assert_eq!(invalid.key, KEY_TIME_FORMAT);
        assert!(
            invalid.message.contains("HH:MM:SS"),
            "the message must quote the value: {invalid}"
        );
    }

    #[test]
    fn an_empty_time_zone_is_rejected() {
        let invalid = invalid("[output]\ntime_zone = \"\"\n");
        assert_eq!(invalid.key, KEY_TIME_ZONE);
        assert!(invalid.message.contains("Europe/Moscow"), "{invalid}");
    }

    /// The tzdb has the last word on an IANA name, and the config layer has to
    /// ask it: otherwise `hog config` calls the file fine and the next run
    /// fails with an error that names no file at all.
    #[test]
    fn a_zone_the_tzdb_does_not_have_is_rejected() {
        let invalid = invalid("[output]\ntime_zone = \"Mars/Olympus\"\n");
        assert_eq!(invalid.key, KEY_TIME_ZONE);
        assert!(invalid.message.contains("Mars/Olympus"), "{invalid}");
    }

    /// Same for a pattern that has a `%` in it and still means nothing: `%J` is
    /// not a strftime specifier, so `TimeFormat::parse` waves it through and
    /// only jiff can turn it down.
    #[test]
    fn a_strftime_pattern_jiff_cannot_use_is_rejected() {
        let invalid = invalid("[output]\ntime_format = \"%J\"\n");
        assert_eq!(invalid.key, KEY_TIME_FORMAT);
        assert!(invalid.message.contains("%J"), "{invalid}");
    }

    /// The pattern check must not drag the zone in with it: a file with a
    /// perfectly good pattern and a broken zone has to blame the zone.
    #[test]
    fn a_broken_zone_does_not_get_reported_as_a_broken_pattern() {
        let invalid = invalid("[output]\ntime_format = \"%H:%M\"\ntime_zone = \"Mars/Olympus\"\n");
        assert_eq!(invalid.key, KEY_TIME_ZONE);
    }

    #[test]
    fn an_unknown_colour_choice_is_rejected() {
        let invalid = invalid("[output]\ncolor = \"pink\"\n");
        assert_eq!(invalid.key, KEY_COLOR);
        assert!(invalid.message.contains("pink"), "{invalid}");
    }

    #[test]
    fn an_empty_palette_is_rejected() {
        // Not pedantry: v2 picks a colour with `% len(palette)`.
        let invalid = invalid("[output]\nkey_colors = []\n");
        assert_eq!(invalid.key, KEY_KEY_COLORS);
    }

    #[test]
    fn a_malformed_palette_entry_is_rejected() {
        let invalid = invalid("[output]\nkey_colors = [\"#ff3366\", \"red\"]\n");
        assert_eq!(invalid.key, KEY_KEY_COLORS);
        assert!(invalid.message.contains("red"), "{invalid}");
    }

    #[test]
    fn the_documented_palette_validates() {
        valid(
            "[output]\nkey_colors = [\"#ff3366\", \"#66cc66\", \"#ff9933\", \"#66ccff\", \
             \"#cc99ff\", \"#cc9966\", \"#669999\", \"#ff9999\", \"#99cc66\", \"#9999cc\"]\n",
        );
    }

    #[test]
    fn validation_stops_at_the_first_bad_value() {
        // time_format is checked before color, so a file with both mistakes
        // reports the first one in starter-file order.
        let invalid = invalid("[output]\ntime_format = \"nope\"\ncolor = \"pink\"\n");
        assert_eq!(invalid.key, KEY_TIME_FORMAT);
    }

    // ------------------------------------------------------------- value words

    #[test]
    fn colour_choices_map_onto_the_cli_enum() {
        assert_eq!(parse_color("auto"), Some(ColorChoiceArg::Auto));
        assert_eq!(parse_color("ALWAYS"), Some(ColorChoiceArg::Always));
        assert_eq!(parse_color("Never"), Some(ColorChoiceArg::Never));
        assert_eq!(parse_color("sometimes"), None);
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color(" auto"), None, "TOML values are not trimmed");
    }

    #[test]
    fn hex_colours_are_six_digits_after_a_hash() {
        assert!(is_hex_color("#ff3366"));
        assert!(is_hex_color("#FF3366"));
        assert!(is_hex_color("#000000"));
        assert!(!is_hex_color("ff3366"), "the hash is required");
        assert!(!is_hex_color("#f36"), "the CSS shorthand is not accepted");
        assert!(!is_hex_color("#ff33667"));
        assert!(!is_hex_color("#gggggg"));
        assert!(!is_hex_color("#"));
        assert!(!is_hex_color(""));
    }
}
