//! Where the bytes come from, and how they are cut into lines.
//!
//! `BufRead::lines()` is **banned** in this crate. It yields
//! `Result<String, io::Error>`, so the first byte that is not valid UTF-8 turns
//! into an error and a naive caller stops the stream — exactly the silent
//! truncation this rewrite exists to fix. Lines are `Vec<u8>` here and stay
//! bytes until the renderer has decided what it can do with them.

use std::io::{self, BufRead, BufReader, IsTerminal, Read, StdinLock};
use std::process::ChildStdout;

use crate::cli::RunArgs;
use crate::command::{self, CommandPlan};
use crate::error::Error;
use crate::settings::Settings;

/// Longest line we will hold in memory, matching hulog's `bufio.Scanner` limit.
///
/// Unlike hulog, hitting it is not fatal: see [`Line::Truncated`].
pub(crate) const MAX_LINE: usize = 1024 * 1024;

/// [`MAX_LINE`] in the unit `Read::take` wants. Lossless: `usize` is never
/// wider than `u64` on any target hog builds for, and the value is a constant.
const MAX_LINE_LIMIT: u64 = MAX_LINE as u64;

/// Initial capacity of the read buffer. Large enough that a typical log line
/// costs no syscall of its own.
const READ_BUFFER: usize = 64 * 1024;

/// Where a stream of log lines comes from.
///
/// The pipeline is written against this and never learns which arm it got:
/// following `ssh` and reading a redirected file are the same loop.
#[derive(Debug)]
pub(crate) enum Source {
    Stdin(StdinLock<'static>),
    /// The read end of a spawned command's stdout pipe. The process itself is
    /// owned by [`crate::command::session::Session`], which also owns this
    /// `Input` — so a stream can never outlive the guard that kills its child.
    Command(ChildStdout),
    /// An in-memory stream. Test-only, and shaped exactly like `Command`:
    /// another owned `Read` the pipeline cannot tell apart from stdin.
    #[cfg(test)]
    Bytes(io::Cursor<Vec<u8>>),
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Stdin(stdin) => stdin.read(buf),
            Self::Command(stdout) => stdout.read(buf),
            #[cfg(test)]
            Self::Bytes(cursor) => cursor.read(buf),
        }
    }
}

/// Which half of the mode table of HLD §6 this invocation landed in.
///
/// Deliberately *not* an already-opened stream: [`select`] decides the mode and
/// nothing else, so it neither locks stdin nor spawns a process, and the whole
/// table can be unit-tested with the terminal answer passed in as a `bool`.
#[derive(Debug)]
pub(crate) enum Mode {
    /// Read stdin. Reached only when stdin is a pipe or a file.
    Stdin,
    /// Run this argv and read its stdout. The plan is already assembled, so
    /// every refusal in HLD §5 — the whitelist, the arity check, a template
    /// that will not split — has already happened.
    Command(CommandPlan),
}

/// Picks the mode for this run: the whole of HLD §6, "Режимы ввода".
///
/// | arguments | stdin        | `command` template | behaviour                 |
/// |-----------|--------------|--------------------|---------------------------|
/// | yes       | any          | yes                | run it, stdin unread      |
/// | yes       | any          | no                 | run `echo {@}`            |
/// | no        | pipe or file | any                | read stdin                |
/// | no        | terminal     | yes, no `{N}`      | run it                    |
/// | no        | terminal     | yes, with `{N}`    | arity error               |
/// | no        | terminal     | no                 | [`Error::NoInput`] + help |
///
/// Row two is the one that changed in v0.4, and it changed twice over: HLD §5
/// cancelled the old "arguments without a template is an error" rule in favour
/// of the built-in [`command::DEFAULT_COMMAND`], so `hog prod api` on a
/// brand-new install prints `prod api` rather than a refusal. Nothing here
/// answers it any more — the row falls straight through to [`command::plan`],
/// which substitutes into `echo {@}` exactly as it would into a configured
/// template.
///
/// That leaves **one** place where a missing `command` is still visible, and it
/// is the last row: a bare `hog` at a prompt. Running `echo` with nothing to
/// echo would be a silent no-op, and the person who typed it wants the usage
/// text, so that stays [`Error::NoInput`] with exit code 2 — the reason
/// [`Settings::command`] is still an `Option` rather than a `String` filled in
/// with the default at load time.
///
/// `--dry-run` is the one thing that reads the table differently, and it is not
/// a hole in it: the table describes **runs**, and `--dry-run` is a question
/// about the command rather than a run. It therefore always takes the command
/// branch, including on rows three and six. Answering "what would you run?" by
/// reading stdin would make hog sit and block under a flag whose own `--help`
/// promises it prints and exits — the same "waits forever" failure the last row
/// exists to prevent, reached from the other side.
///
/// Rows four and five are not distinguished here, and that is the point:
/// [`command::plan`] checks the arity against the arguments it was given, so a
/// template with no placeholders plans successfully from zero arguments and one
/// with `{0}`/`{1}` produces exactly `template needs 2 arguments, got 0`.
///
/// [`Error::NoInput`] is what keeps `hog` at a bare prompt from waiting forever
/// for the user to type JSON at it — the classic bug of the naive version.
pub(crate) fn select(args: &RunArgs, settings: &Settings) -> Result<Mode, Error> {
    select_for(args, settings, io::stdin().is_terminal())
}

/// [`select`] with the terminal question already answered, so the six rows can
/// be tested without one.
fn select_for(args: &RunArgs, settings: &Settings, stdin_is_terminal: bool) -> Result<Mode, Error> {
    // Row three, and the `--dry-run` exception to it (see the doc comment).
    // Arguments choose the mode; the terminal test only breaks the tie when
    // there are none. `hog < file.log` lands here, as it should.
    if args.args.is_empty() && !stdin_is_terminal && !args.dry_run {
        return Ok(Mode::Stdin);
    }

    // Row six, the only row that can still tell "configured" from "not": a bare
    // `hog` at a prompt with nothing in the config. The built-in `echo {@}`
    // would run here and print an empty line, which is not what the person who
    // typed `hog` wanted — they want the usage text, so they get it.
    if args.args.is_empty() && stdin_is_terminal && !args.dry_run && settings.command.is_none() {
        return Err(Error::NoInput);
    }

    // Every other row is command mode, and there is always a template to plan:
    // `plan` falls back to `echo {@}` when the config sets none (HLD §5).
    Ok(Mode::Command(command::plan(settings, &args.args)?))
}

/// What [`Input::read_line`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Line {
    /// A whole line. The trailing `\n` and at most one `\r` before it are gone.
    Full,
    /// The line was longer than [`MAX_LINE`]. The first `MAX_LINE` bytes are in
    /// the buffer and **are rendered**; the rest was drained up to the next
    /// `\n` and dropped, and a warning went to stderr.
    ///
    /// This is the whole point of the rewrite: hulog's `Scanner` returned
    /// `ErrTooLong`, the loop ended, nobody checked `Err()`, and the entire
    /// remainder of the log vanished with exit code 0.
    Truncated,
    /// End of stream. A final line without a trailing `\n` is reported as
    /// [`Line::Full`] first; `Eof` only ever comes with an empty buffer.
    Eof,
}

/// A buffered byte source that yields log lines.
#[derive(Debug)]
pub(crate) struct Input {
    reader: BufReader<Source>,
}

impl Input {
    /// Locks stdin and reads from it.
    ///
    /// Only correct after [`select`] has returned [`Mode::Stdin`], which is what
    /// proves stdin is not a terminal; the terminal gate lives there, not here,
    /// so that nothing locks a stream it has not been told to read.
    pub(crate) fn stdin() -> Self {
        Self::new(Source::Stdin(io::stdin().lock()))
    }

    /// Wraps an arbitrary source. Used by tests and by the command session.
    pub(crate) fn new(source: Source) -> Self {
        Self {
            reader: BufReader::with_capacity(READ_BUFFER, source),
        }
    }

    /// Wraps a slice of bytes. Test-only.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(Source::Bytes(io::Cursor::new(bytes.into())))
    }

    /// Reads one line into `buf`, replacing its previous contents.
    ///
    /// Implemented as `(&mut reader).take(MAX_LINE).read_until(b'\n', buf)` so
    /// that memory stays bounded no matter what the producer does. Contract:
    ///
    /// * invalid UTF-8 is passed through untouched — no lossy conversion here;
    /// * the trailing `\n` is stripped, and so is exactly **one** `\r` before
    ///   it (the ONLCR that `ssh -tt` adds). A second `\r` is data;
    /// * a final line with no trailing `\n` is still returned as [`Line::Full`];
    /// * `buf` is cleared on entry, and its capacity is reused across calls.
    pub(crate) fn read_line(&mut self, buf: &mut Vec<u8>) -> io::Result<Line> {
        buf.clear();

        let read = (&mut self.reader)
            .take(MAX_LINE_LIMIT)
            .read_until(b'\n', buf)?;

        if read == 0 {
            return Ok(Line::Eof);
        }

        if buf.last() == Some(&b'\n') {
            buf.pop();
            strip_one_carriage_return(buf);
            return Ok(Line::Full);
        }

        // No newline in what we read. Either the stream ended (a final line
        // without a trailing newline — still a line), or we hit the cap.
        if read < MAX_LINE {
            return Ok(Line::Full);
        }

        // The cap. Everything up to the next newline is read and dropped so
        // that the *following* lines survive; hulog stopped the loop here and
        // lost the rest of the log.
        let (dropped, newline) = self.drain_line()?;
        if dropped == 0 {
            // The line was exactly MAX_LINE bytes long: nothing was lost, and
            // the newline (if any) has now been consumed.
            if newline {
                strip_one_carriage_return(buf);
            }
            return Ok(Line::Full);
        }

        Ok(Line::Truncated)
    }

    /// `true` when nothing is buffered, i.e. the next read would block.
    ///
    /// This is the entire flush policy (HLD §8): flush when the reader has run
    /// dry, and let `BufWriter` batch otherwise. No `--follow` flag, and no
    /// tty heuristic — `tail -f x | hog` is a pipe *and* live at the same time.
    pub(crate) fn is_drained(&self) -> bool {
        self.reader.buffer().is_empty()
    }

    /// Reads and discards bytes up to and including the next `\n`.
    ///
    /// Returns how many bytes were discarded *before* the newline, and whether
    /// a newline was found at all (`false` means the stream ended first).
    /// Nothing is buffered: the loop works straight out of the `BufReader`'s
    /// own buffer, so an adversarial 4 GiB line costs constant memory.
    fn drain_line(&mut self) -> io::Result<(u64, bool)> {
        let mut dropped: u64 = 0;

        loop {
            let (consumed, found) = match self.reader.fill_buf() {
                Ok([]) => return Ok((dropped, false)),
                Ok(available) => match available.iter().position(|&byte| byte == b'\n') {
                    Some(index) => (index.saturating_add(1), true),
                    None => (available.len(), false),
                },
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };

            self.reader.consume(consumed);
            // The newline itself is a delimiter, not lost data.
            let data = if found {
                consumed.saturating_sub(1)
            } else {
                consumed
            };
            dropped = dropped.saturating_add(data as u64);

            if found {
                return Ok((dropped, true));
            }
        }
    }
}

/// Removes one trailing `\r`, the ONLCR a pty adds. A second one is data.
fn strip_one_carriage_return(buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
}

/// The six rows of HLD §6, exercised without a terminal, a pipe or a process.
///
/// They live here rather than in `tests/` because the tie-breaker is a `bool`
/// in [`select_for`]: an integration test can only reach rows four to six
/// through `script(1)`, and would be skipped on a machine without it.
#[cfg(test)]
mod mode_table {
    use super::*;

    use clap::Parser as _;

    use crate::cli::Cli;

    fn args(argv: &[&str]) -> RunArgs {
        Cli::try_parse_from(argv)
            .unwrap_or_else(|err| panic!("`{}` must parse:\n{err}", argv.join(" ")))
            .run
    }

    fn settings(command: Option<&str>) -> Settings {
        Settings {
            command: command.map(str::to_owned),
            ..Settings::default()
        }
    }

    /// One row of the table: argv, template, and whether stdin is a terminal.
    fn row(argv: &[&str], command: Option<&str>, terminal: bool) -> Result<Mode, Error> {
        select_for(&args(argv), &settings(command), terminal)
    }

    fn argv_of(mode: &Mode) -> Vec<&str> {
        match mode {
            Mode::Command(plan) => plan.argv().collect(),
            Mode::Stdin => panic!("expected command mode, got stdin"),
        }
    }

    /// Row 1 — arguments and a template: run it, whatever stdin is. Both halves
    /// of "whatever stdin is" are checked: the mode must not flip when the same
    /// invocation moves from an interactive shell into a pipeline.
    #[test]
    fn arguments_and_a_template_run_the_command_on_any_stdin() {
        for terminal in [true, false] {
            let mode = row(
                &["hog", "prod", "api"],
                Some("ssh -tt {0} 'docker logs -f myapp-{1}-1'"),
                terminal,
            )
            .expect("must plan");
            assert_eq!(
                argv_of(&mode),
                ["ssh", "-tt", "prod", "docker logs -f myapp-api-1"],
                "terminal: {terminal}"
            );
        }
    }

    /// Row 2 — arguments, no template: the built-in `echo {@}` runs, whatever
    /// stdin is. HLD §5 cancelled the refusal that used to live here, and this
    /// is the row that proves the cancellation reaches the mode table and not
    /// just `command::plan`: the guard that shorted it out never let `plan` see
    /// this case at all.
    #[test]
    fn arguments_without_a_template_run_the_built_in_echo() {
        for terminal in [true, false] {
            let mode = row(&["hog", "prod", "api"], None, terminal).expect("must plan");
            assert_eq!(
                argv_of(&mode),
                ["echo", "prod", "api"],
                "terminal: {terminal}"
            );
        }
    }

    /// The same row with a hostile argument: the built-in is a real template,
    /// so the whitelist of HLD §5 runs over its arguments too. A default that
    /// skipped the check would be a hole opened by *not* configuring anything.
    #[test]
    fn the_built_in_echo_still_whitelists_its_arguments() {
        let err = row(&["hog", "--", "api; rm -rf /"], None, true).expect_err("must refuse");
        assert!(matches!(err, Error::BadArgument(_)), "{err:?}");
        assert_eq!(err.exit_code(), 1);
    }

    /// Row 3 — no arguments and a pipe or a file on stdin: read it. A template
    /// in the config must **not** hijack `cat app.log | hog`.
    #[test]
    fn a_pipe_on_stdin_is_read_even_when_a_template_exists() {
        for command in [None, Some("kubectl logs -f -l app=api")] {
            let mode = row(&["hog"], command, false).expect("must read stdin");
            assert!(matches!(mode, Mode::Stdin), "command: {command:?}");
        }
    }

    /// Row 4 — a bare `hog` at a prompt with a template that needs no
    /// arguments. HLD §6 calls this out by name: it is a legitimate setup, not
    /// a mistake.
    #[test]
    fn a_terminal_and_a_template_without_placeholders_runs_it() {
        let mode = row(&["hog"], Some("kubectl logs -f -l app=api"), true).expect("must plan");
        assert_eq!(argv_of(&mode), ["kubectl", "logs", "-f", "-l", "app=api"]);
    }

    /// Row 5 — the same, but the template needs arguments nobody gave. The
    /// message is the one HLD §6 spells out, word for word.
    #[test]
    fn a_terminal_and_a_template_with_placeholders_is_an_arity_error() {
        let err = row(&["hog"], Some("ssh {0} 'logs {1}'"), true).expect_err("must refuse");
        assert!(matches!(err, Error::TemplateArity(_)), "{err:?}");
        assert!(
            err.to_string()
                .starts_with("template needs 2 arguments, got 0"),
            "{err}"
        );
        assert_eq!(err.exit_code(), 1);
    }

    /// Row 6 — nothing to read and nothing to run. The row that stops a bare
    /// `hog` from waiting forever for the user to type JSON at it.
    #[test]
    fn a_bare_terminal_with_no_template_is_a_usage_error() {
        let err = row(&["hog"], None, true).expect_err("must refuse");
        assert!(matches!(err, Error::NoInput), "{err:?}");
        assert_eq!(err.exit_code(), 2);
    }

    /// Not a row of its own, but the boundary the rows exist to protect: a
    /// hostile argument is refused before anything is assembled, and the
    /// refusal is exit 1 rather than a plan.
    #[test]
    fn a_hostile_argument_never_becomes_a_plan() {
        let err =
            row(&["hog", "--", "api; rm -rf /"], Some("ssh {0}"), false).expect_err("must refuse");
        assert!(matches!(err, Error::BadArgument(_)), "{err:?}");
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.to_string()
                .contains("contains characters that are not allowed"),
            "{err}"
        );
    }

    /// The exclude flags must not disturb the mode: `-e` takes one value and no
    /// more, so the positionals after it are still positionals (the `num_args`
    /// trap named in `cli.rs`).
    #[test]
    fn the_exclude_flags_do_not_eat_the_positional_arguments() {
        let mode = row(
            &["hog", "-e", "a,b", "-E", "prod", "api"],
            Some("ssh {0} {1}"),
            true,
        )
        .expect("must plan");
        assert_eq!(argv_of(&mode), ["ssh", "prod", "api"]);
    }

    /// `--dry-run` is the one flag that reads the table differently: it asks a
    /// question about the command, so it must never be answered by reading
    /// stdin — the flag promises to print and exit, and a pipe on stdin would
    /// make it block instead.
    #[test]
    fn dry_run_plans_the_command_even_with_a_pipe_on_stdin() {
        let mode = select_for(
            &args(&["hog", "--dry-run"]),
            &settings(Some("kubectl logs -f -l app=api")),
            false,
        )
        .expect("must plan");
        assert_eq!(argv_of(&mode), ["kubectl", "logs", "-f", "-l", "app=api"]);
    }

    /// The same, with nothing configured: `--dry-run` asks what would run, and
    /// the honest answer is the built-in — not the usage text of row six, which
    /// would leave the question unanswered, and not the terminal gate, which
    /// would be a plain falsehood with a pipe on stdin.
    #[test]
    fn dry_run_without_a_template_shows_the_built_in() {
        let mode =
            select_for(&args(&["hog", "--dry-run"]), &settings(None), false).expect("must plan");
        assert_eq!(argv_of(&mode), ["echo"]);
    }

    /// Without `--dry-run`, the same invocation is row three and reads stdin.
    /// This is the pair that pins the exception as an exception.
    #[test]
    fn without_dry_run_the_same_invocation_reads_stdin() {
        let mode = select_for(
            &args(&["hog"]),
            &settings(Some("kubectl logs -f -l app=api")),
            false,
        )
        .expect("must read stdin");
        assert!(matches!(mode, Mode::Stdin));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the whole source, returning each line's bytes and its kind.
    fn read_all(bytes: impl Into<Vec<u8>>) -> Vec<(Line, Vec<u8>)> {
        let mut input = Input::from_bytes(bytes);
        let mut buf = Vec::new();
        let mut lines = Vec::new();
        loop {
            let kind = input
                .read_line(&mut buf)
                .expect("in-memory read cannot fail");
            if kind == Line::Eof {
                assert!(buf.is_empty(), "Eof must come with an empty buffer");
                return lines;
            }
            lines.push((kind, buf.clone()));
        }
    }

    #[test]
    fn empty_input_is_eof_immediately() {
        assert!(read_all(&b""[..]).is_empty());
    }

    #[test]
    fn splits_on_newlines_and_keeps_blank_lines() {
        let lines = read_all(&b"a\n\nb\n"[..]);
        assert_eq!(
            lines,
            [
                (Line::Full, b"a".to_vec()),
                (Line::Full, Vec::new()),
                (Line::Full, b"b".to_vec()),
            ]
        );
    }

    #[test]
    fn final_line_without_a_newline_is_still_a_line() {
        let lines = read_all(&b"a\nb"[..]);
        assert_eq!(
            lines,
            [(Line::Full, b"a".to_vec()), (Line::Full, b"b".to_vec())]
        );
    }

    #[test]
    fn strips_exactly_one_carriage_return() {
        let lines = read_all(&b"a\r\nb\r\r\n\r\n"[..]);
        assert_eq!(
            lines,
            [
                (Line::Full, b"a".to_vec()),
                // The second \r is data, not ONLCR.
                (Line::Full, b"b\r".to_vec()),
                (Line::Full, Vec::new()),
            ]
        );
    }

    #[test]
    fn a_lone_carriage_return_at_eof_is_data() {
        let lines = read_all(&b"a\r"[..]);
        assert_eq!(lines, [(Line::Full, b"a\r".to_vec())]);
    }

    #[test]
    fn invalid_utf8_passes_through_byte_for_byte() {
        let lines = read_all(&b"\xff\xfe ok\nnext\n"[..]);
        assert_eq!(
            lines,
            [
                (Line::Full, b"\xff\xfe ok".to_vec()),
                (Line::Full, b"next".to_vec()),
            ]
        );
    }

    // The hulog bug: one over-long line killed the rest of the stream.
    #[test]
    fn over_long_line_is_truncated_and_the_stream_survives() {
        let mut bytes = vec![b'x'; MAX_LINE + 4096];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"survivor\n");

        let lines = read_all(bytes);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, Line::Truncated);
        assert_eq!(lines[0].1.len(), MAX_LINE);
        assert!(lines[0].1.iter().all(|&byte| byte == b'x'));
        assert_eq!(lines[1], (Line::Full, b"survivor".to_vec()));
    }

    #[test]
    fn over_long_final_line_without_a_newline_is_truncated() {
        let bytes = vec![b'x'; MAX_LINE + 1];
        let lines = read_all(bytes);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, Line::Truncated);
        assert_eq!(lines[0].1.len(), MAX_LINE);
    }

    // Exactly at the cap nothing is lost, so it is a whole line, not a
    // truncated one — with or without the trailing newline.
    #[test]
    fn line_of_exactly_max_len_is_not_truncated() {
        let mut bytes = vec![b'x'; MAX_LINE];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"next\n");

        let lines = read_all(bytes);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, Line::Full);
        assert_eq!(lines[0].1.len(), MAX_LINE);
        assert_eq!(lines[1], (Line::Full, b"next".to_vec()));
    }

    #[test]
    fn line_of_exactly_max_len_at_eof_is_not_truncated() {
        let lines = read_all(vec![b'x'; MAX_LINE]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, Line::Full);
        assert_eq!(lines[0].1.len(), MAX_LINE);
    }

    // CRLF exactly at the cap: the \r is the last byte we read, the \n is the
    // first byte drained. Nothing was lost, so the \r is still ONLCR.
    #[test]
    fn crlf_across_the_cap_boundary_still_strips() {
        let mut bytes = vec![b'x'; MAX_LINE - 1];
        bytes.extend_from_slice(b"\r\nnext\n");

        let lines = read_all(bytes);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, Line::Full);
        assert_eq!(lines[0].1.len(), MAX_LINE - 1);
        assert_eq!(lines[1], (Line::Full, b"next".to_vec()));
    }

    #[test]
    fn the_buffer_is_cleared_between_lines() {
        let mut input = Input::from_bytes(&b"first line\nb\n"[..]);
        let mut buf = Vec::new();
        assert_eq!(input.read_line(&mut buf).expect("read"), Line::Full);
        assert_eq!(input.read_line(&mut buf).expect("read"), Line::Full);
        assert_eq!(buf, b"b");
    }

    #[test]
    fn drained_only_once_the_buffer_runs_out() {
        let mut input = Input::from_bytes(&b"a\nb\n"[..]);
        let mut buf = Vec::new();

        input.read_line(&mut buf).expect("read");
        assert!(!input.is_drained(), "`b\\n` is still buffered");
        input.read_line(&mut buf).expect("read");
        assert!(input.is_drained(), "caught up with the producer");
    }
}
