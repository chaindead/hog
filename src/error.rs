//! Domain errors and the exit-code table.
//!
//! Two layers, per HLD §2:
//!
//! * [`Error`] — a `thiserror` enum holding exactly those failures that map to
//!   **distinct exit codes**. The mapping is a `match` on the enum; it is never
//!   derived from error text.
//! * everything else — `anyhow::Error` with `.context()` along the way.
//!
//! ```text
//!   0   success / input stream ended
//!   1   runtime error (config, template, bad argument)
//!   2   usage error (clap, or "stdin is a terminal")
//! 127   the configured command is not in PATH
//! 141   our stdout was closed (`| head`)
//!   n   propagated from the configured command
//! ```

use std::process::ExitCode;

use crate::command::CommandError;

/// Exit code for a runtime failure with no more specific code.
pub(crate) const EXIT_FAILURE: u8 = 1;
/// Exit code for a usage error, matching what clap emits on its own.
pub(crate) const EXIT_USAGE: u8 = 2;
/// Exit code shells use for "command not found".
pub(crate) const EXIT_NOT_FOUND: u8 = 127;
/// Exit code shells use for "killed by SIGPIPE" (128 + 13).
pub(crate) const EXIT_BROKEN_PIPE: u8 = 141;
/// What a shell adds to a signal number to report a death by that signal.
const SIGNAL_BASE: i32 = 128;

/// Failures that carry their own exit code.
///
/// HLD §2 also lists `UnknownHost`, but §3 removed the host registry — there is
/// no longer anything that could be an unknown host, so that variant is gone.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The config file is not valid TOML, or sets a value hog cannot honour.
    ///
    /// Rendered as `path:line: message`, the shape every editor already knows
    /// how to jump to. The line number is the entire reason `config::load`
    /// parses with `toml_edit` and its spans rather than with a plain
    /// deserializer, so it is part of this variant rather than folded into the
    /// message by the caller.
    ///
    /// `line` is 1-based, and is 1 for the rare failure that carries no
    /// position at all — line 1 is where an editor lands anyway, and inventing
    /// a `0` would name a line no file has.
    #[error("{path}:{line}: {message}")]
    ConfigParse {
        /// The config file, spelled the way it was reached.
        path: String,
        /// 1-based line the failure sits on.
        line: usize,
        /// What is wrong, in one lowercase line.
        message: String,
    },

    /// stdin is a terminal and there is nothing else to read. Without this the
    /// process would sit and wait for the user to type JSON at it (HLD §6).
    #[error("no input: stdin is a terminal")]
    NoInput,

    /// A positional argument contains something the whitelist of HLD §5
    /// refuses. Exit code 1.
    ///
    /// The long-form guidance ("hog only accepts letters, digits and …") is
    /// already the `Display` of the inner [`CommandError`], so this variant
    /// only adds the exit code. Wrapping rather than restating is deliberate:
    /// two copies of a security message drift apart, and the copy in the
    /// exit-code table is the one nobody would think to update.
    #[error(transparent)]
    BadArgument(CommandError),

    /// The `command` template cannot be split into words, has a hole in its
    /// `{N}` indices, or does not match the number of arguments given
    /// ("template needs 2 arguments, got 0"). Exit code 1.
    ///
    /// One variant for all three because they share an exit code *and* a
    /// remedy: the template in the config file is wrong for this invocation.
    #[error(transparent)]
    TemplateArity(CommandError),

    /// The first word of the template is not an executable in `PATH`.
    ///
    /// Exit code 127, the code every shell uses for this, so a script wrapping
    /// hog can tell "the tool is missing" from "the tool ran and failed".
    #[error("command not found: {program}")]
    CommandNotFound {
        /// The first word of the assembled argv, as `PATH` did not find it.
        program: String,
        /// The whole argv on one line, for the hint.
        command: String,
    },

    /// The command ran and exited non-zero. Its code is propagated as ours.
    ///
    /// The line count is part of the message, not a separate diagnostic: HLD §5
    /// wants one loud line, and the whole point of it is that a dropped VPN
    /// (`ssh` exits 255 after 1423 lines) must not read like "the logs ended".
    #[error("command exited with status {status} after {lines} {}", lines_word(*lines))]
    CommandExit {
        /// The child's exit status, 0..=255 on unix.
        status: i32,
        /// How many lines hog had already rendered.
        lines: u64,
    },

    /// The command was killed by a signal. Reported as 128 + the signal, which
    /// is what a shell would have reported for the same death.
    #[error("command was killed by signal {signal} after {lines} {}", lines_word(*lines))]
    CommandSignal {
        /// The signal number.
        signal: i32,
        /// How many lines hog had already rendered.
        lines: u64,
    },

    /// `--ts-format` was neither a strftime pattern (containing `%`) nor one of
    /// the two reserved words.
    #[error("invalid time format {0:?}: expected a strftime pattern, or `raw`, or `none`")]
    BadTimeFormat(String),

    /// `output.color` in the config named something that is not a colour
    /// choice. Unreachable from the CLI, where clap checks the value enum.
    #[error("invalid colour choice {0:?}: expected `auto`, `always` or `never`")]
    BadColorChoice(String),

    /// `--timezone` named a zone the tzdb does not know.
    #[error(
        "unknown time zone {0:?}: expected `local`, `utc`, or an IANA name like `Europe/Moscow`"
    )]
    UnknownTimeZone(String),

    /// Our stdout went away (`hog | head -5`). Mapped to 141 so the shell sees
    /// what it would have seen from a SIGPIPE death.
    ///
    /// Note this is an *error value*, not a `process::exit`: returning lets the
    /// command session's `Drop`-guard run and reap the child before we leave.
    #[error("stdout closed")]
    BrokenPipe,

    #[error("shell completions are not implemented yet (v1.0)")]
    CompletionsNotImplemented,
}

/// `"line"` or `"lines"`, so the loud line of HLD §5 does not say "1 lines".
fn lines_word(lines: u64) -> &'static str {
    if lines == 1 { "line" } else { "lines" }
}

/// Routes a command-mode refusal onto the exit-code table.
///
/// Both arms land on exit code 1; the two variants exist because the *hints*
/// differ, not the code. There is deliberately no third arm for "nothing is
/// configured": HLD §5 replaced that refusal with the built-in `echo {@}`, so
/// [`crate::command::plan`] always has a template to work from.
impl From<CommandError> for Error {
    fn from(err: CommandError) -> Self {
        match err {
            err @ CommandError::Argument { .. } => Self::BadArgument(err),
            err @ CommandError::Template { .. } => Self::TemplateArity(err),
        }
    }
}

impl Error {
    /// The process exit code for this failure.
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            Self::NoInput => EXIT_USAGE,
            Self::BrokenPipe => EXIT_BROKEN_PIPE,
            Self::CommandNotFound { .. } => EXIT_NOT_FOUND,
            Self::CommandExit { status, .. } => propagated(*status),
            Self::CommandSignal { signal, .. } => propagated(SIGNAL_BASE.saturating_add(*signal)),
            Self::ConfigParse { .. }
            | Self::BadArgument(_)
            | Self::TemplateArity(_)
            | Self::BadTimeFormat(_)
            | Self::BadColorChoice(_)
            | Self::UnknownTimeZone(_)
            | Self::CompletionsNotImplemented => EXIT_FAILURE,
        }
    }

    /// Extra guidance printed under the one-line error message.
    ///
    /// Kept next to the variant so the long-form help of HLD §5 lives beside
    /// the error it explains.
    fn hint(&self) -> Hint<'_> {
        match self {
            Self::NoInput => Hint::Usage,
            Self::CommandNotFound { program, command } => Hint::NotFound(program, command),
            _ => Hint::None,
        }
    }
}

/// Turns a foreign exit code into one this process can actually return.
///
/// `ExitCode` is a byte, so 128 + 9 fits but nothing above 255 does. A value
/// that would land on 0 after the truncation is reported as a plain failure
/// instead: exiting 0 for a command that did not succeed is the one answer
/// that would be read as "everything is fine".
fn propagated(code: i32) -> u8 {
    let byte = u8::try_from(code.rem_euclid(256)).unwrap_or(EXIT_FAILURE);
    if byte == 0 { EXIT_FAILURE } else { byte }
}

enum Hint<'a> {
    None,
    /// Print the `--help` text after the error.
    ///
    /// That text is not static: `--help` carries the "Configured command" block
    /// of HLD §6, so a bare `hog` at a prompt with no config gets the built-in
    /// `echo {@}` and the `hog config init` hint printed under its usage — which
    /// is the entire guidance the deleted `NoCommand` hint used to carry, now
    /// living in one place instead of two.
    Usage,
    /// Say what hog tried to execute, and why `PATH` is the thing to look at.
    NotFound(&'a str, &'a str),
}

/// Writes the guidance for [`Error::CommandNotFound`].
///
/// The argv is shown because the usual cause is a template whose first word is
/// not what its author thought — a shell alias, a function, or a builtin. There
/// is no local shell in command mode (HLD §5), so none of those exist here.
fn write_not_found_hint<W: std::io::Write>(stderr: &mut W, program: &str, command: &str) {
    let _ = writeln!(stderr);
    let _ = writeln!(
        stderr,
        "  hog runs the `command` template without a local shell, so {program:?} has to be\n  \
         an executable in PATH — a shell alias, function or builtin will not do."
    );
    let _ = writeln!(stderr);
    let _ = writeln!(stderr, "  hog tried to run:\n\n      {command}");
    let _ = writeln!(stderr);
    let _ = writeln!(
        stderr,
        "  Check what would run, without running it:  hog --dry-run ARG..."
    );
}

/// Prints `err` to stderr and returns the process exit code.
///
/// A broken pipe exits quietly: the downstream process is gone, complaining
/// about it is noise, and `$? == 141` already tells the shell what happened.
pub fn report(err: &anyhow::Error) -> ExitCode {
    use std::io::Write as _;

    let domain = err.downcast_ref::<Error>();
    let code = domain.map_or(EXIT_FAILURE, Error::exit_code);

    if matches!(domain, Some(Error::BrokenPipe)) {
        return ExitCode::from(code);
    }

    // Not `eprintln!`: the `println!` family panics on a broken pipe.
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "error: {err}");
    for cause in err.chain().skip(1) {
        let _ = writeln!(stderr, "  caused by: {cause}");
    }
    let _ = stderr.flush();

    match domain.map(Error::hint) {
        Some(Hint::Usage) => {
            let _ = writeln!(stderr);
            crate::help::print_usage_hint(&mut stderr);
        }
        Some(Hint::NotFound(program, command)) => {
            write_not_found_hint(&mut stderr, program, command);
        }
        _ => {}
    }

    ExitCode::from(code)
}
