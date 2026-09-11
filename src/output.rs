//! The one and only place the program writes to stdout.
//!
//! Centralising it is what makes the broken-pipe rule enforceable: every write
//! goes through [`Output::stream`], so [`crate::pipeline`] has exactly one
//! `io::Error` to inspect for [`io::ErrorKind::BrokenPipe`].
//!
//! **`println!` is banned in this crate.** It panics when stdout is closed, so
//! `hog | head -5` would print a panic message instead of exiting quietly. Use
//! `writeln!` into [`Stream`].
//!
//! SIGPIPE stays at `SIG_IGN` (Rust's default). Restoring `SIG_DFL` would kill
//! the process mid-write, skipping the session's `Drop`-guard and orphaning the
//! command it spawned.

use std::io::{self, BufWriter, StdoutLock};

use anstream::AutoStream;

use crate::cli::ColorChoiceArg;
use crate::render::ColorLevel;

/// Capacity of the write buffer. Flushing is driven by the reader running dry,
/// not by this number, so it only bounds how much a redirected run batches.
const WRITE_BUFFER: usize = 64 * 1024;

/// The concrete write target. Buffering sits **outside** `AutoStream` so a
/// redirected run does one large write instead of one per line.
///
/// The `AutoStream` here is always the **pass-through** one. Its stripping mode
/// would be the obvious way to implement `--color never`, and it is wrong for
/// this program: `StripStream` feeds every byte to a VT parser, which silently
/// swallows bytes that are not valid UTF-8. A log line is not required to be
/// UTF-8, and passing those bytes through untouched is a §8 guarantee, so
/// colour is suppressed one layer up instead — see [`crate::render::theme`].
pub(crate) type Stream = BufWriter<AutoStream<StdoutLock<'static>>>;

/// Owns stdout and the colour decision made for it.
pub(crate) struct Output {
    stream: Stream,
    color: ColorLevel,
}

impl Output {
    /// Locks stdout and resolves `--color` against the environment.
    ///
    /// `anstream` handles `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE` and the
    /// is-a-tty test; [`anstyle_query::truecolor`] decides between
    /// [`ColorLevel::TrueColor`] and [`ColorLevel::Ansi256`].
    ///
    /// The answer is resolved **before** the stream is built, because the
    /// stream itself must not be the one that acts on it: see [`Stream`].
    pub(crate) fn stdout(choice: ColorChoiceArg) -> Self {
        let raw = io::stdout().lock();
        let resolved = resolve_choice(&raw, choice.into());
        let color = color_level(resolved, anstyle_query::truecolor());

        Self {
            // Pass-through, never `AutoStream::never`: bytes reach the fd as
            // written. With `ColorLevel::None` the theme emits no escapes, so
            // there is nothing for a stripping stream to have removed.
            stream: BufWriter::with_capacity(WRITE_BUFFER, AutoStream::always(raw)),
            color,
        }
    }

    /// How much colour this stream can carry. Fed to the theme once, at start.
    pub(crate) fn color(&self) -> ColorLevel {
        self.color
    }

    /// The write target. Everything that produces output goes through here.
    pub(crate) fn stream(&mut self) -> &mut Stream {
        &mut self.stream
    }
}

/// Asks `anstream` what `choice` means for this stream.
///
/// Only `Auto` needs the environment; an explicit `--color always/never` is its
/// own answer, and deliberately outranks `NO_COLOR`, exactly as `AutoStream`
/// would have decided internally.
fn resolve_choice(
    raw: &StdoutLock<'static>,
    choice: anstream::ColorChoice,
) -> anstream::ColorChoice {
    match choice {
        anstream::ColorChoice::Auto => AutoStream::choice(raw),
        explicit => explicit,
    }
}

/// Turns the stream's resolved colour choice into the palette depth the theme
/// should precompute.
///
/// Split out from [`Output::stdout`] because it is the only part of the
/// decision that can be tested without owning the process's real stdout.
fn color_level(choice: anstream::ColorChoice, truecolor: bool) -> ColorLevel {
    match choice {
        anstream::ColorChoice::Never => ColorLevel::None,
        // Truecolor is the exception, not the rule: Apple Terminal.app, which
        // is where this gets developed, has no 24-bit colour.
        _ if truecolor => ColorLevel::TrueColor,
        _ => ColorLevel::Ansi256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write as _;

    use anstream::ColorChoice;

    #[test]
    fn never_means_no_colour_even_on_a_truecolor_terminal() {
        assert_eq!(color_level(ColorChoice::Never, true), ColorLevel::None);
        assert_eq!(color_level(ColorChoice::Never, false), ColorLevel::None);
    }

    #[test]
    fn colour_is_downgraded_unless_colorterm_says_otherwise() {
        assert_eq!(color_level(ColorChoice::Always, false), ColorLevel::Ansi256);
        assert_eq!(
            color_level(ColorChoice::AlwaysAnsi, false),
            ColorLevel::Ansi256
        );
        assert_eq!(
            color_level(ColorChoice::Always, true),
            ColorLevel::TrueColor
        );
        assert_eq!(
            color_level(ColorChoice::AlwaysAnsi, true),
            ColorLevel::TrueColor
        );
    }

    /// `--color never` must reach the stream as `ColorChoice::Never`; the
    /// mapping lives in `cli.rs`, and this pins that it is the one we consume.
    #[test]
    fn cli_choice_maps_onto_the_stream_choice() {
        assert_eq!(ColorChoice::from(ColorChoiceArg::Never), ColorChoice::Never);
        assert_eq!(
            ColorChoice::from(ColorChoiceArg::Always),
            ColorChoice::Always
        );
        assert_eq!(ColorChoice::from(ColorChoiceArg::Auto), ColorChoice::Auto);
    }

    /// The whole broken-pipe design rests on buffering sitting *outside*
    /// `AutoStream`: one `writeln!` per line, one `write` syscall per flush.
    #[test]
    fn writes_are_buffered_until_flushed() {
        let mut stream = BufWriter::with_capacity(WRITE_BUFFER, AutoStream::always(Vec::new()));
        write!(stream, "10:32:01 [INF] hello").expect("buffered write cannot fail");

        assert!(
            stream.get_ref().as_inner().is_empty(),
            "nothing should reach the inner stream before a flush"
        );
        stream.flush().expect("flush cannot fail");
        assert_eq!(stream.get_ref().as_inner(), b"10:32:01 [INF] hello");
    }

    /// The regression this file exists to prevent. `AutoStream::never` is a
    /// `StripStream`, and a `StripStream` runs the bytes through a VT parser
    /// that swallows anything that is not valid UTF-8 — so a non-UTF-8 log line
    /// came out of `hog | cat` with bytes missing. The pass-through stream the
    /// program actually uses must not do that.
    #[test]
    fn the_write_path_does_not_swallow_invalid_utf8() {
        let line = b"\xff\xfe\xfd bad bytes";

        let mut stripping = AutoStream::never(Vec::new());
        stripping.write_all(line).expect("a Vec never fails");
        assert_ne!(
            stripping.into_inner(),
            line,
            "if this ever passes, anstream fixed it and the comment above is stale"
        );

        let mut passthrough = AutoStream::always(Vec::new());
        passthrough.write_all(line).expect("a Vec never fails");
        assert_eq!(passthrough.into_inner(), line);
    }
}
