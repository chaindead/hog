//! The `hog config` subcommand: the five modules below it, wired to argv.
//!
//! Nothing here decides anything on its own — discovery, parsing, editing and
//! writing all live in their own modules and are tested there. This is the
//! place that puts them in order and chooses the words the user reads.
//!
//! # The surface is verbs, not flags (HLD §6, §11.8)
//!
//! | invocation | what it does |
//! |---|---|
//! | `hog config` | the resolved configuration, and the path it came from |
//! | `hog config path` | that path alone |
//! | `hog config edit` | open it in `$VISUAL` / `$EDITOR` |
//! | `hog config exclude` | the persistent exclude list |
//! | `hog config exclude add a,b` | append to it, comments intact |
//! | `hog config exclude rm a` | drop from it |
//! | `hog config command` | the command template |
//! | `hog config command set "…"` | replace it, **after** checking it |
//!
//! # Which stream gets what
//!
//! Every verb that answers a question puts **the answer, and only the answer**,
//! on stdout: `hog config path` prints one path, `hog config exclude` prints one
//! field per line, `hog config command` prints one template. That is what makes
//! `$(hog config path)` and `hog config exclude | wc -l` work. Everything that
//! is commentary — an unknown-key warning, "the list is empty", "this is the
//! built-in default", `hog: created …` on the way into an editor — goes to
//! stderr.
//!
//! The verbs that *change* something report the change on stdout instead, since
//! for those the report is the answer.
//!
//! # Config text is untrusted text
//!
//! Every string this module prints that came out of the file — the template,
//! the exclude list, the field-name candidates, the timezone — goes through
//! [`printable`] first. HLD §3 treats a config file as something that can
//! arrive with a repository (`hog --config ./hog.toml` is the supported way to
//! use one), and `hog config` is precisely the command a careful person runs to
//! *inspect* such a file before trusting it. Printing its bytes raw would let
//! it repaint the terminal of the person auditing it, and a newline inside a
//! value would forge extra rows in a summary whose whole purpose is to be
//! believed. `command set` already reported its argument through `{:?}`; the
//! read-only verbs now spell a control character the same way.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, anyhow, bail};
use toml_edit::{DocumentMut, Item, Value};

use super::discover::{self, Env, Location};
use super::edit::{self, Change};
use super::load::{self, Loaded};
use super::write;
use crate::cli::{ColorChoiceArg, CommandOp, ConfigCmd, ExcludeOp};
use crate::command::{self, CommandError, template};
use crate::settings::{self, Settings, TimeFormat, TimeZoneSpec};

/// Width of the label column in the summary, `path:` through `sort_keys:`.
const LABEL_WIDTH: usize = 10;

/// The top-level key `hog config command set` writes.
const COMMAND_KEY: &str = "command";

/// Environment variables `hog config edit` consults, in order. `$VISUAL` first
/// is the convention every editor-launching tool follows: `$EDITOR` is allowed
/// to be a line editor for a dumb terminal, `$VISUAL` is the full-screen one.
const EDITOR_VARS: [&str; 2] = ["VISUAL", "EDITOR"];

/// `hog config …` as the user typed it, plus the environment it runs in.
///
/// The `env` and `editor` fields are not ceremony: they are what lets a test
/// point discovery at a temporary directory and an editor at a script.
/// `std::env::set_var` is `unsafe` in edition 2024 and this package denies
/// `unsafe_code`, so an environment that is read from inside the call is an
/// environment no test can control.
#[derive(Debug)]
pub struct Request<'a> {
    /// The verb, or `None` for bare `hog config`.
    pub action: Option<&'a ConfigCmd>,
    /// The global `--config` value, which clap also fills from `$HOG_CONFIG`.
    pub explicit: Option<&'a Path>,
    /// The sampled environment discovery searches in.
    pub env: &'a Env,
    /// `$VISUAL` or `$EDITOR`, sampled by [`editor_from_env`]. `None` means
    /// neither is set, which is `hog config edit`'s own error and nobody
    /// else's.
    pub editor: Option<OsString>,
}

/// Reads `$VISUAL`, then `$EDITOR`. Empty counts as unset: a shell that exports
/// `EDITOR=` has no editor, and `Command::new("")` would fail with a message
/// about an empty path instead of one about an editor.
///
/// The one place in this module that touches the process environment, called
/// from the binary's edge so that [`run`] stays a function of its argument.
pub fn editor_from_env() -> Option<OsString> {
    EDITOR_VARS
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
}

/// Runs `hog config`.
///
/// Locating the file comes first for every verb, because each one is about a
/// specific file and cannot say anything useful without naming it. After that
/// the verb decides everything, including whether the file is written at all:
/// `path`, `exclude` and `command` with no operand must never create a
/// directory, a file, or an mtime.
///
/// Diagnostics go to `warnings`; everything a script might parse goes to `out`.
/// Returning `Ok(())` means exit code 0; the failures that carry their own code
/// travel as `crate::Error`.
pub fn run<O: Write, E: Write>(
    request: &Request<'_>,
    out: &mut O,
    warnings: &mut E,
) -> anyhow::Result<()> {
    let location = locate_or_err(request)?;

    match request.action {
        None => summary(&location, out, warnings),

        Some(ConfigCmd::Path) => {
            writeln!(out, "{}", location.path.display())?;
            Ok(())
        }

        Some(ConfigCmd::Edit) => edit_in_editor(request, &location, warnings),

        Some(ConfigCmd::Exclude { op: None }) => show_excludes(&location, out, warnings),
        Some(ConfigCmd::Exclude {
            op: Some(ExcludeOp::Add { fields }),
        }) => edit_excludes(&location, fields, &[], out, warnings),
        Some(ConfigCmd::Exclude {
            op: Some(ExcludeOp::Rm { fields }),
        }) => edit_excludes(&location, &[], fields, out, warnings),

        Some(ConfigCmd::Command { op: None }) => show_command(&location, out, warnings),
        Some(ConfigCmd::Command {
            op: Some(CommandOp::Set { template }),
        }) => set_command(&location, template, out, warnings),
    }
}

/// Resolves the config path, or explains why there is none.
///
/// [`discover::locate`](super::discover::locate) returns `None` when `$HOME`
/// gives no usable base. A normal run treats that as "use the built-in
/// defaults" and says nothing, but `hog config` cannot: every one of its verbs
/// is about a specific file, so it fails with the variable named rather than
/// printing an empty path.
fn locate_or_err(request: &Request<'_>) -> anyhow::Result<Location> {
    discover::locate(request.explicit, request.env).with_context(|| {
        format!(
            "cannot work out where the config file lives: ${} is not set \
             (name the file yourself with `hog --config PATH config`)",
            discover::HOME_VAR,
        )
    })
}

// ============================================================ read-only verbs

/// Bare `hog config`: read the file if there is one, then print the summary.
fn summary<O: Write, E: Write>(
    location: &Location,
    out: &mut O,
    warnings: &mut E,
) -> anyhow::Result<()> {
    let loaded = read_only(location, warnings)?;
    print_summary(location, loaded.as_ref(), out)
}

/// `hog config exclude`: the persistent list, one dotted path per line.
///
/// One per line rather than the summary's comma-separated row, because this is
/// the form a script can consume. Nothing stops a hand-edited file from putting
/// a newline inside a field — TOML has multi-line strings — so the framing is
/// kept unambiguous by [`printable`], which turns that newline into the two
/// characters `\n` rather than into a second line of output.
fn show_excludes<O: Write, E: Write>(
    location: &Location,
    out: &mut O,
    warnings: &mut E,
) -> anyhow::Result<()> {
    let settings = resolved(location, warnings)?;
    if settings.exclude.is_empty() {
        writeln!(
            warnings,
            "nothing is excluded — `hog config exclude add <field>` hides one"
        )?;
        return Ok(());
    }
    for field in settings.exclude.iter() {
        writeln!(out, "{}", printable(field))?;
    }
    Ok(())
}

/// `hog config command`: the template, on one line.
///
/// The provenance goes to stderr rather than into the line, so that
/// `hog config command` pipes into anything. It is worth saying at all because
/// a built-in default and a template the user wrote look identical on stdout,
/// and only one of them survives editing the file.
fn show_command<O: Write, E: Write>(
    location: &Location,
    out: &mut O,
    warnings: &mut E,
) -> anyhow::Result<()> {
    let loaded = read_only(location, warnings)?;
    let from_file = loaded
        .as_ref()
        .is_some_and(|loaded| loaded.model.command.is_some());
    let settings = settings_of(loaded.as_ref())?;

    writeln!(out, "{}", printable(command::command_text(&settings)))?;
    if !from_file {
        writeln!(
            warnings,
            "(built-in default — `hog config command set \"…\"` writes your own)"
        )?;
    }
    Ok(())
}

/// Reads the config without creating anything, reporting unknown keys.
///
/// `Ok(None)` is the ordinary "no config yet" case. A file the user *named*
/// (`--config`, `$HOG_CONFIG`) and that is missing is an error, which is
/// [`load::read`]'s rule and not this module's.
fn read_only<E: Write>(location: &Location, warnings: &mut E) -> anyhow::Result<Option<Loaded>> {
    let loaded = load::read(location.clone())?;
    if let Some(loaded) = &loaded {
        loaded.warn_unknown_keys(warnings);
    }
    Ok(loaded)
}

/// The file folded onto the built-in defaults — what a run with no flags would
/// actually use.
fn resolved<E: Write>(location: &Location, warnings: &mut E) -> anyhow::Result<Settings> {
    let loaded = read_only(location, warnings)?;
    settings_of(loaded.as_ref())
}

fn settings_of(loaded: Option<&Loaded>) -> anyhow::Result<Settings> {
    match loaded {
        Some(loaded) => Ok(settings::from_config(&loaded.model)?),
        None => Ok(Settings::default()),
    }
}

// ================================================================ write verbs

/// `hog config exclude add …` and `hog config exclude rm …`.
///
/// Read once, edited together, written once — and only when something actually
/// moved, so running the same `add` twice does not touch the file.
fn edit_excludes<O: Write, E: Write>(
    location: &Location,
    add: &[String],
    remove: &[String],
    out: &mut O,
    warnings: &mut E,
) -> anyhow::Result<()> {
    let mut document = document_for_edit(location, warnings)?;
    if apply_edits(&mut document, add, remove, out)?.is_applied() {
        save(location, &document)?;
    }
    Ok(())
}

/// `hog config command set "<template>"`.
///
/// The template is checked **before** the file is touched (HLD §3): a template
/// that cannot be split, or whose placeholders have a hole in them, would
/// otherwise surface at the next run, far from the place it was created. The
/// check is [`template::parse`] itself rather than a second copy of the rules,
/// so the two can never disagree about what a valid template is.
fn set_command<O: Write, E: Write>(
    location: &Location,
    command: &str,
    out: &mut O,
    warnings: &mut E,
) -> anyhow::Result<()> {
    check_template(command)?;

    let mut document = document_for_edit(location, warnings)?;
    match write_command(&mut document, command)? {
        Change::Applied => {
            save(location, &document)?;
            writeln!(out, "command set to {command:?}")?;
        }
        Change::Unchanged => writeln!(out, "command was already {command:?}")?,
    }
    Ok(())
}

/// Parses a template purely to find out whether it is usable.
///
/// The error is wrapped in [`CommandError::Template`] so that the refusal reads
/// exactly like the one a run would have produced, hint and all.
fn check_template(command: &str) -> Result<(), CommandError> {
    template::parse(command)
        .map(drop)
        .map_err(|reason| CommandError::Template {
            reason,
            template: command.to_owned(),
        })
}

/// Puts `command = "<template>"` into the document, preserving what is around
/// it.
///
/// An existing key has its **value** replaced and keeps its own decor, which is
/// where `toml_edit` stores the comment block above it — the starter config's
/// explanation of `-tt` and `ServerAliveInterval` is forty lines of it, and
/// `Table::insert` over an existing key would replace the `Key` and take those
/// comments with it.
///
/// A key that is not there yet is appended to the **root table**, and that is
/// the placement the key needs: `command` is a top-level key, so a bare
/// `command = …` written after `[output]` would parse back as `output.command`.
/// `toml_edit` renders a table's own key-values before any of its sub-tables,
/// so appending to the root lands the line above the first `[header]`.
fn write_command(document: &mut DocumentMut, command: &str) -> anyhow::Result<Change> {
    let table = document.as_table_mut();

    let Some(item) = table.get_mut(COMMAND_KEY) else {
        table.insert(COMMAND_KEY, Item::Value(Value::from(command)));
        return Ok(Change::Applied);
    };

    let Some(existing) = item.as_value() else {
        bail!(
            "`{COMMAND_KEY}` is a {} in this config, not a string — \
             remove it by hand before setting a template",
            item.type_name()
        );
    };
    if existing.as_str() == Some(command) {
        return Ok(Change::Unchanged);
    }

    let mut value = Value::from(command);
    *value.decor_mut() = existing.decor().clone();
    *item = Item::Value(value);
    Ok(Change::Applied)
}

/// Reads the config that a write verb is about to edit, or seeds a new one.
///
/// A missing file is not an error here — `hog config exclude add trace_id` on a
/// fresh machine is a perfectly ordinary first command. It produces
/// [`edit::starter_document`](super::edit::starter_document), so the user ends
/// up with the fully commented starter carrying their field, instead of a
/// one-line file that explains nothing.
///
/// This is also the one place a missing `--config` / `$HOG_CONFIG` file is not
/// an error: the command was asked to write that exact file, so creating it is
/// the request, not a silent substitution of different settings.
///
/// A file that exists but does not parse **is** an error: appending to a
/// document we could not read would mean rewriting it from our own idea of what
/// it said, and the comments would be the first casualty.
fn document_for_edit<E: Write>(
    location: &Location,
    warnings: &mut E,
) -> anyhow::Result<DocumentMut> {
    let text = match fs::read_to_string(&location.path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return edit::starter_document()
                .context("the starter config compiled into this binary does not parse");
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to read the config file {}", location.path.display())
            });
        }
    };

    let loaded = load::parse(&text, location.clone())?;
    loaded.warn_unknown_keys(warnings);
    Ok(loaded.document)
}

/// Creates the parent directory and writes the document atomically.
fn save(location: &Location, document: &DocumentMut) -> anyhow::Result<()> {
    write::ensure_parent_dir(&location.path)?;
    write::save(&location.path, document)
        .with_context(|| format!("saving {}", location.path.display()))
}

/// Applies every removal and then every addition, reporting each one.
///
/// Removals run first so that the addition wins a contradictory
/// `hog config exclude add x` after an `rm x` in the same breath — which today
/// only happens through the unit tests, since the two are separate verbs now.
///
/// One line per field, on `out`:
///
/// ```text
/// added `grpc.code`
/// `grpc.code` was already excluded
/// removed `trace_id`
/// `trace_id` was not excluded
/// ```
///
/// Returns [`Change::Applied`] if any field actually moved, which is what tells
/// the caller whether to write the file.
fn apply_edits<W: Write + ?Sized>(
    document: &mut DocumentMut,
    add: &[String],
    remove: &[String],
    out: &mut W,
) -> anyhow::Result<Change> {
    let mut overall = Change::Unchanged;

    let removals = remove.iter().map(|field| (field, false));
    let additions = add.iter().map(|field| (field, true));

    for (field, adding) in removals.chain(additions) {
        let change = if adding {
            edit::append_exclude(document, field)?
        } else {
            edit::remove_exclude(document, field)?
        };
        // Escaped for the same reason the read-only verbs are: this echoes back
        // a field name, and a field name can carry a newline or an ESC just as
        // easily from argv as from the file.
        let name = printable(field.trim());
        match (adding, change) {
            (true, Change::Applied) => writeln!(out, "added `{name}`")?,
            (true, Change::Unchanged) => writeln!(out, "`{name}` was already excluded")?,
            (false, Change::Applied) => writeln!(out, "removed `{name}`")?,
            (false, Change::Unchanged) => writeln!(out, "`{name}` was not excluded")?,
        }
        if change.is_applied() {
            overall = Change::Applied;
        }
    }

    Ok(overall)
}

// ======================================================================= edit

/// `hog config edit`: hand the file to the user's editor, then check what came
/// back.
///
/// Three things happen around the editor, and each one is there because of a
/// way the naive version wastes the user's time:
///
/// 1. **the file is seeded first.** Opening an editor on a path that does not
///    exist gives an empty buffer, and the user writes a config from memory;
///    the starter *is* the documentation of the format (HLD §3). The default
///    file is already there by now — [`config::ensure_default`](super::ensure_default)
///    created it at the top of the run — so what this actually covers is a
///    `--config` path the user named and wants opened.
/// 2. **the editor's own exit status is honoured.** `vi` exiting non-zero
///    usually means it never wrote anything, so saying "done" would be a lie.
/// 3. **the result is parsed.** A typo is reported now, with the file and the
///    line, while the user is still at the keyboard — rather than at the next
///    `hog`, when they are looking at something else.
///
/// Nothing is written to stdout: `hog config edit` answers no question, and its
/// commentary shares a terminal with a full-screen editor.
fn edit_in_editor<E: Write>(
    request: &Request<'_>,
    location: &Location,
    warnings: &mut E,
) -> anyhow::Result<()> {
    let Some((program, args)) = request.editor.as_deref().and_then(editor_argv) else {
        return Err(no_editor(&location.path));
    };

    super::create_starter(&location.path, warnings)?;

    let status = Command::new(&program)
        .args(&args)
        .arg(&location.path)
        .status()
        .with_context(|| {
            format!(
                "cannot run the editor `{}` (from ${} or ${})",
                program.to_string_lossy(),
                EDITOR_VARS[0],
                EDITOR_VARS[1],
            )
        })?;

    // Deliberately not "nothing was saved": an editor that exits non-zero may
    // still have written the file. What hog can honestly say is that it stopped
    // before checking it.
    if !status.success() {
        bail!(
            "the editor `{}` exited with status {}, so {} was not checked",
            program.to_string_lossy(),
            status.code().map_or_else(
                || "unknown (killed by a signal)".to_owned(),
                |code| code.to_string()
            ),
            location.path.display()
        );
    }

    check_after_edit(location, warnings)
}

/// Splits `$EDITOR` into a program and its arguments.
///
/// `EDITOR` is a command line, not a path — `code -w`, `emacs -nw` and
/// `subl --wait` are all ordinary values — so it is split by the same shell
/// rules the `command` template uses. A value that is not UTF-8 cannot be split
/// and is taken as a bare program name, which is the only reading left; a value
/// that is only whitespace, or that has an unclosed quote in it, is no editor
/// at all and the caller reports it as an unset one.
fn editor_argv(editor: &OsStr) -> Option<(OsString, Vec<OsString>)> {
    let Some(text) = editor.to_str() else {
        return Some((editor.to_os_string(), Vec::new()));
    };
    let mut words = shlex::split(text)?.into_iter();
    let program = words.next()?;
    Some((program.into(), words.map(OsString::from).collect()))
}

/// The refusal when neither `$VISUAL` nor `$EDITOR` is set.
///
/// Names both variables, one way to set one, and the path — so that the user
/// who does not want to set a variable at all still leaves with what they came
/// for.
fn no_editor(path: &Path) -> anyhow::Error {
    anyhow!(
        "no editor configured\n\n  \
         `hog config edit` opens the config file in ${} or ${}, and neither is set.\n\n  \
         Set one for this shell:     export {}=vi\n  \
         Or open the file yourself:  {}",
        EDITOR_VARS[0],
        EDITOR_VARS[1],
        EDITOR_VARS[1],
        path.display(),
    )
}

/// Re-reads the file the editor just wrote and reports what is wrong with it.
///
/// Three checks, in the order a run would hit them: the TOML parses, the values
/// are ones hog can honour, and the `command` template is usable. The first two
/// are [`load::parse`]'s and [`settings::from_config`]'s own errors, so the
/// message is the same one the next run would have printed — including the
/// `path:line:` prefix an editor can jump to.
fn check_after_edit<E: Write>(location: &Location, warnings: &mut E) -> anyhow::Result<()> {
    let text = fs::read_to_string(&location.path)
        .with_context(|| format!("failed to read back {}", location.path.display()))?;
    let loaded = load::parse(&text, location.clone())?;
    loaded.warn_unknown_keys(warnings);

    let settings = settings::from_config(&loaded.model)?;
    if let Some(command) = settings.command.as_deref() {
        check_template(command)?;
    }
    Ok(())
}

// ==================================================================== summary

/// Prints what `hog config` with no verb says.
///
/// The path first, because it is the question people actually have, then where
/// it came from and whether the file is there at all, then the configuration as
/// hog resolved it — the file folded onto the built-in defaults, which is
/// exactly what a run with no flags would use:
///
/// ```text
/// path:      /Users/you/.hog.toml
/// source:    $HOME
/// file:      loaded
/// command:   ssh -tt {0} 'docker logs -f myapp-{1}-1'
/// exclude:   grpc.code, trace_id
/// ts:        ts, time, timestamp, @timestamp
/// level:     level, severity, lvl
/// msg:       msg, message
/// time:      %H:%M:%S, local
/// color:     auto
/// sort_keys: true
/// ```
///
/// `loaded` is `None` when there is no file — which, since the run creates one,
/// means it could not be created. Every line still prints, showing the built-in
/// defaults, and the `file:` line says what happened.
fn print_summary<W: Write>(
    location: &Location,
    loaded: Option<&Loaded>,
    out: &mut W,
) -> anyhow::Result<()> {
    let settings = settings_of(loaded)?;

    line(out, "path", location.path.display())?;
    line(out, "source", location.source.label())?;
    match loaded {
        Some(_) => line(out, "file", "loaded")?,
        // Rare, and worth saying plainly: the run already tried to create this
        // file and could not — a read-only `$HOME` is the usual reason, and it
        // printed why on stderr on its way past. The settings below are the
        // built-in defaults, which is exactly what the file would have said.
        None => line(out, "file", "not there (hog could not create it)")?,
    }
    // The effective template, which is the built-in `echo {@}` until the file
    // sets one (HLD §5). Saying which of the two it is matters: the default one
    // disappears the moment a `command` key appears in the file.
    let command = printable(command::command_text(&settings));
    if settings.command.is_some() {
        line(out, "command", command)?;
    } else {
        line(out, "command", format_args!("{command}  (built-in)"))?;
    }
    line(out, "exclude", joined(settings.exclude.iter()))?;
    line(out, "ts", joined(field_names(&settings.fields.ts)))?;
    line(out, "level", joined(field_names(&settings.fields.level)))?;
    line(out, "msg", joined(field_names(&settings.fields.msg)))?;
    line(out, "time", describe_time(&settings))?;
    line(out, "color", color_name(settings.color))?;
    line(out, "sort_keys", settings.sort_keys)?;
    // Only when the file sets it: a row that always reads `(none)` is noise in
    // a summary whose whole job is to be read at a glance.
    if !settings.levels.is_empty() {
        let aliases: Vec<String> = settings
            .levels
            .iter()
            .map(|(from, to)| format!("{} -> {}", printable(from), printable(to)))
            .collect();
        line(out, "levels", joined(aliases.iter().map(String::as_str)))?;
    }

    Ok(())
}

/// The colour choice as the config file spells it.
///
/// Written out rather than derived from `Debug`: what this line prints is a
/// value the user can paste straight back into `output.color`, and `Debug` is
/// not a contract.
fn color_name(color: ColorChoiceArg) -> &'static str {
    match color {
        ColorChoiceArg::Auto => "auto",
        ColorChoiceArg::Always => "always",
        ColorChoiceArg::Never => "never",
    }
}

/// Escapes the control characters in a string that came from the config file,
/// leaving everything else exactly as written.
///
/// The spelling is the one `{:?}` uses — `\n`, `\t`, `\u{1b}` — because
/// `command set` already reports its argument that way (`command set to "…"`),
/// and one CLI should name a hostile byte with one word. Unlike `{:?}` this
/// adds no surrounding quotes: `hog config command` and `hog config exclude`
/// are documented to put the bare value on stdout so that `$(…)` works.
///
/// Borrowed whenever there is nothing to escape, which is every real config, so
/// the common path neither allocates nor changes a byte of the output.
///
/// This is deliberately *not* [`crate::help::one_line`]: that one flattens a
/// template onto a single line for a fixed-width help block and may collapse
/// runs of whitespace, which is right for a layout and wrong for a value a
/// script is about to read.
fn printable(text: &str) -> Cow<'_, str> {
    if !text.contains(char::is_control) {
        return Cow::Borrowed(text);
    }

    let mut escaped = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if character.is_control() {
            // `escape_debug` on a single control character yields exactly the
            // `\n` / `\u{1b}` forms `{:?}` would have produced for it.
            escaped.extend(character.escape_debug());
        } else {
            escaped.push(character);
        }
    }
    Cow::Owned(escaped)
}

/// One `label:   value` row of the summary.
///
/// The padding is computed rather than written as `{:<width$}` around a
/// `format_args!`, which is a silent no-op, and rather than around a
/// `format!("{label}:")`, which would allocate a string per row to throw it
/// away again. The colon counts towards the label's width.
fn line<W: Write>(out: &mut W, label: &str, value: impl std::fmt::Display) -> io::Result<()> {
    let pad = LABEL_WIDTH.saturating_sub(label.len() + 1);
    writeln!(out, "{label}:{:pad$} {value}", "")
}

/// A comma-separated list, or `(none)` when there is nothing in it.
///
/// Every element goes through [`printable`]: these are field names read out of
/// the config file, and a newline in one of them would otherwise split the row
/// and forge a second label in the summary.
fn joined<'a, I: Iterator<Item = &'a str>>(values: I) -> String {
    let mut text = String::new();
    for value in values {
        if !text.is_empty() {
            text.push_str(", ");
        }
        text.push_str(&printable(value));
    }
    if text.is_empty() {
        text.push_str("(none)");
    }
    text
}

fn field_names(names: &[String]) -> impl Iterator<Item = &str> {
    names.iter().map(String::as_str)
}

/// `%H:%M:%S, local` — the two timestamp keys on one line, since neither is
/// interesting without the other.
fn describe_time(settings: &Settings) -> String {
    let mut text = String::new();
    match &settings.time.format {
        TimeFormat::Hidden => return "none (no timestamp column)".to_owned(),
        TimeFormat::Raw => text.push_str("raw (printed byte for byte)"),
        TimeFormat::Strftime(pattern) => text.push_str(&printable(pattern)),
    }
    let zone = match &settings.time.zone {
        TimeZoneSpec::Local => Cow::Borrowed("local"),
        TimeZoneSpec::Utc => Cow::Borrowed("utc"),
        TimeZoneSpec::Named(name) => printable(name),
    };
    // The format string above cannot fail and neither can this: writing into a
    // `String` is infallible, and `write!` is only how `fmt` composes.
    let _ = write!(text, ", {zone}");
    text
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panicking test is a failing test")]
mod tests {
    use std::iter;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::super::discover::{CONFIG_FILE, Source};
    use super::*;

    /// A temp directory that removes itself (`test-fixture-raii`).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("hog-cmd-{tag}-{}-{serial}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the temp directory is creatable");
            Self {
                path: path.canonicalize().expect("the temp directory resolves"),
            }
        }

        /// The config path discovery will compute for this directory.
        fn config(&self) -> PathBuf {
            self.path.join(CONFIG_FILE)
        }

        fn env(&self) -> Env {
            Env {
                hog_config: None,
                home: Some(OsString::from(self.path.clone())),
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Runs one verb, returning (stdout, stderr).
    ///
    /// No test here uses [`ConfigCmd::Edit`] on purpose: it spawns whatever the
    /// developer's `$EDITOR` happens to be, so the editor path is covered by
    /// `tests/config_cli.rs` with a script instead. The refusal when there is
    /// no editor at all is testable here, because it needs no process.
    fn config(env: &Env, action: Option<&ConfigCmd>) -> anyhow::Result<(String, String)> {
        let request = Request {
            action,
            explicit: None,
            env,
            editor: None,
        };
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        run(&request, &mut out, &mut warnings)?;
        Ok((
            String::from_utf8(out).expect("stdout is UTF-8"),
            String::from_utf8(warnings).expect("stderr is UTF-8"),
        ))
    }

    fn fields(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn add(values: &[&str]) -> ConfigCmd {
        ConfigCmd::Exclude {
            op: Some(ExcludeOp::Add {
                fields: fields(values),
            }),
        }
    }

    fn remove(values: &[&str]) -> ConfigCmd {
        ConfigCmd::Exclude {
            op: Some(ExcludeOp::Rm {
                fields: fields(values),
            }),
        }
    }

    fn set(template: &str) -> ConfigCmd {
        ConfigCmd::Command {
            op: Some(CommandOp::Set {
                template: template.to_owned(),
            }),
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("the config is readable")
    }

    /// Puts the starter in place, the way a real run's
    /// [`ensure_default`](super::super::ensure_default) would have before any
    /// verb ran.
    ///
    /// None of the verbs create the default file themselves any more, so a test
    /// about editing an existing config has to say so.
    fn seed(dir: &TempDir) {
        super::super::create_starter(&dir.config(), &mut Vec::new())
            .expect("the starter is writable");
    }

    // ------------------------------------------------------------------- path

    /// The verb creates nothing, and that is still its own rule rather than an
    /// accident of the run: `hog config path` answers a question, and the file
    /// it names is brought into existence above this module.
    #[test]
    fn path_prints_the_file_and_nothing_else() {
        let dir = TempDir::new("path");
        let (out, warnings) =
            config(&dir.env(), Some(&ConfigCmd::Path)).expect("`config path` succeeds");
        assert_eq!(out, format!("{}\n", dir.config().display()));
        assert!(warnings.is_empty(), "{warnings}");
        assert!(
            !dir.config().exists(),
            "`config path` must not create anything"
        );
    }

    // --------------------------------------------------------------- excludes

    /// The first `hog config exclude add` on a fresh machine seeds the
    /// commented starter, so the user gets the documentation of the format too.
    #[test]
    fn a_first_append_seeds_the_starter_with_the_field_in_it() {
        let dir = TempDir::new("seed");
        let (out, _) = config(&dir.env(), Some(&add(&["trace_id"]))).expect("the add succeeds");

        assert_eq!(out, "added `trace_id`\n");
        let written = read(&dir.config());
        assert_eq!(written, edit::STARTER.replace("[]", "[\"trace_id\"]"));
        assert!(
            written.contains("# exclude = [\n#   \"serviceName\""),
            "the commented example survived: {written}"
        );
    }

    #[test]
    fn appends_and_removals_report_every_field() {
        let dir = TempDir::new("edits");
        config(&dir.env(), Some(&add(&["a", "b"]))).expect("the add succeeds");

        let (out, _) = config(&dir.env(), Some(&remove(&["a", "zz"]))).expect("the rm succeeds");
        assert_eq!(out, "removed `a`\n`zz` was not excluded\n");

        let (out, _) = config(&dir.env(), Some(&add(&["b", "c"]))).expect("the add succeeds");
        assert_eq!(out, "`b` was already excluded\nadded `c`\n");
        assert!(read(&dir.config()).contains("exclude = [\"b\", \"c\"]"));
    }

    /// An append that changes nothing must not rewrite the file.
    #[test]
    fn an_unchanged_edit_leaves_the_file_alone() {
        let dir = TempDir::new("mtime");
        config(&dir.env(), Some(&add(&["a"]))).expect("the add succeeds");
        let before = fs::metadata(dir.config())
            .and_then(|meta| meta.modified())
            .expect("mtime is readable");

        config(&dir.env(), Some(&add(&["a"]))).expect("a repeat succeeds");

        let after = fs::metadata(dir.config())
            .and_then(|meta| meta.modified())
            .expect("mtime is readable");
        assert_eq!(before, after, "the file was rewritten for nothing");
    }

    #[test]
    fn a_broken_config_stops_an_edit_rather_than_rewriting_it() {
        let dir = TempDir::new("broken");
        let path = dir.config();
        fs::create_dir_all(path.parent().expect("the config has a parent"))
            .expect("the directory is creatable");
        fs::write(&path, "exclude = [\n[output\n").expect("the setup write succeeds");

        let err = config(&dir.env(), Some(&add(&["a"]))).expect_err("a broken file fails");
        assert!(err.to_string().contains(".hog.toml:"), "{err}");
        assert_eq!(
            read(&path),
            "exclude = [\n[output\n",
            "the unreadable file was left exactly as it was"
        );
    }

    /// `hog config exclude` with no operand reads and never writes.
    #[test]
    fn showing_the_exclude_list_prints_one_field_per_line() {
        let dir = TempDir::new("show-exclude");

        let (out, warnings) = config(&dir.env(), Some(&ConfigCmd::Exclude { op: None }))
            .expect("an empty list is not an error");
        assert!(out.is_empty(), "stdout carried a note: {out:?}");
        assert!(warnings.contains("nothing is excluded"), "{warnings}");
        assert!(!dir.config().exists(), "showing the list created a file");

        config(&dir.env(), Some(&add(&["b", "a"]))).expect("the add succeeds");
        let (out, warnings) =
            config(&dir.env(), Some(&ConfigCmd::Exclude { op: None })).expect("the list prints");
        assert_eq!(
            out, "a\nb\n",
            "the resolved set is sorted, like `ExcludeSet`"
        );
        assert!(warnings.is_empty(), "{warnings}");
    }

    // ---------------------------------------------------------------- command

    /// stdout carries the template and nothing else, so `$(hog config command)`
    /// is a template. With no file there is still one — HLD §5's built-in
    /// `echo {@}` — and the fact that it is the built-in goes to stderr, where
    /// it cannot end up inside the substitution.
    #[test]
    fn showing_the_command_prints_the_template_alone() {
        let dir = TempDir::new("show-command");

        let (out, warnings) = config(&dir.env(), Some(&ConfigCmd::Command { op: None }))
            .expect("a missing template is not an error");
        assert_eq!(out, format!("{}\n", command::DEFAULT_COMMAND));
        assert!(warnings.contains("built-in default"), "{warnings}");
        assert!(
            !dir.config().exists(),
            "showing the template created a file"
        );

        config(&dir.env(), Some(&set("ssh {0} 'logs {1}'"))).expect("the set succeeds");
        let (out, warnings) = config(&dir.env(), Some(&ConfigCmd::Command { op: None }))
            .expect("the template prints");
        assert_eq!(out, "ssh {0} 'logs {1}'\n");
        assert!(warnings.is_empty(), "{warnings}");
    }

    /// The rule of HLD §3: a template that could only fail later never reaches
    /// the file.
    #[test]
    fn a_broken_template_is_refused_before_anything_is_written() {
        let dir = TempDir::new("bad-template");
        seed(&dir);

        for (template, needle) in [
            ("ssh {0} 'docker logs", "unclosed quote"),
            ("ssh {0} {2}", "uses {2} but never {1}"),
            ("   ", "empty"),
        ] {
            let err = config(&dir.env(), Some(&set(template))).expect_err("must be refused");
            let message = err.to_string();
            assert!(message.contains(needle), "{template:?}: {message}");
            assert_eq!(
                read(&dir.config()),
                edit::STARTER,
                "{template:?} reached the file"
            );
        }
    }

    /// A refused template must not create a config file either — the check runs
    /// before the document is read or seeded.
    #[test]
    fn a_refused_template_does_not_create_a_file() {
        let dir = TempDir::new("bad-template-fresh");
        config(&dir.env(), Some(&set("ssh '"))).expect_err("must be refused");
        assert!(!dir.config().exists(), "a refusal created a config file");
    }

    #[test]
    fn setting_the_command_writes_the_key_and_keeps_every_comment() {
        let dir = TempDir::new("set-command");
        seed(&dir);

        // The starter ships `command` commented out, so the built-in `echo {@}`
        // is what runs until someone sets their own. Setting one has to add the
        // key without disturbing the example or the explanation around it.
        let before = read(&dir.config());
        assert!(
            !before.contains("\ncommand = "),
            "the starter must not ship an active command key: {before}"
        );

        let (out, _) =
            config(&dir.env(), Some(&set("kubectl logs -f -l app=api"))).expect("the set succeeds");
        assert_eq!(out, "command set to \"kubectl logs -f -l app=api\"\n");

        let written = read(&dir.config());
        assert!(
            written.contains("command = \"kubectl logs -f -l app=api\"\n"),
            "{written}"
        );
        assert!(
            written.contains("# -o ServerAliveInterval=15"),
            "the forty lines of explanation above the key survived: {written}"
        );
        assert!(
            written.contains("# command = \"ssh -tt"),
            "the commented-out example survived: {written}"
        );
    }

    #[test]
    fn setting_the_same_command_twice_does_not_rewrite_the_file() {
        let dir = TempDir::new("set-same");
        config(&dir.env(), Some(&set("echo {0}"))).expect("the set succeeds");
        let before = fs::metadata(dir.config())
            .and_then(|meta| meta.modified())
            .expect("mtime is readable");

        let (out, _) = config(&dir.env(), Some(&set("echo {0}"))).expect("a repeat succeeds");
        assert_eq!(out, "command was already \"echo {0}\"\n");
        let after = fs::metadata(dir.config())
            .and_then(|meta| meta.modified())
            .expect("mtime is readable");
        assert_eq!(before, after, "the file was rewritten for nothing");
    }

    /// `command` is a top-level key: written after `[output]` it would parse
    /// back as `output.command`, which is the exact mistake HLD §3 records.
    #[test]
    fn a_command_added_to_a_file_full_of_tables_lands_above_them() {
        let mut document: DocumentMut = "exclude = [\"a\"]\n\n[output]\ncolor = \"auto\"\n"
            .parse()
            .expect("the fixture parses");

        assert_eq!(
            write_command(&mut document, "ssh {0}").expect("the key is writable"),
            Change::Applied
        );

        let text = document.to_string();
        let command_at = text.find("command =").expect("the key was written");
        let table_at = text.find("[output]").expect("the table is still there");
        assert!(command_at < table_at, "the key landed in [output]:\n{text}");

        let reparsed: DocumentMut = text.parse().expect("the result parses");
        assert_eq!(
            reparsed["command"].as_str(),
            Some("ssh {0}"),
            "the key is not top-level:\n{text}"
        );
    }

    /// A `command` that is a table rather than a string is the user's, and hog
    /// will not silently flatten it.
    #[test]
    fn a_command_that_is_not_a_string_is_refused() {
        let mut document: DocumentMut = "[command]\nname = \"ssh\"\n"
            .parse()
            .expect("the fixture parses");
        let err = write_command(&mut document, "ssh {0}").expect_err("a table is not a template");
        assert!(err.to_string().contains("not a string"), "{err}");
    }

    // ----------------------------------------------------------------- editor

    #[test]
    fn without_an_editor_the_error_names_both_variables_and_the_file() {
        let dir = TempDir::new("no-editor");
        let err = config(&dir.env(), Some(&ConfigCmd::Edit)).expect_err("there is no editor");
        let message = err.to_string();
        assert!(message.contains("$VISUAL"), "{message}");
        assert!(message.contains("$EDITOR"), "{message}");
        assert!(message.contains(CONFIG_FILE), "{message}");
        assert!(
            !dir.config().exists(),
            "the refusal created a file it never opened"
        );
    }

    #[test]
    fn an_editor_is_a_command_line_not_a_path() {
        let (program, args) = editor_argv(OsStr::new("code -w --new-window")).expect("splits");
        assert_eq!(program, OsStr::new("code"));
        assert_eq!(args, ["-w", "--new-window"]);

        let (program, args) = editor_argv(OsStr::new("vi")).expect("splits");
        assert_eq!(program, OsStr::new("vi"));
        assert!(args.is_empty());

        // A path with a space in it, quoted the way a shell would want it.
        let (program, args) =
            editor_argv(OsStr::new("'/Applications/My Editor' --wait")).expect("splits");
        assert_eq!(program, OsStr::new("/Applications/My Editor"));
        assert_eq!(args, ["--wait"]);
    }

    #[test]
    fn an_editor_that_is_only_whitespace_or_unbalanced_is_no_editor() {
        assert!(editor_argv(OsStr::new("   ")).is_none());
        assert!(editor_argv(OsStr::new("vi '")).is_none());
    }

    // ---------------------------------------------------------------- summary

    /// No file, which for a real run means hog could not create one. Every row
    /// still prints, showing the built-in defaults, and `file:` says why.
    #[test]
    fn the_summary_names_the_path_the_source_and_the_defaults() {
        let dir = TempDir::new("summary-empty");
        let (out, _) = config(&dir.env(), None).expect("the summary succeeds");

        assert!(out.contains(&dir.config().display().to_string()), "{out}");
        assert!(out.contains("source:    $HOME"), "{out}");
        assert!(out.contains("file:      not there"), "{out}");
        assert!(out.contains("exclude:   (none)"), "{out}");
        assert!(
            out.contains("ts:        ts, time, timestamp, @timestamp"),
            "{out}"
        );
        assert!(out.contains("time:      %H:%M:%S, local"), "{out}");
        assert!(out.contains("color:     auto"), "{out}");
        assert!(out.contains("sort_keys: true"), "{out}");
    }

    #[test]
    fn the_summary_shows_what_the_file_changed() {
        let dir = TempDir::new("summary-file");
        seed(&dir);
        config(&dir.env(), Some(&add(&["trace_id"]))).expect("the add succeeds");

        let (out, _) = config(&dir.env(), None).expect("the summary succeeds");
        assert!(out.contains("file:      loaded"), "{out}");
        assert!(out.contains("exclude:   trace_id"), "{out}");
        // The starter leaves `command` commented out, so a freshly initialised
        // file still reports the built-in default — and says so, rather than
        // letting it pass for something the user chose.
        assert!(out.contains("command:   echo {@}  (built-in)"), "{out}");

        config(&dir.env(), Some(&set("kubectl logs -f -l app=api"))).expect("the set succeeds");
        let (out, _) = config(&dir.env(), None).expect("the summary succeeds");
        assert!(
            out.contains("command:   kubectl logs -f -l app=api"),
            "{out}"
        );
        assert!(!out.contains("(built-in)"), "{out}");
    }

    /// An unknown key is a warning with a line number, on stderr, and the
    /// command still does its job (HLD §7.2).
    #[test]
    fn an_unknown_key_warns_with_its_line_and_does_not_fail() {
        let dir = TempDir::new("unknown");
        let path = dir.config();
        fs::create_dir_all(path.parent().expect("the config has a parent"))
            .expect("the directory is creatable");
        fs::write(&path, "exclude = []\n\nfrom_the_future = 1\n")
            .expect("the setup write succeeds");

        let (out, warnings) = config(&dir.env(), None).expect("it still works");
        assert!(out.contains("file:      loaded"), "{out}");
        assert!(
            warnings.contains(":3: unknown key `from_the_future`"),
            "{warnings}"
        );
    }

    #[test]
    fn nowhere_to_look_is_an_error_that_names_the_variable() {
        let env = Env::default();
        let request = Request {
            action: Some(&ConfigCmd::Path),
            explicit: None,
            env: &env,
            editor: None,
        };
        let err = run(&request, &mut Vec::new(), &mut Vec::new()).expect_err("nowhere to look");
        let message = format!("{err:#}");
        assert!(message.contains("$HOME"), "{message}");
        assert!(message.contains("--config"), "{message}");
    }

    #[test]
    fn an_explicit_path_is_created_by_an_edit_rather_than_refused() {
        let dir = TempDir::new("explicit");
        let path = dir.path.join("ci.toml");
        let env = Env::default();
        let action = add(&["trace_id"]);
        let request = Request {
            action: Some(&action),
            explicit: Some(&path),
            env: &env,
            editor: None,
        };
        run(&request, &mut Vec::new(), &mut Vec::new()).expect("the add creates the named file");
        assert!(read(&path).contains("exclude = [\"trace_id\"]"));
    }

    /// The summary, unlike an edit, must not invent a file the user named.
    #[test]
    fn a_summary_for_a_missing_explicit_file_is_an_error() {
        let dir = TempDir::new("explicit-missing");
        let path = dir.path.join("nope.toml");
        let env = Env::default();
        let request = Request {
            action: None,
            explicit: Some(&path),
            env: &env,
            editor: None,
        };
        let err =
            run(&request, &mut Vec::new(), &mut Vec::new()).expect_err("the named file must exist");
        assert!(err.to_string().contains("--config"), "{err}");
    }

    #[test]
    fn a_hidden_timestamp_column_is_spelled_out() {
        let mut settings = Settings::default();
        settings.time.format = TimeFormat::Hidden;
        assert!(describe_time(&settings).starts_with("none"));
        settings.time.format = TimeFormat::Raw;
        settings.time.zone = TimeZoneSpec::Utc;
        assert_eq!(describe_time(&settings), "raw (printed byte for byte), utc");
    }

    #[test]
    fn an_empty_list_prints_as_none() {
        assert_eq!(joined(iter::empty()), "(none)");
        assert_eq!(joined(["a", "b"].into_iter()), "a, b");
    }

    // ============================ config text is untrusted text (see the docs)

    /// A control character survives into the file as a TOML escape, so this is
    /// reachable without hand-editing anything: `` is ordinary TOML.
    const HOSTILE: &str = concat!(
        "exclude = [\"ok\", \"bad\\u001B[31m\", \"nl\\u000Ahidden\"]\n",
        "command = \"echo \\u001B[31mRED\\u0007 {@}\"\n",
        "[fields]\n",
        "ts = [\"t\\u001B[32ms\"]\n",
        "[output]\n",
        // Not `time_zone`: that one is checked against the real zone database
        // before anything prints it, and the refusal already escapes. The
        // format string has no such gate, so it is the one that can carry ESC
        // all the way to the summary.
        "time_format = \"%H\\u001B[33m:%M\"\n",
    );

    fn hostile_config(tag: &str) -> TempDir {
        let dir = TempDir::new(tag);
        let path = dir.config();
        fs::create_dir_all(path.parent().expect("the config has a parent"))
            .expect("the setup mkdir succeeds");
        fs::write(&path, HOSTILE).expect("the setup write succeeds");
        dir
    }

    /// The summary is what a careful person runs *before* trusting a config
    /// that arrived with a repository. It must not be repaintable by the file
    /// it is reporting on.
    #[test]
    fn a_hostile_config_cannot_repaint_the_summary() {
        let dir = hostile_config("hostile-summary");
        let (out, _) = config(&dir.env(), None).expect("the summary still works");

        assert!(!out.contains('\u{1b}'), "raw ESC reached stdout: {out:?}");
        assert!(!out.contains('\u{7}'), "raw BEL reached stdout: {out:?}");
        assert!(out.contains("\\u{1b}[31mRED"), "{out:?}");
        assert!(out.contains("nl\\nhidden"), "{out:?}");
        assert!(out.contains("t\\u{1b}[32ms"), "{out:?}");
        assert!(out.contains("%H\\u{1b}[33m:%M"), "{out:?}");
    }

    /// The forgery the escaping exists to stop: a newline inside a value would
    /// otherwise print a row that looks exactly like one of the summary's own
    /// labels, and contradict the real row printed below it.
    #[test]
    fn a_newline_in_a_value_cannot_forge_a_summary_row() {
        let dir = TempDir::new("hostile-forge");
        let path = dir.config();
        fs::create_dir_all(path.parent().expect("the config has a parent"))
            .expect("the setup mkdir succeeds");
        fs::write(
            &path,
            "command = \"echo \\u000Aexclude:   EVERYTHING\\u000A {@}\"\n",
        )
        .expect("the setup write succeeds");

        let (out, _) = config(&dir.env(), None).expect("the summary still works");

        let forged = out
            .lines()
            .filter(|line| line.starts_with("exclude:"))
            .count();
        assert_eq!(forged, 1, "the file forged an `exclude:` row:\n{out}");
    }

    /// `hog config exclude` and `hog config command` are documented to put one
    /// value per line on stdout so `$(…)` works. A control character must not
    /// be able to break that framing either.
    #[test]
    fn the_machine_readable_verbs_keep_one_value_per_line() {
        let dir = hostile_config("hostile-verbs");

        let action = ConfigCmd::Exclude { op: None };
        let (out, _) = config(&dir.env(), Some(&action)).expect("the list still works");
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert_eq!(out.lines().count(), 3, "one line per field: {out:?}");

        let action = ConfigCmd::Command { op: None };
        let (out, _) = config(&dir.env(), Some(&action)).expect("the template still works");
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert_eq!(out.lines().count(), 1, "one line, one template: {out:?}");
    }

    /// The escaping must be invisible to every config that is not an attack:
    /// `printable` borrows, and the bytes are the ones that were written.
    #[test]
    fn ordinary_text_is_passed_through_untouched() {
        for text in [
            "",
            "grpc.request.deadline",
            "ssh -tt {0} 'docker logs -f {@}'",
            "%H:%M:%S",
            "Europe/Moscow",
            "ключ",
        ] {
            assert!(matches!(printable(text), Cow::Borrowed(_)), "{text:?}");
            assert_eq!(printable(text), text);
        }
        assert_eq!(printable("a\nb"), "a\\nb");
        assert_eq!(printable("a\tb"), "a\\tb");
        assert_eq!(printable("a\u{1b}b"), "a\\u{1b}b");
        assert_eq!(printable("a\u{0}b"), "a\\0b");
    }

    #[test]
    fn the_source_label_is_the_knob_the_user_would_turn() {
        let dir = TempDir::new("source");
        let location = Location {
            path: dir.config(),
            source: Source::Flag,
        };
        let mut out = Vec::new();
        print_summary(&location, None, &mut out).expect("the summary writes");
        let text = String::from_utf8(out).expect("the summary is UTF-8");
        assert!(text.contains("source:    --config"), "{text}");
    }
}
