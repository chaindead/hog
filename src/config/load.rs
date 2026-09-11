//! Reading the config: parse once, keep both views, audit what hog does not
//! know.
//!
//! # Why the file is parsed once and kept twice
//!
//! `toml_edit` 0.25 has two document types and this module needs both:
//!
//! * [`Document<S>`] — the immutable root the parser produces. It is the only
//!   one that carries **spans**, which is where the line numbers in the
//!   unknown-key warnings come from. `ImDocument` is the deprecated 0.23 alias
//!   for it and must not be used.
//! * [`DocumentMut`] — the editable tree. `hog config -e` appends to it and
//!   [`write`](super::write) writes it back with every comment and blank line
//!   intact.
//!
//! `Document::into_mut` despans as it converts, so everything that needs a line
//! number has to happen **before** the conversion; and `de::from_document`
//! consumes the document, so one of the two views is produced from a clone.
//! That is one clone of a file measured in kilobytes, once per process — the
//! alternative is parsing the same text twice, which is both slower and able to
//! disagree with itself.
//!
//! Which view gets the clone is not arbitrary. Serde takes it, because the
//! spanned original has to outlive deserialization: a value serde accepts but
//! hog cannot honour (`color = "pink"`) is only worth reporting with the line it
//! is written on, and by then the model is built. The editable view is made
//! last, from the original, and costs nothing.

use std::fmt::Display;
use std::io::Write;
use std::ops::Range;
use std::path::Path;
use std::{fs, io};

use anyhow::Context as _;
use toml_edit::{Document, DocumentMut, Item, TableLike};

use super::discover::{self, Env, Location};
use super::model::{self, Model};
use crate::error::Error;

/// Line reported for a key whose span the parser did not record.
///
/// Only reachable for a document built in memory rather than parsed; no editor
/// will jump to it, and no test should expect it from parsed input.
const NO_LINE: usize = 0;

/// A config file that was found, parsed and audited.
///
/// The fields are public because this is a carrier, not an invariant: the
/// parsed model, the editable document and the provenance all travel together
/// precisely so that `hog config` can read the model, edit the document and
/// name the file in one breath, without a second read that could disagree with
/// the first.
#[derive(Debug)]
pub struct Loaded {
    /// The file this came from, and the layer of the search order that named
    /// it. Carried so errors and `hog config` output can name the real file
    /// rather than "the config".
    pub location: Location,

    /// The deserialized config — the middle layer of
    /// `defaults < config file < CLI flags`.
    pub model: Model,

    /// The same file as an editable tree, comments and formatting intact.
    /// [`edit`](super::edit) mutates this and [`write`](super::write) saves it.
    pub document: DocumentMut,

    /// Keys hog does not understand, in the order they appear in the file.
    /// Already reported by the time a caller sees this, except in tests.
    pub unknown: Vec<UnknownKey>,
}

impl Loaded {
    /// The file this config was read from.
    pub fn path(&self) -> &Path {
        &self.location.path
    }

    /// Prints one warning line per unknown key, in file order.
    ///
    /// The line number is what makes the warning worth printing, so the format
    /// is the usual compiler-ish one an editor can jump to:
    ///
    /// ```text
    /// warning: /Users/you/.hog.toml:12: unknown key `output.time_fmt`
    /// ```
    ///
    /// A warning, never an error: denying unknown keys would break a config
    /// written for a newer hog (HLD §7.2). Write failures are dropped — a
    /// closed stderr must not turn a warning into a second failure.
    pub fn warn_unknown_keys<W: Write>(&self, out: &mut W) {
        let path = self.path().display();
        for key in &self.unknown {
            let _ = writeln!(
                out,
                "warning: {path}:{}: unknown key `{}`",
                key.line, key.path
            );
        }
    }
}

/// A key the config file sets and this version of hog does not understand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey {
    /// The dotted path as written, e.g. `output.time_fmt`.
    pub path: String,
    /// 1-based line number of the key in the file — the whole point of doing
    /// this with `toml_edit` spans rather than `serde(deny_unknown_fields)`.
    pub line: usize,
}

/// Locates and reads the config for a run.
///
/// `Ok(None)` means "no config file, use the built-in defaults": either nothing
/// could be located, or the default path simply has no file yet. A missing file
/// at a path the user named explicitly is an error instead — see
/// [`Source::is_explicit`](super::discover::Source::is_explicit).
///
/// # Errors
///
/// The file was named explicitly and is missing, cannot be read, or does not
/// parse. All three exit 1.
pub fn load(explicit: Option<&Path>, env: &Env) -> anyhow::Result<Option<Loaded>> {
    match discover::locate(explicit, env) {
        Some(location) => read(location),
        // No $HOME: there is no file to miss.
        None => Ok(None),
    }
}

/// Reads and parses the file named by `location`.
///
/// `Ok(None)` is returned when the location is not explicit and nothing there
/// is a file — it does not exist, it is a directory, or a component of the path
/// is not one ([`is_not_a_file`]). An unreadable file that *does* exist is
/// always an error: a permissions problem is not the same as "no config", and
/// treating it as one would render the stream with the wrong settings.
///
/// # Errors
///
/// A missing file at an explicit location, any I/O failure on a file that is
/// really there, or a parse failure from [`parse`].
pub fn read(location: Location) -> anyhow::Result<Option<Loaded>> {
    let text = match fs::read_to_string(&location.path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if location.source.is_explicit() {
                // The user named this file. Falling back to the defaults would
                // render the whole stream with settings they did not ask for,
                // and the only clue would be the output looking wrong.
                anyhow::bail!(
                    "{} points at a file that does not exist: {}",
                    location.source.label(),
                    location.path.display()
                );
            }
            return Ok(None);
        }
        // The same answer as `NotFound`, phrased by the kernel differently
        // because the *path* is unusable rather than merely empty: `$HOME` is a
        // regular file (`ENOTDIR`), or something made a directory called
        // `.hog.toml` (`EISDIR`). Either way there is no config file here and
        // there never was one — and since hog creates the default file itself
        // now, refusing to render a single log line over a `$HOME` it could not
        // write into would break the one promise auto-creation has to keep: a
        // config hog failed to make is worth the built-in defaults, never an
        // exit code. A path the user *named* keeps the error, with the reason
        // in it: they asked for that file and deserve to hear why it is not one.
        Err(err) if !location.source.is_explicit() && is_not_a_file(err.kind()) => {
            return Ok(None);
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to read the config file {}", location.path.display())
            });
        }
    };

    parse(&text, location).map(Some)
}

/// Does this read failure mean "nothing here is a file", as opposed to "a file
/// is here and hog may not read it"?
///
/// The distinction is the whole point: `EACCES` on a real `~/.hog.toml` stays
/// fatal, because that file *is* the user's config and rendering the stream
/// with the built-in defaults instead would be a silent wrong answer. A
/// directory, or a `$HOME` that is not a directory, holds no config to be wrong
/// about.
fn is_not_a_file(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::IsADirectory | io::ErrorKind::NotADirectory
    )
}

/// Parses config text that is already in memory.
///
/// The pure half of [`read`], and where the interesting test matrix lives:
/// every unknown-key case and every malformed-value case runs through here with
/// no filesystem involved.
///
/// # Errors
///
/// A syntax error surfaces as [`Error::ConfigParse`] (exit code 1) carrying the
/// file name and the line number from [`TomlError::span`](toml_edit::TomlError::span),
/// so a broken config reads like a compiler diagnostic rather than a stack
/// trace. A value hog cannot honour ([`Model::validate`]) fails the same way,
/// on the line it is written. Unknown keys are collected into
/// [`Loaded::unknown`], not failed on.
pub fn parse(text: &str, location: Location) -> anyhow::Result<Loaded> {
    let parsed = Document::parse(text.to_owned())
        .map_err(|err| config_error(&location, text, err.span(), err.message()))?;

    // Before anything consumes the document: this is the only view with spans.
    let unknown = audit(&parsed);

    let model: Model = toml_edit::de::from_document(parsed.clone())
        .map_err(|err| config_error(&location, text, err.span(), err.message()))?;

    if let Err(invalid) = model.validate() {
        let span = key_span(&parsed, invalid.key);
        return Err(config_error(&location, text, span, &invalid));
    }

    // Last, because it despans: after this call no line number can be found.
    let document = parsed.into_mut();

    Ok(Loaded {
        location,
        model,
        document,
        unknown,
    })
}

/// Builds the one config failure that carries an exit code.
fn config_error(
    location: &Location,
    text: &str,
    span: Option<Range<usize>>,
    message: impl Display,
) -> anyhow::Error {
    Error::ConfigParse {
        path: location.path.display().to_string(),
        line: span.map_or(1, |span| line_of(text, span.start)),
        message: message.to_string(),
    }
    .into()
}

/// Walks the parsed document and collects every key hog does not know.
///
/// The walk is over the **document**, not the model, because that is the only
/// side that still knows where each key was written. Rules:
///
/// * dotted paths are built segment by segment, so a key inside `[output]`
///   reports as `output.time_fmt` and not as `time_fmt`;
/// * a table listed in [`OPEN_TABLES`](super::model::OPEN_TABLES) is not
///   descended into — `[output.levels]` holds user data, and warning about
///   `output.levels.30` would be nonsense;
/// * an unknown **table** is reported once, by its header, and not descended
///   into either: a mistyped `[outpout]` should produce one warning, not one
///   per key inside it.
///
/// Keys with no span (a document built in memory rather than parsed) are
/// reported with line `0`, which no editor will jump to and no test should
/// expect from parsed input.
pub fn audit(document: &Document<String>) -> Vec<UnknownKey> {
    let mut found = Vec::new();
    walk(document.as_table(), "", document.raw(), &mut found);
    found
}

/// One level of [`audit`], for anything table-shaped.
///
/// `&dyn TableLike` rather than `&Table` so that `output = { color = "auto" }`
/// walks exactly like `[output]` does: TOML spells the same key two ways and
/// the user should be warned about a typo in either.
fn walk(table: &dyn TableLike, prefix: &str, text: &str, found: &mut Vec<UnknownKey>) {
    for (name, item) in table.iter() {
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}.{name}")
        };

        // An open table's *contents* are user data; its header was already
        // accepted as a known key by the caller.
        if model::OPEN_TABLES.contains(&path.as_str()) {
            continue;
        }

        if !model::is_known(&path) {
            found.push(UnknownKey {
                line: line_at(table, name, item, text),
                path,
            });
            // Not descended into: one warning for `[outpout]`, not one per key
            // the user wrote under it.
            continue;
        }

        if let Some(child) = item.as_table_like() {
            walk(child, &path, text, found);
        } else if let Item::ArrayOfTables(tables) = item {
            // `[[output]]` where a table was meant: serde will reject it, but
            // the audit runs first and a typo inside it should still be named.
            for child in tables {
                walk(child, &path, text, found);
            }
        }
    }
}

/// The line a key is written on, preferring the key's own span over its value's.
fn line_at(table: &dyn TableLike, name: &str, item: &Item, text: &str) -> usize {
    table
        .key(name)
        .and_then(toml_edit::Key::span)
        .or_else(|| item.span())
        .map_or(NO_LINE, |span| line_of(text, span.start))
}

/// The span of the key at `dotted_path`, for turning a validation failure into
/// a line number.
///
/// Walks the same way [`walk`] does, so `[output] color = …`,
/// `output = { color = … }` and `output.color = …` all resolve. `None` when the
/// path is not in the document, which can only happen if
/// [`KNOWN_KEYS`](super::model::KNOWN_KEYS) and the serde model disagree.
fn key_span(document: &Document<String>, dotted_path: &str) -> Option<Range<usize>> {
    let mut table: &dyn TableLike = document.as_table();
    let mut segments = dotted_path.split('.').peekable();

    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            return table
                .key(segment)
                .and_then(toml_edit::Key::span)
                .or_else(|| table.get(segment).and_then(Item::span));
        }
        table = table.get(segment)?.as_table_like()?;
    }

    None
}

/// Turns a byte offset from a `toml_edit` span into a 1-based line number.
///
/// The number of `\n`-separated pieces in `text[..offset]`, which is the same
/// number as "newlines before the offset, plus one" without the `+ 1` having to
/// be got right. An offset past the end of the text clamps to the last line
/// rather than panicking — a span that cannot be trusted should still not take
/// the process down over a warning. The split runs over **bytes** for the same
/// reason: an offset that lands inside a multi-byte character would panic a
/// `str` slice.
pub fn line_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    text.as_bytes()[..end].split(|byte| *byte == b'\n').count()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::discover::{CONFIG_FILE, Source};
    use super::*;

    /// The commented starter config, which is also the format's documentation.
    /// Reading it here keeps `model::KNOWN_KEYS` honest: a key the starter
    /// documents and the model forgot would warn about itself on a fresh
    /// install.
    const STARTER: &str = super::super::edit::STARTER;

    fn location(path: &str) -> Location {
        Location {
            path: PathBuf::from(path),
            source: Source::Home,
        }
    }

    fn loaded(text: &str) -> Loaded {
        parse(text, location("/cfg/config.toml")).expect("test config must parse")
    }

    /// The `(path, line)` pairs the audit reports, which is all a test cares
    /// about.
    fn unknown(text: &str) -> Vec<(String, usize)> {
        loaded(text)
            .unknown
            .into_iter()
            .map(|key| (key.path, key.line))
            .collect()
    }

    fn parse_failure(text: &str) -> (usize, String) {
        let err = parse(text, location("/cfg/config.toml")).expect_err("must fail");
        match err.downcast_ref::<Error>() {
            Some(Error::ConfigParse { line, message, .. }) => (*line, message.clone()),
            other => panic!("expected a ConfigParse error, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------- line_of

    #[test]
    fn line_of_counts_newlines_before_the_offset() {
        let text = "a\nb\nc";
        assert_eq!(line_of(text, 0), 1);
        assert_eq!(line_of(text, 1), 1, "the newline itself is still line 1");
        assert_eq!(line_of(text, 2), 2);
        assert_eq!(line_of(text, 4), 3);
    }

    #[test]
    fn line_of_clamps_an_offset_past_the_end() {
        assert_eq!(line_of("a\nb\n", usize::MAX), 3);
        assert_eq!(line_of("", 99), 1);
    }

    /// A `str` slice at this offset would panic; the byte count does not.
    #[test]
    fn line_of_survives_an_offset_inside_a_character() {
        assert_eq!(line_of("«\n»", 1), 1);
    }

    // ------------------------------------------------------------------ audit

    #[test]
    fn a_config_of_only_known_keys_reports_nothing() {
        assert!(unknown("exclude = []\ncommand = \"ssh {0}\"\n").is_empty());
        assert!(unknown("[fields]\nts = \"at\"\n[output]\ncolor = \"never\"\n").is_empty());
    }

    /// The starter file is what hog writes on a first run. If it warned about
    /// itself, every fresh install would start with a diagnostic.
    #[test]
    fn the_starter_config_has_no_unknown_keys() {
        assert_eq!(unknown(STARTER), Vec::new());
    }

    /// Not a formality: `hog config -e` writes this document back, and a
    /// `toml_edit` round-trip that lost a comment would silently delete the
    /// documentation of the format.
    #[test]
    fn an_untouched_document_round_trips_byte_for_byte() {
        assert_eq!(loaded(STARTER).document.to_string(), STARTER);
    }

    #[test]
    fn an_unknown_top_level_key_is_reported_with_its_line() {
        assert_eq!(
            unknown("exclude = []\n\nfrom_the_future = 1\n"),
            [("from_the_future".to_owned(), 3)]
        );
    }

    #[test]
    fn an_unknown_key_in_a_table_reports_its_dotted_path() {
        assert_eq!(
            unknown("[output]\ntime_format = \"%H\"\ntime_fmt = \"%H\"\n"),
            [("output.time_fmt".to_owned(), 3)]
        );
    }

    #[test]
    fn an_unknown_table_is_reported_once_by_its_header() {
        assert_eq!(
            unknown("[outpout]\ntime_format = \"%H\"\ncolor = \"auto\"\n"),
            [("outpout".to_owned(), 1)],
            "one warning for the header, not one per key inside it"
        );
    }

    #[test]
    fn user_data_inside_an_open_table_is_never_unknown() {
        assert!(
            unknown("[output.levels]\n\"30\" = \"info\"\ntrace = \"debug\"\n").is_empty(),
            "[output.levels] keys are whatever the producer emits"
        );
    }

    #[test]
    fn an_inline_table_is_walked_like_a_header_table() {
        assert_eq!(
            unknown("output = { color = \"auto\", time_fmt = \"%H\" }\n"),
            [("output.time_fmt".to_owned(), 1)]
        );
    }

    #[test]
    fn a_dotted_key_reports_the_path_it_spells() {
        assert_eq!(
            unknown("output.time_fmt = \"%H\"\n"),
            [("output.time_fmt".to_owned(), 1)]
        );
    }

    #[test]
    fn unknown_keys_come_back_in_file_order() {
        let text = "zzz = 1\n[output]\nb_typo = 1\nc_typo = 2\n";
        assert_eq!(
            unknown(text),
            [
                ("zzz".to_owned(), 1),
                ("output.b_typo".to_owned(), 3),
                ("output.c_typo".to_owned(), 4),
            ]
        );
    }

    #[test]
    fn a_key_that_only_shares_a_prefix_with_an_open_table_is_unknown() {
        assert_eq!(
            unknown("[output]\nlevelsets = 1\n"),
            [("output.levelsets".to_owned(), 2)]
        );
    }

    #[test]
    fn warnings_name_the_file_the_line_and_the_key() {
        let mut out = Vec::new();
        loaded("[output]\ntime_fmt = \"%H\"\n").warn_unknown_keys(&mut out);
        assert_eq!(
            String::from_utf8(out).expect("warnings are UTF-8"),
            "warning: /cfg/config.toml:2: unknown key `output.time_fmt`\n"
        );
    }

    #[test]
    fn a_config_without_unknown_keys_warns_about_nothing() {
        let mut out = Vec::new();
        loaded("exclude = []\n").warn_unknown_keys(&mut out);
        assert!(out.is_empty());
    }

    // ------------------------------------------------------------- parse fails

    #[test]
    fn a_syntax_error_carries_its_line_and_a_readable_message() {
        let (line, message) = parse_failure("exclude = []\n[output\ncolor = \"auto\"\n");
        assert_eq!(line, 2);
        assert!(!message.is_empty(), "the parser's own words are kept");
        assert!(
            !message.contains('\n'),
            "one line, so `error: {{err}}` stays one line: {message}"
        );
    }

    #[test]
    fn a_wrongly_typed_value_carries_its_line() {
        let (line, message) = parse_failure("[output]\nsort_keys = \"yes\"\n");
        assert_eq!(line, 2);
        assert!(message.contains("boolean"), "{message}");
    }

    #[test]
    fn the_error_names_the_file() {
        let err = parse("[output\n", location("/cfg/config.toml")).expect_err("must fail");
        assert!(err.to_string().starts_with("/cfg/config.toml:1:"), "{err}");
    }

    // A value serde accepts and hog cannot honour, reported on its own line.
    #[test]
    fn a_value_hog_cannot_honour_fails_on_its_line() {
        let (line, message) = parse_failure("[output]\ntime_format = \"%H\"\ncolor = \"pink\"\n");
        assert_eq!(line, 3);
        assert!(message.contains("pink"), "{message}");
    }

    #[test]
    fn a_bad_value_in_an_inline_table_also_finds_its_line() {
        let (line, _) = parse_failure("exclude = []\n\noutput = { color = \"pink\" }\n");
        assert_eq!(line, 3);
    }

    #[test]
    fn a_bad_time_format_is_a_config_error_not_a_literal() {
        let (line, message) = parse_failure("[output]\ntime_format = \"HH:MM:SS\"\n");
        assert_eq!(line, 2);
        assert!(message.contains("HH:MM:SS"), "{message}");
    }

    // ------------------------------------------------------------------ model

    #[test]
    fn the_parsed_model_carries_the_file_and_its_values() {
        let loaded = loaded(
            "exclude = [\"trace_id\"]\ncommand = \"ssh {0}\"\n[output]\nsort_keys = false\n",
        );
        assert_eq!(loaded.path(), Path::new("/cfg/config.toml"));
        assert_eq!(
            loaded.model.exclude.as_deref(),
            Some(["trace_id".to_owned()].as_slice())
        );
        assert_eq!(loaded.model.command.as_deref(), Some("ssh {0}"));
        assert_eq!(
            loaded.model.output.and_then(|output| output.sort_keys),
            Some(false)
        );
    }

    /// The middle layer has to be able to say "the file did not mention this".
    #[test]
    fn an_empty_file_decides_nothing() {
        let model = loaded("").model;
        assert!(model.exclude.is_none());
        assert!(model.output.is_none());
    }

    // ------------------------------------------------------------------- read

    /// A directory that deletes itself, so a failing test cannot leave one
    /// behind (`test-fixture-raii`).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("hog-config-load-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("the temp directory must be creatable");
            Self { path }
        }

        /// Writes `text` to `name` inside the directory and returns the path.
        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.path.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the parent must be creatable");
            }
            fs::write(&path, text).expect("the file must be writable");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn a_missing_file_at_a_guessed_path_is_not_an_error() {
        let dir = TempDir::new();
        let location = Location {
            path: dir.path.join("nothing-here.toml"),
            source: Source::Home,
        };
        let loaded = read(location).expect("a missing default config is the ordinary case");
        assert!(loaded.is_none());
    }

    #[test]
    fn a_missing_file_the_user_named_is_an_error_that_names_the_knob() {
        for (source, label) in [(Source::Flag, "--config"), (Source::Env, "$HOG_CONFIG")] {
            let dir = TempDir::new();
            let path = dir.path.join("nothing-here.toml");
            let err = read(Location {
                path: path.clone(),
                source,
            })
            .expect_err("an explicitly named file must exist");
            let message = err.to_string();
            assert!(message.contains(label), "{message}");
            assert!(
                message.contains(&path.display().to_string()),
                "the message must name the file: {message}"
            );
        }
    }

    #[test]
    fn an_existing_file_is_read_parsed_and_audited() {
        let dir = TempDir::new();
        let path = dir.write("config.toml", "exclude = [\"a\"]\nmystery = 1\n");
        let loaded = read(Location {
            path: path.clone(),
            source: Source::Flag,
        })
        .expect("the file parses")
        .expect("the file exists");

        assert_eq!(loaded.path(), path);
        assert_eq!(
            loaded.model.exclude.as_deref(),
            Some(["a".to_owned()].as_slice())
        );
        assert_eq!(loaded.unknown.len(), 1);
        assert_eq!(loaded.unknown[0].path, "mystery");
    }

    /// A directory at a path the user *named* is an error, and the error says
    /// which kind of wrong it is rather than claiming the file is missing.
    #[test]
    fn a_directory_at_a_named_path_is_an_error_rather_than_no_config() {
        let dir = TempDir::new();
        let err = read(Location {
            path: dir.path.clone(),
            source: Source::Flag,
        })
        .expect_err("a directory is not an absent file");
        assert!(err.to_string().contains("failed to read the config file"));
    }

    /// The same directory at the *default* path is the built-in defaults, not
    /// an exit code.
    ///
    /// hog creates `~/.hog.toml` itself now, so this is the shape a failed
    /// creation leaves behind — someone ran `mkdir ~/.hog.toml`, or `$HOME` is
    /// a regular file — and a tool that answers a broken `$HOME` by refusing to
    /// print logs is useless exactly when it is needed. Nothing here is a
    /// config file, so there is nothing to get wrong by ignoring it.
    #[test]
    fn a_directory_at_the_default_path_is_no_config_rather_than_an_error() {
        let dir = TempDir::new();
        let loaded = read(Location {
            path: dir.path.clone(),
            source: Source::Home,
        })
        .expect("a directory at a guessed path is not the user's config");
        assert!(loaded.is_none());
    }

    /// `$HOME` is a regular file, so `$HOME/.hog.toml` cannot exist at all:
    /// `ENOTDIR`, which is `NotFound` wearing a different hat.
    #[test]
    fn a_home_that_is_a_file_is_no_config_rather_than_an_error() {
        let dir = TempDir::new();
        let home = dir.write("home-is-a-file", "not a directory\n");
        let loaded = read(Location {
            path: home.join(CONFIG_FILE),
            source: Source::Home,
        })
        .expect("a `$HOME` that is not a directory holds no config");
        assert!(loaded.is_none());

        // And the same path named explicitly still reports itself, with the
        // kernel's reason underneath rather than a claim that it is missing.
        let err = read(Location {
            path: home.join(CONFIG_FILE),
            source: Source::Env,
        })
        .expect_err("a named file that cannot exist is the user's news");
        assert!(
            err.to_string().contains("failed to read the config file"),
            "{err}"
        );
    }

    // ------------------------------------------------------------------- load

    /// Builds an [`Env`] whose `$HOME` is `dir`.
    fn env_at(dir: &TempDir) -> Env {
        Env {
            hog_config: None,
            home: Some(dir.path.clone().into_os_string()),
        }
    }

    #[test]
    fn load_finds_the_dotfile_in_home() {
        let dir = TempDir::new();
        dir.write(CONFIG_FILE, "exclude = [\"trace_id\"]\n");

        let loaded = load(None, &env_at(&dir))
            .expect("the file parses")
            .expect("the file exists");
        assert_eq!(loaded.location.source, Source::Home);
        assert_eq!(
            loaded.model.exclude.as_deref(),
            Some(["trace_id".to_owned()].as_slice())
        );
    }

    #[test]
    fn load_without_a_config_file_is_the_built_in_defaults() {
        let dir = TempDir::new();
        assert!(
            load(None, &env_at(&dir))
                .expect("a missing default config is not an error")
                .is_none()
        );
    }

    #[test]
    fn load_with_nowhere_to_look_is_the_built_in_defaults() {
        assert!(
            load(None, &Env::default())
                .expect("no HOME is not an error")
                .is_none()
        );
    }

    #[test]
    fn load_prefers_the_explicit_path_over_the_default_one() {
        let dir = TempDir::new();
        dir.write(CONFIG_FILE, "exclude = [\"from_home\"]\n");
        let explicit = dir.write("explicit.toml", "exclude = [\"from_flag\"]\n");

        let loaded = load(Some(&explicit), &env_at(&dir))
            .expect("the file parses")
            .expect("the file exists");
        assert_eq!(loaded.location.source, Source::Flag);
        assert_eq!(
            loaded.model.exclude.as_deref(),
            Some(["from_flag".to_owned()].as_slice())
        );
    }

    /// HLD §3: `$HOG_CONFIG` pointing at a file that is not there is an error,
    /// not a silent fall-through to the defaults.
    #[test]
    fn load_fails_when_hog_config_points_at_nothing() {
        let dir = TempDir::new();
        let missing = dir.path.join("nothing-here.toml");
        let env = Env {
            hog_config: Some(missing.clone().into_os_string()),
            ..Env::default()
        };
        let err = load(Some(&missing), &env).expect_err("the named file must exist");
        assert!(err.to_string().contains("$HOG_CONFIG"), "{err}");
    }
}
