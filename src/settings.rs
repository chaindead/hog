//! Resolved runtime settings — the only shape the renderer ever sees.
//!
//! Layers, in order (HLD §3): built-in defaults < config file < CLI flags.
//! [`resolve`] is the single place where that folding happens: the config file
//! arrives as a [`Model`] and becomes the base through [`from_config`], and the
//! CLI is folded on top of it by [`resolve_onto`].
//!
//! Nothing here is `Option`-shaped **except** [`Settings::command`]: an
//! `Option` that survives into the renderer means the same defaulting decision
//! gets re-made once per log line. `command` is the exception because "there is
//! no command template" is a real state the run has to be able to report, not a
//! value with a sensible default.

use crate::cli::{ColorChoiceArg, RunArgs};
use crate::config::Model;
use crate::config::model::{self, Candidates};
use crate::error::Error;

/// Built-in defaults for the three special columns (HLD §3, `[fields]`).
pub const DEFAULT_TS_FIELDS: &[&str] = &["ts", "time", "timestamp", "@timestamp"];
pub const DEFAULT_LEVEL_FIELDS: &[&str] = &["level", "severity", "lvl"];
pub const DEFAULT_MSG_FIELDS: &[&str] = &["msg", "message"];
/// Built-in default for `[output].time_format`.
pub const DEFAULT_TIME_FORMAT: &str = "%H:%M:%S";

/// Everything the run needs, fully resolved.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Which JSON keys carry the timestamp / level / message columns.
    pub fields: FieldNames,
    /// How the timestamp column is parsed and printed.
    pub time: TimeSettings,
    /// Dotted paths pruned from the output, subtree and all.
    pub exclude: ExcludeSet,
    /// `[output.levels]` — a producer's own level spellings, mapped onto the
    /// ones hog knows.
    pub levels: LevelAliases,
    /// `true` sorts the tail alphabetically; `false` keeps JSON order.
    pub sort_keys: bool,
    /// `--color` as requested. The stream still consults `NO_COLOR` etc.
    pub color: ColorChoiceArg,
    /// The `command` template from the config file, if the file sets one.
    ///
    /// `None` no longer means "refuse": since HLD §5 a missing key runs the
    /// built-in `echo {@}` (`command::DEFAULT_COMMAND`). What the `Option`
    /// still decides is the **last row of the mode table** — a bare `hog` at a
    /// prompt with nothing configured gets the usage text instead of an empty
    /// `echo` — and the "(built in)" note in `--help` and `hog config`.
    ///
    /// The template is **not** validated here — splitting it and
    /// checking its `{N}` indices depends on the arguments of this particular
    /// invocation, so a config that only ever reads stdin is never refused for
    /// a template it does not use.
    pub command: Option<String>,
}

/// Candidate key names for each special column.
///
/// Each list is tried **in order**, and the first candidate present in *this*
/// line wins — the decision is per line, not per run, because one stream can
/// carry two producers. A candidate that loses stays in the tail: if a line has
/// both `ts` and `time`, `ts` becomes the column and `time` prints as `time=…`.
#[derive(Debug, Clone)]
pub struct FieldNames {
    pub ts: Vec<String>,
    pub level: Vec<String>,
    pub msg: Vec<String>,
}

/// How the timestamp column is produced.
#[derive(Debug, Clone)]
pub struct TimeSettings {
    pub format: TimeFormat,
    pub zone: TimeZoneSpec,
}

/// `[output].time_format`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeFormat {
    /// `"none"` — drop the timestamp column entirely.
    Hidden,
    /// `"raw"` — print the timestamp byte for byte, no parsing at all.
    Raw,
    /// A jiff strftime pattern such as `%H:%M:%S`.
    Strftime(String),
}

impl TimeFormat {
    /// Parses a `time_format` value.
    ///
    /// A value containing `%` is a strftime pattern. A value without one must
    /// be exactly `raw` or `none`; anything else is [`Error::BadTimeFormat`],
    /// so a typo like `HH:MM:SS` fails loudly instead of printing itself once
    /// per line.
    pub fn parse(value: &str) -> Result<Self, Error> {
        if value.contains('%') {
            return Ok(Self::Strftime(value.to_owned()));
        }
        if value.eq_ignore_ascii_case("raw") {
            return Ok(Self::Raw);
        }
        if value.eq_ignore_ascii_case("none") {
            return Ok(Self::Hidden);
        }
        Err(Error::BadTimeFormat(value.to_owned()))
    }
}

/// `[output].time_zone`. Kept symbolic on purpose: resolving it needs jiff, and
/// `render::time` is the only module allowed to mention jiff (HLD §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeZoneSpec {
    /// The machine's zone. The default, and a deliberate change from hulog,
    /// which kept the offset from the input string.
    Local,
    Utc,
    /// An IANA name such as `Europe/Moscow`.
    Named(String),
}

impl TimeZoneSpec {
    /// Parses a `time_zone` value. Never fails here — an unknown IANA name is
    /// only detected when `render::time` asks the tzdb for it.
    pub fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("local") {
            Self::Local
        } else if value.eq_ignore_ascii_case("utc") {
            Self::Utc
        } else {
            Self::Named(value.to_owned())
        }
    }
}

/// `[output.levels]`: raw level value → the level name hog should treat it as.
///
/// pino and bunyan send numbers (`30`, `50`), and hog's level table is written
/// in words, so without this every such line would print `[30]` unstyled. The
/// lookup happens **in front of** the theme's own table, so an alias can only
/// ever redirect a value — it cannot invent a tag hog does not have.
///
/// A `Vec` rather than a map: the table is a handful of entries read once per
/// line that has a level column at all, and for the overwhelmingly common case
/// of an empty table the scan costs nothing. Matching is ASCII-case-insensitive,
/// like the theme's own, so `"TRACE"` in the file matches `trace` in the log.
#[derive(Debug, Clone, Default)]
pub struct LevelAliases {
    entries: Vec<(Box<str>, Box<str>)>,
}

impl LevelAliases {
    /// Builds the table. Entries whose key or value is blank are dropped: they
    /// could never match anything, or would map a level onto nothing.
    pub fn new<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        Self {
            entries: entries
                .into_iter()
                .filter_map(|(raw, name)| {
                    let raw = raw.as_ref().trim();
                    let name = name.as_ref().trim();
                    if raw.is_empty() || name.is_empty() {
                        None
                    } else {
                        Some((Box::from(raw), Box::from(name)))
                    }
                })
                .collect(),
        }
    }

    /// The level name to use for `raw`, which is `raw` itself when no alias
    /// covers it. Borrowing rather than allocating: this runs per line.
    pub fn resolve<'a>(&'a self, raw: &'a str) -> &'a str {
        self.entries
            .iter()
            .find(|(from, _)| from.eq_ignore_ascii_case(raw))
            .map_or(raw, |(_, to)| &**to)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The pairs, in file order, for `hog config`'s summary.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(from, to)| (&**from, &**to))
    }
}

/// The set of dotted paths to hide.
///
/// Matching is **exact on the full dotted path**, checked before descending
/// into a node. That single rule produces the whole table in HLD §6: `grpc`
/// matches the node `grpc`, so its subtree is never expanded and `grpc.code`
/// disappears with it, while `grpcStatus` is a different node and survives.
/// Substring or prefix matching would break that last column.
///
/// Backed by a sorted `Vec` + binary search: the set is tiny (a dozen entries),
/// built once, and read once per JSON node.
#[derive(Debug, Clone, Default)]
pub struct ExcludeSet {
    paths: Vec<Box<str>>,
}

impl ExcludeSet {
    /// Builds the set, sorting and de-duplicating. Entries are trimmed;
    /// empty entries are dropped (`-e a,,b` is `-e a,b`).
    pub fn new<I>(paths: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let mut paths: Vec<Box<str>> = paths
            .into_iter()
            .filter_map(|path| {
                let trimmed = path.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(Box::from(trimmed))
                }
            })
            .collect();
        // `sort_unstable` is fine: equal entries are indistinguishable, and
        // `dedup` collapses them right after.
        paths.sort_unstable();
        paths.dedup();
        Self { paths }
    }

    /// Is this exact dotted path excluded?
    pub fn contains(&self, dotted_path: &str) -> bool {
        self.paths
            .binary_search_by(|candidate| (**candidate).cmp(dotted_path))
            .is_ok()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The paths, sorted. Used to re-layer the set: `resolve_onto` folds the
    /// CLI list onto the one `from_config` built out of the file.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().map(|path| &**path)
    }
}

impl Default for Settings {
    /// The built-in layer: what `hog` does with no config file and no flags.
    fn default() -> Self {
        Self {
            fields: FieldNames {
                ts: owned(DEFAULT_TS_FIELDS),
                level: owned(DEFAULT_LEVEL_FIELDS),
                msg: owned(DEFAULT_MSG_FIELDS),
            },
            time: TimeSettings {
                format: TimeFormat::Strftime(DEFAULT_TIME_FORMAT.to_owned()),
                // Deliberately not the offset carried by the input string —
                // see HLD §10.3 and the migration note in the README.
                zone: TimeZoneSpec::Local,
            },
            exclude: ExcludeSet::default(),
            levels: LevelAliases::default(),
            sort_keys: true,
            color: ColorChoiceArg::Auto,
            command: None,
        }
    }
}

fn owned(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// The middle layer: the config file folded onto the built-in defaults.
///
/// Every key the file did not mention keeps its default, which is the whole
/// reason [`Model`] is `Option`-shaped all the way down. Only `exclude` is
/// layered any further, by [`resolve_onto`]; everything else the file sets
/// simply replaces the default.
///
/// # Errors
///
/// [`Error::BadTimeFormat`] or [`Error::BadColorChoice`] for a value hog cannot
/// honour. In the binary these are unreachable: `config::load::parse` runs
/// [`Model::validate`] while the document still carries spans, so the same
/// failures have already been reported with a file and a line number. They are
/// still returned rather than unwrapped — a `Model` can be built by hand, and
/// an `expect()` in a production path is not allowed (`err-no-unwrap-prod`).
pub(crate) fn from_config(config: &Model) -> Result<Settings, Error> {
    let mut settings = Settings::default();

    if let Some(exclude) = &config.exclude {
        settings.exclude = ExcludeSet::new(exclude.iter().cloned());
    }
    settings.command.clone_from(&config.command);

    if let Some(fields) = &config.fields {
        replace_with(&mut settings.fields.ts, fields.ts.as_ref());
        replace_with(&mut settings.fields.level, fields.level.as_ref());
        replace_with(&mut settings.fields.msg, fields.msg.as_ref());
    }

    if let Some(output) = &config.output {
        if let Some(format) = &output.time_format {
            settings.time.format = TimeFormat::parse(format)?;
        }
        if let Some(zone) = &output.time_zone {
            settings.time.zone = TimeZoneSpec::parse(zone);
        }
        if let Some(color) = &output.color {
            settings.color =
                model::parse_color(color).ok_or_else(|| Error::BadColorChoice(color.clone()))?;
        }
        if let Some(levels) = &output.levels {
            settings.levels = LevelAliases::new(levels);
        }
        if let Some(sort_keys) = output.sort_keys {
            settings.sort_keys = sort_keys;
        }
    }

    Ok(settings)
}

/// Replaces a candidate list when the file named one, leaving it alone
/// otherwise. A bare `ts = "at"` and `ts = ["at"]` mean the same thing.
fn replace_with(target: &mut Vec<String>, candidates: Option<&Candidates>) {
    if let Some(candidates) = candidates {
        *target = candidates.as_slice().to_vec();
    }
}

/// Folds the CLI on top of the defaults. The only resolver in the crate.
///
/// Exclude semantics (HLD §6, §10.4):
///
/// | invocation      | effective excludes        |
/// |-----------------|---------------------------|
/// | `hog`           | config list               |
/// | `hog -e foo`    | config list **+** `foo`   |
/// | `hog -E`        | empty                     |
/// | `hog -E -e foo` | exactly `["foo"]`         |
///
/// `--ts-field` and friends **replace** the candidate list rather than
/// extending it: naming a field explicitly is a statement about this stream.
///
/// `config` is `None` when no config file was found, which is the documented
/// fourth case of the search order — built-in defaults, silently (HLD §3).
pub(crate) fn resolve(args: &RunArgs, config: Option<&Model>) -> Result<Settings, Error> {
    let base = match config {
        Some(config) => from_config(config)?,
        None => Settings::default(),
    };
    resolve_onto(base, args)
}

/// Applies the CLI layer to an already-resolved lower layer.
///
/// Split out from [`resolve`] so that the layering rules are testable without
/// a config file, and so that v0.2 has somewhere to hand its parsed config.
pub(crate) fn resolve_onto(base: Settings, args: &RunArgs) -> Result<Settings, Error> {
    let mut settings = base;

    if let Some(field) = &args.ts_field {
        settings.fields.ts = vec![field.clone()];
    }
    if let Some(field) = &args.level_field {
        settings.fields.level = vec![field.clone()];
    }
    if let Some(field) = &args.msg_field {
        settings.fields.msg = vec![field.clone()];
    }

    if let Some(format) = &args.ts_format {
        settings.time.format = TimeFormat::parse(format)?;
    }
    if let Some(zone) = &args.timezone {
        settings.time.zone = TimeZoneSpec::parse(zone);
    }

    // `-E` drops the lower layer, `-e` adds to whatever is left. Both together
    // are the documented way to replace the list outright.
    let mut paths: Vec<String> = if args.reset_exclude {
        Vec::new()
    } else {
        settings.exclude.iter().map(str::to_owned).collect()
    };
    paths.extend(args.exclude.iter().cloned());
    settings.exclude = ExcludeSet::new(paths);

    // `--color` is an `Option` precisely so that this line can be skipped when
    // the flag was not given: with a clap default, "not given" and "given as
    // auto" would be the same value and `output.color` in the file could never
    // take effect.
    if let Some(color) = args.color {
        settings.color = color;
    }

    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::Parser as _;

    use crate::cli::Cli;

    /// Parses a command line the way `main` does, and hands back the run args.
    fn args(argv: &[&str]) -> RunArgs {
        Cli::try_parse_from(argv).expect("test argv must parse").run
    }

    /// Deserializes config text the way `config::load::parse` does.
    fn config(text: &str) -> Model {
        toml_edit::de::from_str(text).expect("test config must deserialize")
    }

    fn effective_excludes(argv: &[&str]) -> Vec<String> {
        let settings = resolve(&args(argv), None).expect("settings must resolve");
        settings.exclude.iter().map(str::to_owned).collect()
    }

    /// The config layer standing in for a file with `exclude = ["cfg_a",
    /// "cfg_b"]`, built the same way the binary builds it.
    fn base_with_config_excludes() -> Settings {
        from_config(&config("exclude = [\"cfg_a\", \"cfg_b\"]\n")).expect("the config resolves")
    }

    fn layered(argv: &[&str]) -> Vec<String> {
        let settings =
            resolve_onto(base_with_config_excludes(), &args(argv)).expect("settings must resolve");
        settings.exclude.iter().map(str::to_owned).collect()
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let settings = Settings::default();
        assert_eq!(settings.fields.ts, DEFAULT_TS_FIELDS);
        assert_eq!(settings.fields.level, DEFAULT_LEVEL_FIELDS);
        assert_eq!(settings.fields.msg, DEFAULT_MSG_FIELDS);
        assert_eq!(
            settings.time.format,
            TimeFormat::Strftime(DEFAULT_TIME_FORMAT.to_owned())
        );
        assert_eq!(settings.time.zone, TimeZoneSpec::Local);
        assert!(settings.sort_keys);
        assert!(settings.exclude.is_empty());
    }

    // HLD §6/§10.4, with an empty v0.1 config layer.
    #[test]
    fn exclude_table_without_a_config() {
        assert_eq!(effective_excludes(&["hog"]), Vec::<String>::new());
        assert_eq!(effective_excludes(&["hog", "-e", "foo"]), ["foo"]);
        assert_eq!(effective_excludes(&["hog", "-E"]), Vec::<String>::new());
        assert_eq!(effective_excludes(&["hog", "-E", "-e", "foo"]), ["foo"]);
    }

    // The same table against a non-empty lower layer, which is what makes the
    // difference between "adds" and "resets" observable.
    #[test]
    fn exclude_table_with_a_config_layer() {
        assert_eq!(layered(&["hog"]), ["cfg_a", "cfg_b"]);
        assert_eq!(layered(&["hog", "-e", "foo"]), ["cfg_a", "cfg_b", "foo"]);
        assert_eq!(layered(&["hog", "-E"]), Vec::<String>::new());
        assert_eq!(layered(&["hog", "-E", "-e", "foo"]), ["foo"]);
        // -E -e cfg_a is a replacement, not a removal of one entry.
        assert_eq!(layered(&["hog", "-E", "-e", "cfg_a"]), ["cfg_a"]);
    }

    #[test]
    fn exclude_accepts_commas_and_repeats() {
        assert_eq!(
            effective_excludes(&["hog", "-e", "a,b", "-e", "c"]),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn exclude_entries_are_trimmed_sorted_and_deduplicated() {
        let set = ExcludeSet::new(
            ["b", " a ", "", "   ", "a"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
        assert_eq!(set.iter().collect::<Vec<_>>(), ["a", "b"]);
    }

    // The last column of the HLD §6 table: same prefix, different segment.
    #[test]
    fn exclude_matching_is_exact_per_path() {
        let set = ExcludeSet::new(["grpc".to_owned()]);
        assert!(set.contains("grpc"));
        assert!(!set.contains("grpcStatus"));
        assert!(!set.contains("grpc.code"), "pruning happens at the node");
        assert!(!set.contains(""));
    }

    #[test]
    fn empty_exclude_set_contains_nothing() {
        let set = ExcludeSet::default();
        assert!(set.is_empty());
        assert!(!set.contains("anything"));
    }

    /// `Error` is not `PartialEq` (it carries `CommandError`), so the happy
    /// path is unwrapped by hand rather than compared as a `Result`.
    fn time_format(value: &str) -> TimeFormat {
        TimeFormat::parse(value).expect("value must parse")
    }

    #[test]
    fn time_format_parsing() {
        assert_eq!(
            time_format("%H:%M:%S"),
            TimeFormat::Strftime("%H:%M:%S".to_owned())
        );
        assert_eq!(time_format("raw"), TimeFormat::Raw);
        assert_eq!(time_format("RAW"), TimeFormat::Raw);
        assert_eq!(time_format("none"), TimeFormat::Hidden);
        // A typo must not print itself once per line.
        assert!(matches!(
            TimeFormat::parse("HH:MM:SS"),
            Err(Error::BadTimeFormat(value)) if value == "HH:MM:SS"
        ));
    }

    #[test]
    fn time_zone_parsing() {
        assert_eq!(TimeZoneSpec::parse("local"), TimeZoneSpec::Local);
        assert_eq!(TimeZoneSpec::parse("Local"), TimeZoneSpec::Local);
        assert_eq!(TimeZoneSpec::parse("utc"), TimeZoneSpec::Utc);
        assert_eq!(
            TimeZoneSpec::parse("Europe/Moscow"),
            TimeZoneSpec::Named("Europe/Moscow".to_owned())
        );
    }

    #[test]
    fn field_flags_replace_the_candidate_list() {
        let settings = resolve(
            &args(&[
                "hog",
                "--ts-field",
                "at",
                "--level-field",
                "sev",
                "--msg-field",
                "text",
            ]),
            None,
        )
        .expect("settings must resolve");
        assert_eq!(settings.fields.ts, ["at"]);
        assert_eq!(settings.fields.level, ["sev"]);
        assert_eq!(settings.fields.msg, ["text"]);
    }

    #[test]
    fn bad_time_format_fails_at_resolve_time() {
        let err = resolve(&args(&["hog", "--ts-format", "HH:MM"]), None).expect_err("must fail");
        assert!(matches!(err, Error::BadTimeFormat(_)));
    }

    // ============================================================ config layer

    /// A file that mentions nothing changes nothing: the middle layer has to be
    /// able to say "not set" rather than "set to the default".
    #[test]
    fn an_empty_config_is_the_built_in_defaults() {
        let settings = from_config(&config("")).expect("an empty config resolves");
        let defaults = Settings::default();
        assert_eq!(settings.fields.ts, defaults.fields.ts);
        assert_eq!(settings.time.format, defaults.time.format);
        assert_eq!(settings.time.zone, defaults.time.zone);
        assert_eq!(settings.sort_keys, defaults.sort_keys);
        assert_eq!(settings.color, defaults.color);
        assert!(settings.exclude.is_empty());
        assert!(settings.command.is_none());
    }

    #[test]
    fn the_config_sets_every_key_it_mentions() {
        let settings = from_config(&config(
            "exclude = [\"trace_id\", \"grpc\"]\n\
             command = \"ssh {0}\"\n\
             [fields]\n\
             ts = \"at\"\n\
             level = [\"lvl\", \"severity\"]\n\
             [output]\n\
             time_format = \"raw\"\n\
             time_zone = \"utc\"\n\
             color = \"never\"\n\
             sort_keys = false\n",
        ))
        .expect("the config resolves");

        assert_eq!(
            settings.exclude.iter().collect::<Vec<_>>(),
            ["grpc", "trace_id"]
        );
        assert_eq!(settings.command.as_deref(), Some("ssh {0}"));
        assert_eq!(settings.fields.ts, ["at"]);
        assert_eq!(settings.fields.level, ["lvl", "severity"]);
        // Unmentioned: still the default.
        assert_eq!(settings.fields.msg, DEFAULT_MSG_FIELDS);
        assert_eq!(settings.time.format, TimeFormat::Raw);
        assert_eq!(settings.time.zone, TimeZoneSpec::Utc);
        assert_eq!(settings.color, ColorChoiceArg::Never);
        assert!(!settings.sort_keys);
    }

    /// The layering trap `--color` carries: with a clap default, "not given"
    /// and "given as auto" would be the same value and this key could never
    /// win. It is an `Option` on the CLI side precisely so that it can.
    #[test]
    fn the_file_sets_the_colour_until_the_flag_says_otherwise() {
        let file = config("[output]\ncolor = \"never\"\n");
        assert_eq!(
            resolve(&args(&["hog"]), Some(&file))
                .expect("resolves")
                .color,
            ColorChoiceArg::Never
        );
        assert_eq!(
            resolve(&args(&["hog", "--color", "always"]), Some(&file))
                .expect("resolves")
                .color,
            ColorChoiceArg::Always
        );
        // Naming the default explicitly is still an override, not a no-op.
        assert_eq!(
            resolve(&args(&["hog", "--color", "auto"]), Some(&file))
                .expect("resolves")
                .color,
            ColorChoiceArg::Auto
        );
    }

    /// The HLD §11.6 table, against a real config file this time.
    #[test]
    fn the_exclude_table_layers_over_a_real_config() {
        let file = config("exclude = [\"a\"]\n");
        let effective = |argv: &[&str]| -> Vec<String> {
            resolve(&args(argv), Some(&file))
                .expect("resolves")
                .exclude
                .iter()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(effective(&["hog"]), ["a"]);
        assert_eq!(effective(&["hog", "-e", "b"]), ["a", "b"]);
        assert_eq!(effective(&["hog", "-E"]), Vec::<String>::new());
        assert_eq!(effective(&["hog", "-E", "-e", "c"]), ["c"]);
    }

    /// A flag beats the file for the three field lists too, and replaces the
    /// whole candidate list rather than extending it.
    #[test]
    fn a_field_flag_beats_the_file() {
        let file = config("[fields]\nts = [\"at\", \"when\"]\n");
        let settings =
            resolve(&args(&["hog", "--ts-field", "stamp"]), Some(&file)).expect("resolves");
        assert_eq!(settings.fields.ts, ["stamp"]);
    }

    #[test]
    fn the_config_maps_a_producers_own_level_spellings() {
        let settings = from_config(&config(
            "[output.levels]\n\"30\" = \"info\"\n\"50\" = \"error\"\n",
        ))
        .expect("the config resolves");

        assert_eq!(settings.levels.resolve("30"), "info");
        assert_eq!(settings.levels.resolve("50"), "error");
        // Anything the table does not cover comes back untouched.
        assert_eq!(settings.levels.resolve("warn"), "warn");
        assert!(!settings.levels.is_empty());
        assert!(Settings::default().levels.is_empty());
    }

    #[test]
    fn level_aliases_are_trimmed_case_insensitive_and_never_blank() {
        let aliases = LevelAliases::new([(" TRACE ", " debug "), ("", "info"), ("x", "  ")]);
        assert_eq!(aliases.resolve("trace"), "debug");
        assert_eq!(aliases.resolve("x"), "x", "a blank target is not an alias");
        assert_eq!(aliases.iter().collect::<Vec<_>>(), [("TRACE", "debug")]);
    }

    /// Only reachable from a hand-built `Model` — `config::load::parse`
    /// validates first — but it must be an error, never an `expect()`.
    #[test]
    fn a_colour_the_file_spells_wrong_is_an_error() {
        let mut file = config("[output]\n");
        file.output
            .get_or_insert_with(Default::default)
            .color
            .replace("pink".to_owned());
        assert!(matches!(
            from_config(&file),
            Err(Error::BadColorChoice(value)) if value == "pink"
        ));
    }
}
