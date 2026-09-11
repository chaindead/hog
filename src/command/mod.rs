//! Command mode: `hog prod api` runs the config's `command` template.
//!
//! HLD §5. This module assembles the argv and nothing else — **no process is
//! spawned here**, and neither `mod.rs`, [`template`] nor [`validate`] so much
//! as mentions `std::process`. That split is the point: the security-critical
//! half of command mode (what ends up in the argv, and what is refused) is
//! pure, so it is covered by unit tests that run in microseconds and need no
//! ssh, no network and no fixture host. `session.rs` gets the spawning, the
//! held-open stdin and the `Drop`-guard, and takes a finished [`CommandPlan`].
//!
//! The whole of it is [`plan`]:
//!
//! ```text
//!   settings.command  ──shlex::split──▶ words ──┐
//!   hog's arguments   ──whitelist────▶ SafeArg ─┴─substitute──▶ CommandPlan
//! ```
//!
//! # Why [`plan`] is also the input-mode table
//!
//! HLD §6 picks the mode from the positional arguments, with `IsTerminal`
//! breaking the tie. Four of its rows end in "run the command", and `plan`
//! answers all four without the caller having to reason about placeholders:
//!
//! | arguments | `command` template | what `plan` does |
//! |---|---|---|
//! | yes | yes | substitutes, or refuses the arguments |
//! | yes | no | runs the built-in [`DEFAULT_COMMAND`] |
//! | no (terminal) | yes, no `{N}` | returns the plan — arity 0 matches |
//! | no (terminal) | yes, with `{N}` | `template needs 2 arguments, got 0` |
//!
//! HLD §5 cancelled the old "no template is an error" rule: **there is always a
//! template**, because a config without a `command` key falls back to
//! `echo {@}`. A freshly installed hog therefore shows where its arguments go
//! instead of refusing to do anything, which is also why `--help` can print a
//! command block unconditionally.
//!
//! The caller still owns the two rows that never reach here: reading stdin, and
//! the short help on a bare terminal with no template — the one place where
//! "configured" and "not configured" still differ, and the reason
//! `Settings::command` stays an `Option`.

// The impure half, and the only one that is not `pub`: nothing outside the
// crate has any business spawning hog's child process, and its behaviour is
// observable end to end from `tests/cmd_session.rs` through the real binary.
pub(crate) mod session;
pub mod template;
pub mod validate;

use std::borrow::Cow;
use std::fmt;

use crate::settings::Settings;

pub use template::{Template, TemplateError};
pub use validate::{ArgError, SafeArg};

/// The template hog runs when the config file sets no `command` (HLD §5).
///
/// `hog a b c` prints `a b c`. That is the whole point: it is not a useful
/// command, it is a *legible* one — a brand-new install answers the question
/// "where do my arguments go?" on the first run, instead of erroring out, and
/// `--help` has a command block to print before the user has written a config.
///
/// `echo` and not `printf`: `echo` takes any number of arguments, which is what
/// makes `{@}` alone the right template for it, and it is a `/bin/echo` on
/// every unix (there is no local shell here, so the builtin is not what runs).
pub const DEFAULT_COMMAND: &str = "echo {@}";

/// The template this run will use: the configured one, or [`DEFAULT_COMMAND`].
///
/// Separate from [`plan`] because `--help` needs the same answer without
/// assembling anything (HLD §6, "Динамический help"). Whether the answer was
/// configured or built in is `settings.command.is_some()` — the distinction the
/// input-mode table still needs, and the reason the field is an `Option`.
pub fn command_text(settings: &Settings) -> &str {
    settings.command.as_deref().unwrap_or(DEFAULT_COMMAND)
}

/// Everything [`plan`] can refuse, with the guidance of HLD §5 in its
/// `Display`.
///
/// The hint is part of the message rather than a separate `hint()` the printer
/// has to remember to call, so the refusal reads the same wherever it is
/// printed from. The inner errors are held in fields named `reason` rather than
/// `source` on purpose: a source would make `report` print the one-line summary
/// a second time as `caused by:`, under a hint that has already explained it.
///
/// Every variant is a runtime failure, i.e. **exit code 1**. There is no
/// `exit_code` here and no new variant in [`crate::error::Error`]: an error
/// that does not downcast to that enum already exits 1 through `report`, which
/// is exactly the code HLD §6 assigns to "config, template, bad argument".
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// An argument failed the whitelist of HLD §5.
    #[error(
        "{reason}\n\n  \
         Arguments are substituted into a shell command, so hog only accepts\n  \
         {allowed}\n\n  \
         Check what would run:  hog --dry-run {invocation}"
    )]
    Argument {
        /// Which argument, and why.
        reason: ArgError,
        /// The arguments as typed, re-quoted for display.
        invocation: String,
        /// [`validate::ALLOWED_DESCRIPTION`], so the message needs no constant.
        allowed: &'static str,
    },

    /// The template cannot be split, or does not match the arguments.
    #[error(
        "{reason}\n\n  \
         The command template is:\n\n      \
         {template:?}\n\n  \
         {{0}} is the first argument, {{1}} the second, and so on;\n  \
         {{@}} is whatever no index took, and needs whitespace around it.\n  \
         A bare {{}} stays literal; write {{{{0}}}} for a literal {{0}}.\n  \
         Check what would run, without running it:  hog --dry-run ARG..."
    )]
    Template {
        /// What is wrong with it.
        reason: TemplateError,
        /// The template exactly as the config file spells it. Printed with
        /// `{:?}` so that it reads like the TOML line it came from — and so
        /// that a control byte pasted into a config cannot repaint the
        /// terminal of whoever is reading the error.
        template: String,
    },
}

/// A finished argv: what to run, and with what.
///
/// Holding the program separately from its arguments is not cosmetic — it is
/// the shape `Command::new(program).args(args)` needs, and it makes the
/// "program" of a template that starts with `{0}` an ordinary substituted word
/// rather than a special case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPlan {
    program: String,
    args: Vec<String>,
}

impl CommandPlan {
    /// The executable to run, resolved against `PATH` by the OS, never by hog.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Its arguments, already substituted.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The whole argv, program first.
    pub fn argv(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.program.as_str()).chain(self.args.iter().map(String::as_str))
    }

    /// What `--dry-run` prints: the argv, one word per line, and nothing else
    /// (HLD §5 — print it and exit, running nothing).
    ///
    /// One word per line rather than one shell-ish line, because the question
    /// `--dry-run` exists to answer is *where the word boundaries are*: whether
    /// `2>&1` ended up inside the quoted remote command or beside it, and
    /// whether an argument with a space became one word or two. A
    /// space-separated line would hide exactly that. Words that are not plain
    /// printable ASCII are shown with `{:?}`, so a tab or a space is visible
    /// rather than implied.
    pub fn dry_run_text(&self) -> String {
        self.argv()
            .map(|word| show_word(word).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Builds a plan from already-substituted words.
    ///
    /// `words` comes from [`Template::render`], and is empty in exactly one
    /// case: a template that is nothing but `{@}`, rendered with no arguments
    /// left over. Reported as [`TemplateError::Empty`] rather than answered,
    /// because the alternative would be inventing a program name — and
    /// `command = "{@}"` really does say "the arguments are the command", so a
    /// run of it with nothing to run is a genuinely empty template.
    fn from_words(words: Vec<String>) -> Result<Self, TemplateError> {
        let mut words = words.into_iter();
        let program = words.next().ok_or(TemplateError::Empty)?;
        Ok(Self {
            program,
            args: words.collect(),
        })
    }
}

impl fmt::Display for CommandPlan {
    /// The argv on one line, for a log message or an error. Not what
    /// `--dry-run` prints — see [`CommandPlan::dry_run_text`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for word in self.argv() {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            f.write_str(&show_word(word))?;
        }
        Ok(())
    }
}

/// Assembles the argv for this run.
///
/// The order is the order HLD §5 requires and the tests pin: split the template
/// first (a broken template is a config defect, and it is wrong to blame the
/// arguments for it), then check the count, then check every argument against
/// the whitelist, and only then substitute. The whitelist running before
/// substitution is not enforced by this order alone — [`Template::render`]
/// takes `SafeArg`, so it could not be called any other way.
///
/// Note what the count check is *not* allowed to skip: with `{@}` in the
/// template there is no upper bound on the arguments, so every one of them —
/// including the ones that end up in the tail — goes through the whitelist.
/// A variadic template widens what may be passed, never what may be in it.
///
/// # Errors
///
/// [`CommandError`], which carries the long-form guidance in its `Display` and
/// maps to exit code 1.
pub fn plan(settings: &Settings, args: &[String]) -> Result<CommandPlan, CommandError> {
    // Never `None`: a config without a `command` key runs `echo {@}` (HLD §5).
    let text = command_text(settings);

    let template = template::parse(text).map_err(|reason| CommandError::Template {
        reason,
        template: text.to_owned(),
    })?;

    template
        .check_arity(args.len())
        .map_err(|reason| CommandError::Template {
            reason,
            template: text.to_owned(),
        })?;

    let safe: Vec<SafeArg<'_>> = args
        .iter()
        .enumerate()
        .map(|(index, raw)| SafeArg::parse(index, raw))
        .collect::<Result<_, _>>()
        .map_err(|reason| CommandError::Argument {
            reason,
            invocation: invocation(args),
            allowed: validate::ALLOWED_DESCRIPTION,
        })?;

    let words = template
        .render(&safe)
        .map_err(|reason| CommandError::Template {
            reason,
            template: text.to_owned(),
        })?;

    CommandPlan::from_words(words).map_err(|reason| CommandError::Template {
        reason,
        template: text.to_owned(),
    })
}

/// The arguments as a `hog --dry-run …` tail, for the hints.
///
/// Re-quoted rather than pasted in raw: these are the arguments that just
/// failed a whitelist, so they are the last thing that should reach a terminal
/// unescaped. HLD §5 prints them bare in its example, where they happen to be
/// harmless.
fn invocation(args: &[String]) -> String {
    args.iter()
        .map(|arg| show_word(arg).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A word as it should be shown to a human: itself when it is plain printable
/// ASCII, and `{:?}`-quoted when it is not.
///
/// The quoting is for **display only**. It is never applied to anything that
/// goes into an argv, which is the whole subject of HLD §5: hog does not quote
/// what it substitutes, it refuses what it cannot carry.
fn show_word(word: &str) -> Cow<'_, str> {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\');
    if plain {
        Cow::Borrowed(word)
    } else {
        Cow::Owned(format!("{word:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings carrying just a template, the way `config::load` would leave
    /// them.
    fn settings(command: Option<&str>) -> Settings {
        Settings {
            command: command.map(str::to_owned),
            ..Settings::default()
        }
    }

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn plan_for(command: &str, args: &[&str]) -> Result<CommandPlan, CommandError> {
        plan(&settings(Some(command)), &owned(args))
    }

    fn argv(command: &str, args: &[&str]) -> Vec<String> {
        plan_for(command, args)
            .expect("must plan")
            .argv()
            .map(str::to_owned)
            .collect()
    }

    /// The invocation from HLD §5, end to end.
    #[test]
    fn the_starter_template_plans_the_documented_argv() {
        let plan = plan_for(
            "ssh -tt -o ServerAliveInterval=15 {0} 'docker logs -f --since 1h myapp-{1}-1'",
            &["prod", "api"],
        )
        .expect("must plan");

        assert_eq!(plan.program(), "ssh");
        assert_eq!(
            plan.args(),
            [
                "-tt",
                "-o",
                "ServerAliveInterval=15",
                "prod",
                "docker logs -f --since 1h myapp-api-1",
            ]
        );
    }

    /// HLD §5 by name: neither command may be rewritten on its way through.
    #[test]
    fn the_two_commands_that_must_pass_through_untouched_do() {
        assert_eq!(
            argv("find . -name '*.log' -exec cat {} \\;", &[]),
            ["find", ".", "-name", "*.log", "-exec", "cat", "{}", ";"]
        );
        assert_eq!(
            argv("docker ps --format '{{.Names}}'", &[]),
            ["docker", "ps", "--format", "{{.Names}}"]
        );
    }

    /// A quoted section is one argv entry, and a substituted value never adds
    /// another one.
    #[test]
    fn a_substituted_argument_never_splits_a_word() {
        let plan = plan_for("ssh {0} 'systemctl status {1}.service'", &["prod", "api"])
            .expect("must plan");
        assert_eq!(plan.args().len(), 2);
        assert_eq!(plan.args()[1], "systemctl status api.service");
    }

    // ================================================ the input-mode table §6

    /// Row four: no arguments, a template without `{N}`. Runs.
    #[test]
    fn a_template_without_placeholders_plans_with_no_arguments() {
        assert_eq!(
            argv("kubectl logs -f -l app=api", &[]),
            ["kubectl", "logs", "-f", "-l", "app=api"]
        );
    }

    /// Row five: no arguments, a template with `{N}`. The message is the one
    /// HLD §6 spells out.
    #[test]
    fn a_template_with_placeholders_and_no_arguments_is_an_arity_error() {
        let err = plan_for("ssh {0} 'logs {1}'", &[]).expect_err("must fail");
        assert!(
            matches!(
                &err,
                CommandError::Template {
                    reason: TemplateError::Arity {
                        needed: 2,
                        given: 0
                    },
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("template needs 2 arguments, got 0"),
            "{err}"
        );
    }

    /// Row two: arguments, no template. HLD §5 cancelled the old refusal — the
    /// built-in `echo {@}` runs, so `hog prod api` prints `prod api` on a
    /// brand-new install instead of explaining that nothing is configured.
    #[test]
    fn arguments_without_a_template_run_the_built_in_echo() {
        let plan = plan(&settings(None), &owned(&["prod", "api"])).expect("must plan");
        assert_eq!(plan.program(), "echo");
        assert_eq!(plan.args(), ["prod", "api"]);
    }

    /// The built-in takes any number of arguments, zero included: it is
    /// `echo {@}`, and `{@}` has no ceiling.
    #[test]
    fn the_built_in_template_takes_any_number_of_arguments() {
        for args in [&[][..], &["one"][..], &["a", "b", "c", "d", "e"][..]] {
            let plan = plan(&settings(None), &owned(args)).expect("must plan");
            assert_eq!(plan.program(), "echo");
            assert_eq!(plan.args(), args, "{args:?}");
        }
    }

    /// The built-in is a real template, not a special case in the planner: it
    /// goes through the same parse, the same whitelist and the same
    /// substitution as anything a user writes.
    #[test]
    fn the_built_in_template_is_parsed_like_any_other() {
        let parsed = template::parse(DEFAULT_COMMAND).expect("the built-in must parse");
        assert!(parsed.is_variadic());
        assert_eq!(parsed.required_arity(), 0);

        let err = plan(&settings(None), &owned(&["api; rm -rf /"])).expect_err("must refuse");
        assert!(matches!(err, CommandError::Argument { .. }), "{err:?}");
    }

    /// A configured template wins over the built-in; the `Option` is only ever
    /// read here, so "configured" and "built in" cannot drift apart.
    #[test]
    fn a_configured_template_wins_over_the_built_in() {
        assert_eq!(command_text(&settings(None)), DEFAULT_COMMAND);
        assert_eq!(command_text(&settings(Some("ssh {0}"))), "ssh {0}");
        assert_eq!(argv("kubectl logs -f", &[]), ["kubectl", "logs", "-f"]);
    }

    // =========================================================== arity, again

    #[test]
    fn a_missing_argument_is_an_error() {
        let err = plan_for("ssh {0} 'logs {1}'", &["prod"]).expect_err("must fail");
        assert!(
            err.to_string()
                .starts_with("template needs 2 arguments, got 1"),
            "{err}"
        );
    }

    #[test]
    fn a_surplus_argument_is_an_error() {
        let err = plan_for("ssh {0}", &["prod", "api"]).expect_err("must fail");
        assert!(
            err.to_string()
                .starts_with("template needs 1 argument, got 2"),
            "{err}"
        );
    }

    // ================================================================ {@}

    /// The flagship template of HLD §5, planned end to end at N = 0, 1 and 3
    /// arguments left over. `'docker logs -f {@}'` is one shlex word, so the
    /// tail is joined inside it and the argv keeps exactly four entries.
    #[test]
    fn the_flagship_variadic_template_keeps_its_argv_shape() {
        const TEMPLATE: &str = "ssh -tt {0} 'docker logs -f {@}'";

        for (args, remote) in [
            (&["prod"][..], "docker logs -f "),
            (&["prod", "api"][..], "docker logs -f api"),
            (
                &["prod", "api", "--tail", "50"][..],
                "docker logs -f api --tail 50",
            ),
        ] {
            let plan = plan_for(TEMPLATE, args).expect("must plan");
            assert_eq!(plan.program(), "ssh");
            assert_eq!(plan.args(), ["-tt", "prod", remote], "{args:?}");
        }
    }

    /// The interval HLD §8 says comes for free once `{@}` exists: no
    /// `--since` flag, just a tail that reaches the remote command.
    #[test]
    fn the_variadic_tail_carries_flags_through_to_the_remote_command() {
        let plan =
            plan_for("ssh {0} 'docker logs -f {@}'", &["prod", "--since", "30m"]).expect("plans");
        assert_eq!(plan.args(), ["prod", "docker logs -f --since 30m"]);
    }

    /// A `{@}` that is a word of its own splices, so the argv grows and
    /// shrinks with the arguments — and shrinks all the way to nothing.
    #[test]
    fn a_standalone_tail_changes_the_number_of_argv_words() {
        assert_eq!(
            argv("kubectl logs {0} {@}", &["pod"]),
            ["kubectl", "logs", "pod"]
        );
        assert_eq!(
            argv("kubectl logs {0} {@}", &["pod", "-c", "app"]),
            ["kubectl", "logs", "pod", "-c", "app"]
        );
    }

    /// Every argument is whitelisted, tail included: `{@}` widens how many may
    /// be passed, never what may be in them.
    #[test]
    fn an_argument_in_the_tail_is_whitelisted_too() {
        let err = plan_for("ssh {0} 'logs {@}'", &["prod", "ok", "api; rm -rf /"])
            .expect_err("must refuse");
        let CommandError::Argument { reason, .. } = &err else {
            panic!("expected an argument refusal, got {err:?}");
        };
        assert_eq!(reason.position(), 3, "the third argument is the bad one");
    }

    /// The two ways `{@}` can be written wrong reach the caller as template
    /// faults, with the template quoted back (HLD §5).
    #[test]
    fn a_misplaced_tail_is_reported_as_a_template_fault() {
        let glued = plan_for("ssh {0} myapp-{@}-1", &["prod"]).expect_err("must fail");
        assert!(
            glued.to_string().starts_with("command template glues {@}"),
            "{glued}"
        );
        assert!(glued.to_string().contains("myapp-{@}-1"), "{glued}");

        let twice = plan_for("ssh {0} {@} {@}", &["prod"]).expect_err("must fail");
        assert!(
            twice
                .to_string()
                .starts_with("command template uses {@} more than once"),
            "{twice}"
        );
    }

    /// The variadic shortfall, through `plan`: the hint says "at least", so
    /// nobody reads the floor as an exact count.
    #[test]
    fn a_variadic_template_with_too_few_arguments_says_at_least() {
        let err = plan_for("ssh {0} 'logs {@}'", &[]).expect_err("must fail");
        assert!(
            err.to_string()
                .starts_with("template needs at least 1 argument, got 0"),
            "{err}"
        );
        assert!(
            err.to_string().contains("{@} is whatever no index took"),
            "{err}"
        );
    }

    /// `--dry-run` has one job — showing where the word boundaries are — and
    /// `{@}` is the feature that makes those boundaries hard to guess.
    #[test]
    fn dry_run_shows_where_the_tail_landed() {
        let spliced = plan_for("echo {@}", &["a", "b"]).expect("plans");
        assert_eq!(spliced.dry_run_text(), "echo\na\nb");

        let joined = plan_for("ssh {0} 'logs {@}'", &["prod", "a", "b"]).expect("plans");
        assert_eq!(joined.dry_run_text(), "ssh\nprod\n\"logs a b\"");
    }

    /// An empty tail inside a word leaves the space that was before it. The
    /// remote shell drops it when it re-splits, and `--dry-run` shows it
    /// rather than hiding it — which is the point of quoting the word.
    #[test]
    fn an_empty_tail_inside_a_word_is_visible_in_the_dry_run() {
        let plan = plan_for("ssh {0} 'docker logs -f {@}'", &["prod"]).expect("plans");
        assert_eq!(plan.dry_run_text(), "ssh\nprod\n\"docker logs -f \"");
    }

    /// `command = "{@}"` says the arguments *are* the command. With none, the
    /// argv is empty, and an empty argv is an empty template rather than an
    /// invented program name.
    #[test]
    fn a_template_that_is_only_a_tail_needs_something_to_run() {
        assert_eq!(argv("{@}", &["ls", "-la"]), ["ls", "-la"]);
        let err = plan_for("{@}", &[]).expect_err("must fail");
        assert!(
            err.to_string().starts_with("command template is empty"),
            "{err}"
        );
    }

    // ================================================================ refusals

    /// The security boundary, through the public entry point this time.
    #[test]
    fn injection_attempts_never_reach_an_argv() {
        for hostile in [
            "api; rm -rf /",
            "api|tee /tmp/x",
            "$(id)",
            "`id`",
            "api\nrm -rf /",
            "api && id",
            "api\u{1b}[2J",
            "api\u{0}",
            "'api'",
            "../../etc/shadow; id",
        ] {
            let Err(err) = plan_for("ssh {0} 'logs {1}'", &["prod", hostile]) else {
                panic!("{hostile:?} must never reach an argv");
            };
            assert!(
                matches!(err, CommandError::Argument { .. }),
                "{hostile:?}: {err:?}"
            );
        }
    }

    #[test]
    fn the_refusal_says_which_argument_and_what_is_allowed() {
        let err = plan_for("ssh {0} 'logs {1}'", &["prod", "api; rm -rf /"])
            .expect_err("must be refused");
        let message = err.to_string();

        assert!(
            message.starts_with(
                "argument 2 contains characters that are not allowed: \"api; rm -rf /\""
            ),
            "{message}"
        );
        assert!(
            message.contains("letters, digits and . _ - : / @"),
            "{message}"
        );
        assert!(message.contains("hog --dry-run"), "{message}");
        // The hint quotes the hostile argument instead of pasting it in raw.
        assert!(message.contains("prod \"api; rm -rf /\""), "{message}");
    }

    #[test]
    fn an_unclosed_quote_in_the_config_is_reported_as_a_template_fault() {
        let err = plan_for("ssh {0} 'docker logs", &["prod"]).expect_err("must fail");
        let message = err.to_string();
        assert!(
            message.starts_with("command template has an unclosed quote"),
            "{message}"
        );
        // The hint shows the template back, spelled the way the TOML line is.
        assert!(message.contains("\"ssh {0} 'docker logs\""), "{message}");
    }

    #[test]
    fn an_empty_template_is_reported_rather_than_run() {
        let err = plan(&settings(Some("")), &[]).expect_err("must fail");
        assert!(
            err.to_string().starts_with("command template is empty"),
            "{err}"
        );
    }

    #[test]
    fn a_template_with_a_hole_in_its_indices_is_reported() {
        let err = plan_for("ssh {0} 'logs {2}'", &["a", "b", "c"]).expect_err("must fail");
        assert!(
            err.to_string()
                .starts_with("command template uses {2} but never {1}"),
            "{err}"
        );
    }

    /// A config with an ESC in it must not repaint the terminal of whoever is
    /// reading the error about it.
    #[test]
    fn a_control_byte_in_the_template_is_escaped_in_the_hint() {
        let err = plan(&settings(Some("ssh \u{1b}[31m {0}")), &[]).expect_err("must fail");
        let message = err.to_string();
        assert!(!message.contains('\u{1b}'), "raw ESC reached stderr");
        assert!(message.contains("\\u{1b}"), "{message}");
    }

    // ================================================================ dry run

    /// `--dry-run` has to make the word boundaries visible: the quoted remote
    /// command is one line, not four.
    #[test]
    fn dry_run_prints_one_word_per_line() {
        let plan = plan_for("ssh -tt {0} 'docker logs -f myapp-{1}-1'", &["prod", "api"])
            .expect("must plan");
        assert_eq!(
            plan.dry_run_text(),
            "ssh\n-tt\nprod\n\"docker logs -f myapp-api-1\""
        );
    }

    #[test]
    fn dry_run_shows_a_plain_word_without_quotes() {
        let plan = plan_for("kubectl logs -f -l app=api", &[]).expect("must plan");
        assert_eq!(plan.dry_run_text(), "kubectl\nlogs\n-f\n-l\napp=api");
    }

    #[test]
    fn the_one_line_form_is_the_same_words_separated_by_spaces() {
        let plan = plan_for("ssh -tt {0} 'docker logs {1}'", &["prod", "api"]).expect("must plan");
        assert_eq!(plan.to_string(), "ssh -tt prod \"docker logs api\"");
    }

    #[test]
    fn an_empty_word_is_visible_in_the_dry_run() {
        // `ssh '' host` is a deliberately empty argv entry, and it has to look
        // like one rather than like a blank line.
        let plan = plan_for("ssh '' host", &[]).expect("must plan");
        assert_eq!(plan.dry_run_text(), "ssh\n\"\"\nhost");
    }
}
