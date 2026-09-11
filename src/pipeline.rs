//! The streaming loop: [`Input`] -> [`Renderer`] -> [`Output`].
//!
//! Shape of the loop, which the implementation must preserve:
//!
//! ```text
//! loop {
//!     match input.read_line(&mut line)? {
//!         Line::Eof => break,
//!         Line::Truncated => { warn once on stderr; render anyway }
//!         Line::Full => {}
//!     }
//!     renderer.render_bytes(&line, output.stream())?;   // <- only write site
//!     if input.is_drained() { output.flush()?; }
//! }
//! output.flush()?;
//! ```
//!
//! Two rules live here and nowhere else:
//!
//! 1. **Broken pipe.** Any `io::Error` from a write whose kind is
//!    [`std::io::ErrorKind::BrokenPipe`] becomes [`crate::error::Error::BrokenPipe`], which maps
//!    to exit 141. It is returned, not `process::exit`ed, so the command
//!    session's `Drop`-guard still gets to kill and reap the child.
//! 2. **Flush policy.** Flush exactly when [`Input::is_drained`] says the
//!    reader has nothing buffered. Zero added latency while following a live
//!    log, full batching while redirecting to a file, no flag to get wrong.

use std::io::{self, Write};

use anyhow::Context as _;

use crate::error::Error;
use crate::input::{Input, Line, MAX_LINE};
use crate::output::Output;
use crate::render::Renderer;

/// Starting capacity of the line buffer. It grows to fit the longest line seen
/// so far and is then reused, so the steady state is zero allocations per line
/// (`mem-reuse-collections`). [`MAX_LINE`] bounds it from above.
const LINE_BUFFER: usize = 8 * 1024;

/// Streams everything from `input` to `output`, returning the number of lines
/// written.
///
/// The count is not decoration: `command::session::Session::finish` reports
/// `command exited with status 255 after 1423 lines` so that a dropped VPN
/// cannot masquerade as "the logs ended" (HLD §5).
pub(crate) fn run(
    input: &mut Input,
    renderer: &mut Renderer,
    output: &mut Output,
) -> anyhow::Result<u64> {
    // `Output::flush` is `stream().flush()`, so handing the loop the stream
    // alone keeps the single-write-site rule and still flushes the same buffer.
    let mut diagnostics = io::stderr();
    pump(input, output.stream(), &mut diagnostics, |line, out| {
        renderer.render_bytes(line, out)
    })
}

/// The loop itself, with the sink and the renderer left open.
///
/// Generic rather than `dyn` so the real run stays monomorphised, and so the
/// tests can drive the exact same code with a `Vec<u8>` for stdout, a
/// `Vec<u8>` for stderr and a pass-through "renderer".
fn pump<W, D, R>(
    input: &mut Input,
    out: &mut W,
    diagnostics: &mut D,
    mut render: R,
) -> anyhow::Result<u64>
where
    W: Write,
    D: Write,
    R: FnMut(&[u8], &mut W) -> io::Result<()>,
{
    let mut line = Vec::with_capacity(LINE_BUFFER);
    let mut written: u64 = 0;
    let mut truncated: u64 = 0;

    loop {
        let kind = input
            .read_line(&mut line)
            .context("reading the input stream")?;

        match kind {
            Line::Eof => break,
            Line::Truncated => {
                truncated = truncated.saturating_add(1);
                if truncated == 1 {
                    // Once: a stream of over-long lines must not turn stderr
                    // into a second log. The tally goes out at the end.
                    warn_truncated(diagnostics, written.saturating_add(1));
                }
            }
            Line::Full => {}
        }

        render(&line, out).map_err(write_failed)?;
        written = written.saturating_add(1);

        // Caught up with the producer: show what we have.
        if input.is_drained() {
            out.flush().map_err(write_failed)?;
        }
    }

    out.flush().map_err(write_failed)?;

    if truncated > 1 {
        let _ = writeln!(
            diagnostics,
            "hog: warning: {truncated} lines were longer than {} MiB in total",
            MAX_LINE / (1024 * 1024)
        );
    }

    Ok(written)
}

/// Classifies a failed write.
///
/// A broken pipe is the documented, quiet way for `hog | head -5` to end, so it
/// becomes the typed [`Error::BrokenPipe`] (exit 141) rather than an `io`
/// error with a stack of context nobody wants to read.
fn write_failed(err: io::Error) -> anyhow::Error {
    if err.kind() == io::ErrorKind::BrokenPipe {
        return Error::BrokenPipe.into();
    }
    anyhow::Error::new(err).context("writing to stdout")
}

/// Says what happened to an over-long line, in one line, on stderr.
///
/// Diagnostics are best-effort: a closed stderr must not end the run, and it is
/// never `eprintln!`, which panics on exactly that.
fn warn_truncated<D: Write>(diagnostics: &mut D, line_number: u64) {
    let _ = writeln!(
        diagnostics,
        "hog: warning: line {line_number} is longer than {} MiB; printed the first {} MiB and \
         dropped the rest of that line (the rest of the stream is unaffected)",
        MAX_LINE / (1024 * 1024),
        MAX_LINE / (1024 * 1024),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the renderer: the pass-through path, which is what the
    /// real renderer does for a line it cannot parse. One input line in, one
    /// output line out — the invariant the loop must not break.
    fn echo(line: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        out.write_all(line)?;
        out.write_all(b"\n")
    }

    /// Runs the loop over `bytes`, returning (stdout, stderr, line count).
    fn run_over(bytes: impl Into<Vec<u8>>) -> (Vec<u8>, String, u64) {
        let mut input = Input::from_bytes(bytes);
        let mut out = Vec::new();
        let mut diagnostics = Vec::new();
        let written = pump(&mut input, &mut out, &mut diagnostics, echo).expect("must not fail");
        let diagnostics = String::from_utf8(diagnostics).expect("diagnostics are ASCII");
        (out, diagnostics, written)
    }

    /// A sink that counts flushes, to pin the flush policy.
    struct CountingSink {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for CountingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    /// A sink whose every write fails with `kind`.
    struct FailingSink(io::ErrorKind);

    impl Write for FailingSink {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "sink is gone"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(self.0, "sink is gone"))
        }
    }

    #[test]
    fn empty_input_writes_nothing() {
        let (out, diagnostics, written) = run_over(&b""[..]);
        assert!(out.is_empty());
        assert!(diagnostics.is_empty());
        assert_eq!(written, 0);
    }

    #[test]
    fn one_input_line_makes_one_output_line() {
        let (out, _, written) = run_over(&b"a\nb\nc\n"[..]);
        assert_eq!(out, b"a\nb\nc\n");
        assert_eq!(written, 3);
    }

    #[test]
    fn crlf_input_produces_lf_output() {
        let (out, _, written) = run_over(&b"a\r\nb\r\n"[..]);
        assert_eq!(out, b"a\nb\n");
        assert_eq!(written, 2);
    }

    #[test]
    fn a_missing_final_newline_still_renders_the_last_line() {
        let (out, _, written) = run_over(&b"a\nlast line, no newline"[..]);
        assert_eq!(out, b"a\nlast line, no newline\n");
        assert_eq!(written, 2);
    }

    #[test]
    fn invalid_utf8_passes_through_and_does_not_stop_the_stream() {
        let (out, diagnostics, written) = run_over(&b"{\"a\":1}\n\xff\xfe\xfd\nafter\n"[..]);
        assert_eq!(out, b"{\"a\":1}\n\xff\xfe\xfd\nafter\n");
        assert_eq!(written, 3);
        assert!(diagnostics.is_empty());
    }

    /// The hulog bug, from the pipeline's side: an over-long line must cost
    /// that line's tail and nothing else. In Go the loop simply ended here and
    /// the rest of the log was lost with exit code 0.
    #[test]
    fn an_over_long_line_does_not_end_the_stream() {
        let mut bytes = b"before\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_LINE + 4096));
        bytes.extend_from_slice(b"\nafter\n");

        let (out, diagnostics, written) = run_over(bytes);

        assert_eq!(written, 3, "before, the truncated line, after");
        assert!(out.starts_with(b"before\n"));
        assert!(out.ends_with(b"\nafter\n"), "the stream survived");
        // 7 ("before\n") + MAX_LINE + 1 + 6 ("after\n")
        assert_eq!(out.len(), 7 + MAX_LINE + 1 + 6);
        assert!(
            diagnostics.contains("line 2 is longer than 1 MiB"),
            "got {diagnostics:?}"
        );
    }

    #[test]
    fn repeated_truncation_warns_once_and_then_tallies() {
        let mut bytes = Vec::new();
        for _ in 0..3 {
            bytes.extend(std::iter::repeat_n(b'x', MAX_LINE + 16));
            bytes.push(b'\n');
        }

        let (_, diagnostics, written) = run_over(bytes);

        assert_eq!(written, 3);
        assert_eq!(
            diagnostics.matches("is longer than").count(),
            1,
            "one warning per run, not per line: {diagnostics:?}"
        );
        assert!(
            diagnostics.contains("3 lines were longer"),
            "{diagnostics:?}"
        );
    }

    /// Flush when the reader has run dry, and not before: three buffered lines
    /// cost one flush inside the loop plus the final one.
    #[test]
    fn flushes_only_when_the_reader_runs_dry() {
        let mut input = Input::from_bytes(&b"a\nb\nc\n"[..]);
        let mut out = CountingSink {
            bytes: Vec::new(),
            flushes: 0,
        };
        let mut diagnostics = Vec::new();

        let written = pump(&mut input, &mut out, &mut diagnostics, |line, sink| {
            sink.write_all(line)?;
            sink.write_all(b"\n")
        })
        .expect("must not fail");

        assert_eq!(written, 3);
        assert_eq!(out.bytes, b"a\nb\nc\n");
        assert_eq!(out.flushes, 2, "once on catching up, once at the end");
    }

    #[test]
    fn a_broken_pipe_becomes_exit_141_and_stays_quiet() {
        let mut input = Input::from_bytes(&b"a\n"[..]);
        let mut out = FailingSink(io::ErrorKind::BrokenPipe);
        let mut diagnostics = Vec::new();

        let err = pump(&mut input, &mut out, &mut diagnostics, |line, sink| {
            sink.write_all(line)
        })
        .expect_err("a closed stdout must surface");

        let domain = err.downcast_ref::<Error>();
        assert!(matches!(domain, Some(Error::BrokenPipe)), "got {err:?}");
    }

    #[test]
    fn other_write_errors_keep_their_context() {
        let mut input = Input::from_bytes(&b"a\n"[..]);
        let mut out = FailingSink(io::ErrorKind::PermissionDenied);
        let mut diagnostics = Vec::new();

        let err = pump(&mut input, &mut out, &mut diagnostics, |line, sink| {
            sink.write_all(line)
        })
        .expect_err("a failing stdout must surface");

        assert!(err.downcast_ref::<Error>().is_none(), "not a domain error");
        assert!(err.to_string().contains("writing to stdout"), "got {err:?}");
    }
}
