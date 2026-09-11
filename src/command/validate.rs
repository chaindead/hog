//! The whitelist that stands between a positional argument and a remote shell.
//!
//! HLD §5 rejects quoting outright, and the reason is worth repeating here
//! because it is the whole justification for this module: hog cannot know
//! whether an argument lands in a word the *remote* shell will split again
//! (`'docker logs myapp-{1}-1'` — it will) or in a bare argv entry (`-l
//! app={1}` — it will not). In the first case quotes are required, in the
//! second they become literal and break the command. The problem has no
//! general solution, so hog does not attempt one: instead of making dangerous
//! characters safe, it refuses to carry them at all.
//!
//! Allowed: `[A-Za-z0-9._:/@-]`, at most [`MAX_ARG_BYTES`] bytes. That covers
//! docker/k8s/systemd unit names, `user@host` and `host:port`, and excludes
//! everything that could break *any* of the levels — space, quotes, `;`, `|`,
//! `&`, `$`, backtick, `<`, `>`, `(`, `)`, `*`, `?`, `~`, `\`, newline, ESC and
//! every other control byte, plus all of non-ASCII.
//!
//! # Parse, don't validate
//!
//! The whitelist is not a free function that callers are expected to remember
//! to call. It is the *only* constructor of [`SafeArg`], and
//! [`Template::render`](super::template::Template::render) takes `&[SafeArg]`
//! — so substitution physically cannot be handed a string nobody checked. The
//! check therefore happens before substitution by construction, which is what
//! HLD §5 asks for, rather than by a comment saying it should.

use std::fmt;

/// Longest positional argument hog will substitute, in bytes (HLD §5).
///
/// Bytes rather than chars: the limit exists to keep a pathological argument
/// out of an argv, and the kernel counts bytes. Non-ASCII is rejected by the
/// whitelist anyway, so for anything that gets this far the two are equal.
pub const MAX_ARG_BYTES: usize = 256;

/// The allowed characters, spelled once, for the error text and the tests.
pub const ALLOWED_DESCRIPTION: &str = "letters, digits and . _ - : / @";

/// Why an argument cannot be substituted.
///
/// The value is rendered with `{:?}`, never raw. That is deliberate: a
/// rejected argument is attacker-influenced text on its way to the user's
/// terminal, and `{:?}` turns an embedded ESC into `\u{1b}` instead of letting
/// it repaint the screen of the person reading the refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgError {
    /// Something outside the whitelist. The one that matters (HLD §5).
    #[error("argument {position} contains characters that are not allowed: {value:?}")]
    Disallowed {
        /// 1-based position as a human counts arguments: argument 2 is `{1}`.
        position: usize,
        /// The argument as typed, printed quoted and escaped.
        value: String,
    },

    /// `hog prod ""`. Empty passes a whitelist trivially — there is nothing in
    /// it to disallow — so it needs its own rule, otherwise `myapp-{1}-1` would
    /// quietly become `myapp--1` and the failure would surface as a confusing
    /// message from the far end.
    #[error("argument {position} is empty")]
    Empty {
        /// 1-based position, as in [`ArgError::Disallowed`].
        position: usize,
    },

    /// Over [`MAX_ARG_BYTES`]. Checked first, so that the `Disallowed` text
    /// never has to echo a megabyte of input back at the terminal.
    #[error("argument {position} is too long: {len} bytes, the limit is {limit}")]
    TooLong {
        /// 1-based position, as in [`ArgError::Disallowed`].
        position: usize,
        /// Length of the offending argument in bytes.
        len: usize,
        /// [`MAX_ARG_BYTES`], carried so the message needs no constant.
        limit: usize,
    },
}

impl ArgError {
    /// The 1-based argument position this refusal is about.
    pub fn position(&self) -> usize {
        match self {
            Self::Disallowed { position, .. }
            | Self::Empty { position }
            | Self::TooLong { position, .. } => *position,
        }
    }
}

/// A positional argument that has passed the whitelist.
///
/// Borrows rather than owns: the strings live in the parsed `Cli` for the whole
/// run, and copying them would only make it possible for a `SafeArg` to outlive
/// the check that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeArg<'a> {
    value: &'a str,
}

impl<'a> SafeArg<'a> {
    /// Checks one argument.
    ///
    /// `index` is the **0-based** index the template spells as `{index}`; the
    /// error text reports `index + 1`, because HLD §5 numbers arguments the way
    /// a human counts them ("argument 2" is `{1}`). The two numberings are the
    /// one genuine trap in this module, which is why the error also quotes the
    /// offending value — that, not the number, is what identifies it.
    ///
    /// # Errors
    ///
    /// [`ArgError`], in the order: too long, empty, disallowed character.
    pub fn parse(index: usize, raw: &'a str) -> Result<Self, ArgError> {
        let position = index.saturating_add(1);

        if raw.len() > MAX_ARG_BYTES {
            return Err(ArgError::TooLong {
                position,
                len: raw.len(),
                limit: MAX_ARG_BYTES,
            });
        }
        if raw.is_empty() {
            return Err(ArgError::Empty { position });
        }
        if !raw.chars().all(is_allowed) {
            return Err(ArgError::Disallowed {
                position,
                value: raw.to_owned(),
            });
        }

        Ok(Self { value: raw })
    }

    /// The checked text.
    pub fn as_str(self) -> &'a str {
        self.value
    }

    /// Builds a `SafeArg` without checking anything. **Test-only**, and the
    /// only way to prove the property that matters about substitution: a value
    /// containing a space must land in one argv word rather than splitting it.
    /// The whitelist makes that unreachable through [`parse`](Self::parse), so
    /// the test has to come in underneath it.
    #[cfg(test)]
    pub(crate) fn unchecked(value: &'a str) -> Self {
        Self { value }
    }
}

impl fmt::Display for SafeArg<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.value)
    }
}

/// The whitelist itself: `[A-Za-z0-9._:/@-]`.
///
/// `is_ascii_alphanumeric` and not `is_alphanumeric`: the latter is true for
/// `а` (Cyrillic) and for every other non-ASCII letter, and hog has no way to
/// know how the far end will encode those. A non-ASCII host name reaches hog as
/// punycode from the user, or not at all.
fn is_allowed(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '/' | '@' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<SafeArg<'_>, ArgError> {
        SafeArg::parse(0, raw)
    }

    fn accepts(raw: &str) -> bool {
        parse(raw).is_ok()
    }

    #[test]
    fn the_shapes_the_whitelist_exists_for_are_accepted() {
        // HLD §5: docker/k8s/systemd names, user@host, host:port.
        for raw in [
            "prod",
            "api",
            "myapp-api-1",
            "my_service.v2",
            "deploy@prod-1.example.com",
            "10.0.0.7:2222",
            "/var/log/app.log",
            "k8s/namespace/pod-abc123",
            "UPPER0987",
            "a",
            "-",
            "--since",
        ] {
            assert!(accepts(raw), "{raw:?} must be accepted");
        }
    }

    /// The reason this module exists. Every one of these is a way to break out
    /// of a word, and every one of them is rejected before substitution.
    #[test]
    fn injection_attempts_are_rejected() {
        for raw in [
            "api; rm -rf /",
            "api;rm",
            "api|tee",
            "api&",
            "api&&id",
            "$(id)",
            "${HOME}",
            "`id`",
            "api\nrm",
            "api\r\nrm",
            "api\ttab",
            "api rm",
            "'api'",
            "\"api\"",
            "api\\;",
            "api>out",
            "api<in",
            "api*",
            "api?",
            "~/secret",
            "a#b",
            "a!b",
            "a%b",
            "a^b",
            "a+b", // '+' is not on the list; `-` and `_` are.
            "a=b", // `-l app=api` belongs in the template, not in an argument.
            "a,b", // a comma would look like one argument and mean two.
            "a[0]",
            "a{0}",        // an argument may not smuggle a placeholder back in.
            "a\u{1b}[31m", // ESC: repaints the terminal of whoever reads the log.
            "a\u{0}b",     // NUL: truncates the argv entry at the syscall.
            "a\u{7}",      // BEL.
            "a\u{7f}",     // DEL.
            "прод",        // non-ASCII letters are not ASCII-alphanumeric.
            "café",
            "ｆｕｌｌｗｉｄｔｈ",
            "a\u{200b}b", // zero-width space: invisible, and not on the list.
        ] {
            assert!(
                matches!(parse(raw), Err(ArgError::Disallowed { .. })),
                "{raw:?} must be rejected"
            );
        }
    }

    #[test]
    fn the_error_names_the_argument_and_quotes_the_value() {
        // HLD §5 numbers arguments from 1: index 1 is `{1}` is "argument 2".
        let err = SafeArg::parse(1, "api; rm -rf /").expect_err("must be rejected");
        assert_eq!(err.position(), 2);
        assert_eq!(
            err.to_string(),
            "argument 2 contains characters that are not allowed: \"api; rm -rf /\""
        );
    }

    /// A rejected value is attacker-influenced text about to be printed to a
    /// terminal. `{:?}` is what keeps an ESC in it from being executed by the
    /// terminal rather than read by the user.
    #[test]
    fn a_control_byte_is_escaped_in_the_message_not_printed_raw() {
        let message = SafeArg::parse(0, "\u{1b}]0;pwned\u{7}")
            .expect_err("must be rejected")
            .to_string();
        assert!(!message.contains('\u{1b}'), "raw ESC reached stderr");
        assert!(message.contains("\\u{1b}"), "{message}");
    }

    #[test]
    fn empty_is_rejected_rather_than_silently_substituted() {
        assert_eq!(
            SafeArg::parse(1, ""),
            Err(ArgError::Empty { position: 2 }),
            "`myapp-{{1}}-1` with an empty {{1}} would become `myapp--1`"
        );
    }

    #[test]
    fn the_length_limit_is_256_bytes_inclusive() {
        let at_limit = "a".repeat(MAX_ARG_BYTES);
        assert!(accepts(&at_limit));

        let over = "a".repeat(MAX_ARG_BYTES + 1);
        assert_eq!(
            parse(&over).expect_err("must be rejected"),
            ArgError::TooLong {
                position: 1,
                len: MAX_ARG_BYTES + 1,
                limit: MAX_ARG_BYTES,
            }
        );
    }

    /// Length is checked first so that the refusal never echoes an enormous
    /// argument back at the terminal.
    #[test]
    fn an_over_long_argument_is_too_long_before_it_is_disallowed() {
        let huge = "; rm -rf /".repeat(1024);
        let message = parse(&huge).expect_err("must be rejected").to_string();
        assert!(message.starts_with("argument 1 is too long"), "{message}");
        assert!(message.len() < 120, "the whole value must not be echoed");
    }

    #[test]
    fn a_checked_argument_hands_back_exactly_what_went_in() {
        let arg = parse("deploy@prod-1:22").expect("accepted");
        assert_eq!(arg.as_str(), "deploy@prod-1:22");
        assert_eq!(arg.to_string(), "deploy@prod-1:22");
    }
}
