//! The dynamic `--help` of HLD §6: the user's own `command` template, printed
//! under the standard clap sections.
//!
//! Without it `hog --help` documents its positional arguments as `[ARGS]...`
//! and stops there, which answers nothing — the whole meaning of those
//! arguments lives in a template in a file the help text never mentions. With
//! it, `--help` is also the answer to "I installed hog, now what?":
//!
//! ```text
//! Configured command (/home/you/.hog.toml):
//!   ssh -tt -o ServerAliveInterval=15 {0} 'docker logs -f --since 1h myapp-{1}-1'
//!
//! Takes 2 arguments:
//!   hog <ARG0> <ARG1>
//!
//! Example:
//!   hog prod api
//! ```
//!
//! # The two-pass parse, and the trap in it
//!
//! `--config PATH` decides which file the block describes, and clap only hands
//! that value over *after* a successful parse — by which time `--help` has
//! already been printed. So argv is parsed twice: once loosely, for the path
//! alone, and once for real with the finished block attached.
//!
//! The loose pass **must** disable the help and version flags. Both are
//! implemented as parse errors (`ErrorKind::DisplayHelp`,
//! `ErrorKind::DisplayVersion`), so with either one live, `hog --config
//! other.toml --help` fails the loose parse before it reaches `--config`, the
//! path comes back `None`, and the block quietly describes *the default config*
//! — a wrong answer that looks exactly like a right one. That is the failure
//! HLD §6 calls out, and [`preflight_config`] closes it.
//!
//! HLD §6's own snippet closes only half of it. It writes
//! `.disable_version_flag(true)`, which removes clap's **generated** version
//! flag — and the grammar in the same section does not use the generated flag:
//! it declares `version: ()` with `ArgAction::Version` by hand, so that `-v`
//! can be the short form. `disable_version_flag` does not touch a hand-declared
//! argument, so `hog --config other.toml --version` would still error out of
//! the loose pass. Harmless today, because that invocation prints a version and
//! never looks at a config — but it is the same hole, and it is closed here the
//! only way that works: the declared argument's action is swapped out for the
//! duration of the loose pass.
//!
//! And `disable_help_flag(true)` on its own is not enough either, for a reason
//! that only shows up when the flags are written in the other order. Disabling
//! the flag does not make `--help` *harmless*, it makes it **unknown** — and
//! `ignore_errors(true)` keeps the matches collected so far, not the ones that
//! come after the argument it choked on. So `hog --config other.toml --help`
//! worked while `hog --help --config other.toml` silently described the default
//! config: the same wrong-but-plausible answer, reached from the other end of
//! the command line. Both flags therefore get the same treatment — an inert
//! `SetTrue` argument standing in for the real one, so the loose pass consumes
//! it like any other flag and keeps reading.

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command, CommandFactory as _, FromArgMatches as _};

use crate::cli::Cli;
use crate::command::{self, Template};
use crate::config::{self, Env};

/// The id clap gives the `--config` argument, and the one `--version` carries
/// because HLD §6 declares it by hand rather than letting clap generate it.
const CONFIG_ARG: &str = "config";
const VERSION_ARG: &str = "version";

/// Id of the inert `-h/--help` the loose pass puts in place of clap's own.
///
/// Deliberately not `"help"`: `disable_help_subcommand` leaves the name free,
/// but reusing it would make a future `mut_arg("help", …)` ambiguous between
/// this stand-in and the real flag.
const PREFLIGHT_HELP_ARG: &str = "preflight-help";

/// Words the `Example:` line is built from, in order.
///
/// Three is enough for every shape the block prints: beyond that the example
/// falls back to `arg3`, `arg4`, … , which is honest — hog assigns no meaning
/// to a position, so inventing a fourth plausible-sounding word would be
/// inventing a convention.
const EXAMPLE_WORDS: [&str; 3] = ["prod", "api", "web"];

/// How many extra words a variadic example shows landing in `{@}`.
///
/// Two, not one: with a single extra word the reader cannot tell a tail from
/// one more indexed placeholder.
const TAIL_EXAMPLE_WORDS: usize = 2;

/// Parses the process command line with the command block already built.
///
/// This is what `main` calls instead of `Cli::parse()`. Usage errors, `--help`
/// and `--version` are handled by clap exactly as they would have been, and
/// exit with the codes HLD §6's table gives them.
pub fn parse() -> Cli {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let matches = command_for(&argv, &Env::from_process()).get_matches_from(&argv);
    match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        // Unreachable in practice: the derive and the matches come from the
        // same `Cli`. `exit` rather than a panic keeps the exit-code table
        // honest even if that ever stops being true.
        Err(err) => err.exit(),
    }
}

/// Writes the same help text a bare `--help` would print.
///
/// Used for the last row of the mode table (HLD §6): stdin is a terminal, there
/// are no positional arguments and no `command` template. That row is the one
/// place a missing template is still visible, and the help text is what makes it
/// actionable — the block below says the built-in `echo {@}` is what would run
/// and names the file a template goes in.
///
/// Writes are best-effort: this runs while the process is already on its way out
/// with code 2, and a closed stderr must not become a second failure.
pub(crate) fn print_usage_hint<W: std::io::Write>(stderr: &mut W) {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let help = command_for(&argv, &Env::from_process()).render_help();
    // Not `eprintln!`: the `println!` family panics on a broken pipe.
    let _ = write!(stderr, "{help}");
}

/// The clap command for this invocation, with the command block appended.
fn command_for(argv: &[OsString], env: &Env) -> Command {
    let explicit = preflight_config(argv, env);
    Cli::command().after_help(block(explicit.as_deref(), env))
}

/// The loose first pass: `--config PATH`, or `$HOG_CONFIG`, or nothing.
///
/// See the module docs for why the two flags are turned off. `ignore_errors`
/// covers everything else that can go wrong in a pass over a command line that
/// was never meant to succeed here — a missing value, an unknown flag, a
/// subcommand typo — and each of those leaves the *real* parse to produce the
/// error message the user sees.
///
/// The fallback to `env.hog_config` mirrors what clap does with
/// `env = "HOG_CONFIG"` on the same argument, so the loose pass and the real one
/// resolve the same file.
fn preflight_config(argv: &[OsString], env: &Env) -> Option<PathBuf> {
    let pre = Cli::command()
        .ignore_errors(true)
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        // Disabling the flag above only makes `--help` *unknown*, and an
        // unknown argument ends what `ignore_errors` collects — so anything
        // spelled after it, `--config` included, would be lost. This puts an
        // inert flag of the same spelling back, global so that it is also
        // consumed after a subcommand.
        .arg(
            Arg::new(PREFLIGHT_HELP_ARG)
                .short('h')
                .long("help")
                .global(true)
                .action(ArgAction::SetTrue),
        )
        // Removes clap's *generated* version flag. The declared one is next.
        .disable_version_flag(true)
        // The declared `-v/--version` of HLD §6. `ArgAction::Version` is a
        // parse error like `--help` is, so it has to stop being one here.
        .mut_arg(VERSION_ARG, |arg| arg.action(ArgAction::SetTrue))
        .try_get_matches_from(argv);

    pre.ok()
        .and_then(|matches| matches.get_one::<PathBuf>(CONFIG_ARG).cloned())
        .or_else(|| env.hog_config.as_deref().map(PathBuf::from))
}

/// Which template `--help` has to describe, and where it came from.
enum Configured {
    /// The config file sets a `command`.
    File { path: PathBuf, text: String },
    /// No `command` key, so the built-in `echo {@}` is what would run.
    /// `path` is where a template would go, and is `None` only when there is
    /// nowhere to put a config at all (no `$HOME`).
    BuiltIn { path: Option<PathBuf> },
    /// The config could not be read at all — a missing `--config`, a syntax
    /// error, a directory in place of the file.
    Unreadable { why: String },
}

/// Reads the config far enough to name the template, never failing.
///
/// `--help` has to print *something* on every input, including the ones a real
/// run would refuse, so a load failure becomes a line of the block rather than
/// an error. Unknown-key warnings are deliberately not emitted here: they belong
/// to a run, and `--help` is not one.
fn configured(explicit: Option<&Path>, env: &Env) -> Configured {
    match config::load::load(explicit, env) {
        Ok(Some(loaded)) => match loaded.model.command {
            Some(text) => Configured::File {
                path: loaded.location.path,
                text,
            },
            None => Configured::BuiltIn {
                path: Some(loaded.location.path),
            },
        },
        // No file anywhere, or nowhere to look. Either way the built-in runs,
        // and the path is where the user should write a template.
        Ok(None) => Configured::BuiltIn {
            path: config::discover::locate(explicit, env).map(|location| location.path),
        },
        Err(err) => Configured::Unreadable {
            why: err.to_string(),
        },
    }
}

/// Builds the whole block, from the first heading to the last example.
fn block(explicit: Option<&Path>, env: &Env) -> String {
    let mut out = String::new();

    match configured(explicit, env) {
        Configured::File { path, text } => {
            let _ = writeln!(out, "Configured command ({}):", path.display());
            let _ = writeln!(out, "  {}", one_line(&text));
            describe(&mut out, &text);
        }

        Configured::BuiltIn { path } => {
            // Not "no config yet": since hog writes `~/.hog.toml` on the first
            // run, the file is there on every machine that has run hog once and
            // this branch is reached because it sets no `command`, which is the
            // starter's own state.
            let _ = writeln!(out, "Configured command (built in — none is set):");
            let _ = writeln!(out, "  {}", command::DEFAULT_COMMAND);
            describe(&mut out, command::DEFAULT_COMMAND);
            let _ = writeln!(out);
            match path {
                Some(path) => {
                    let _ = writeln!(
                        out,
                        "`hog config command set \"<template>\"` puts your own command in\n{}.",
                        path.display()
                    );
                }
                // No $HOME: there is no file to name, and none will be created.
                None => {
                    let _ = writeln!(
                        out,
                        "$HOME is not set, so hog has no default config path — name one\n\
                         yourself with `hog --config PATH`."
                    );
                }
            }
        }

        Configured::Unreadable { why } => {
            let _ = writeln!(out, "Configured command (unavailable):");
            let _ = writeln!(out, "  {}", one_line(&why));
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "A run would fail the same way. `hog config path` says which file hog reads."
            );
        }
    }

    out
}

/// The arity and example half of the block, for an already-printed template.
///
/// A template that does not parse gets the fault instead: the arity of a
/// template hog cannot split is not a question with an answer, and the user
/// needs to know the config is broken *now* rather than at the next run.
fn describe(out: &mut String, text: &str) {
    let template = match command::template::parse(text) {
        Ok(template) => template,
        Err(err) => {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "This template does not parse: {}.",
                one_line(&err.to_string())
            );
            let _ = writeln!(out, "Fix it with `hog config edit`.");
            return;
        }
    };

    let _ = writeln!(out);
    let _ = writeln!(out, "{}", arity_line(&template));
    let _ = writeln!(out, "  hog{}", usage_tail(&template));

    if let Some(example) = example(&template) {
        let _ = writeln!(out);
        let _ = writeln!(out, "Example:");
        let _ = writeln!(out, "  hog {example}");
    }
}

/// `Takes 2 arguments:` and its four siblings.
fn arity_line(template: &Template) -> String {
    let needed = template.required_arity();
    let word = if needed == 1 { "argument" } else { "arguments" };

    match (template.is_variadic(), needed) {
        (true, 0) => "Takes any number of arguments:".to_owned(),
        (true, _) => format!("Takes at least {needed} {word}:"),
        (false, 0) => "Takes no arguments:".to_owned(),
        (false, _) => format!("Takes {needed} {word}:"),
    }
}

/// ` <ARG0> <ARG1>`, ` <ARG0> [ARGS]...`, ` [ARGS]...`, or nothing at all.
///
/// The names are deliberately positional and meaningless. HLD §6 rejected an
/// `args = ["host", "service"]` key for exactly that reason: names in the config
/// could disagree with the template beside them, and the template is printed two
/// lines up anyway.
fn usage_tail(template: &Template) -> String {
    let mut tail = String::new();
    for index in 0..template.required_arity() {
        let _ = write!(tail, " <ARG{index}>");
    }
    if template.is_variadic() {
        tail.push_str(" [ARGS]...");
    }
    tail
}

/// A runnable example invocation, or `None` for a template that takes nothing.
///
/// A zero-argument template has no example worth printing: `hog` on its own is
/// already the usage line one row above.
fn example(template: &Template) -> Option<String> {
    let count = template.required_arity()
        + if template.is_variadic() {
            TAIL_EXAMPLE_WORDS
        } else {
            0
        };
    if count == 0 {
        return None;
    }

    Some((0..count).map(example_word).collect::<Vec<_>>().join(" "))
}

/// The nth example word: three readable ones, then `arg3`, `arg4`, …
fn example_word(index: usize) -> Cow<'static, str> {
    EXAMPLE_WORDS.get(index).map_or_else(
        || Cow::Owned(format!("arg{index}")),
        |word| Cow::Borrowed(*word),
    )
}

/// Flattens anything that reaches the help text onto one line, dropping control
/// characters.
///
/// The template and the load error both come from a file on disk, and `--help`
/// prints them to a terminal. A newline would break the block's layout; an ESC
/// would repaint the terminal of whoever asked for help. Everything else is
/// passed through unchanged — this is a config file's own text, and mangling it
/// would make it harder to recognise.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|char| if char.is_control() { ' ' } else { char })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;

    /// An [`Env`] whose `$HOME` is `dir`, so a test gets its own file.
    fn env_at(dir: &Path) -> Env {
        Env {
            hog_config: None,
            home: Some(OsString::from(dir)),
        }
    }

    /// The default config file inside a test's own `$HOME`.
    fn config_in(dir: &Path) -> PathBuf {
        dir.join(config::discover::CONFIG_FILE)
    }

    fn argv(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    // ============================================== the four --config branches

    /// `--config` on a file that exists: the block describes *that* file, and
    /// this is the branch the `disable_help_flag` trap silently breaks.
    #[test]
    fn an_explicit_config_is_found_even_with_help_on_the_command_line() {
        let dir = tempdir();
        let named = dir.join("other.toml");
        std::fs::write(&named, "command = \"ssh {0} 'logs {1}'\"\n").expect("write");

        let found = preflight_config(
            &argv(&["hog", "--config", &named.display().to_string(), "--help"]),
            &env_at(&dir),
        );
        assert_eq!(found.as_deref(), Some(named.as_path()));

        let text = block(found.as_deref(), &env_at(&dir));
        assert!(text.contains("ssh {0} 'logs {1}'"), "{text}");
        assert!(text.contains("Takes 2 arguments:"), "{text}");
        assert!(text.contains("hog <ARG0> <ARG1>"), "{text}");
        assert!(text.contains("hog prod api"), "{text}");
    }

    /// The same trap reached from the other end of the command line.
    ///
    /// `disable_help_flag(true)` makes `--help` **unknown** rather than
    /// harmless, and `ignore_errors(true)` keeps only what it matched *before*
    /// the argument it choked on. So with the flags written in this order the
    /// path was silently dropped and the block described the default config —
    /// the same wrong-but-plausible output, one word order away. Deleting the
    /// inert `-h/--help` from [`preflight_config`] fails every row here.
    #[test]
    fn an_explicit_config_survives_a_help_flag_written_before_it() {
        let dir = tempdir();
        let named = dir.join("other.toml");
        std::fs::write(&named, "command = \"ssh {0} 'logs {1}'\"\n").expect("write");
        let named_text = named.display().to_string();

        for words in [
            vec!["hog", "--help", "--config", &named_text],
            vec!["hog", "-h", "--config", &named_text],
            vec!["hog", "--config", &named_text, "-h"],
        ] {
            let line = words.join(" ");
            let found = preflight_config(&argv(&words), &env_at(&dir));
            assert_eq!(found.as_deref(), Some(named.as_path()), "{line}");
        }
    }

    /// The same, with `--version` in place of `--help`: the hand-declared
    /// version argument is a parse error too, and it must not eat the path
    /// either (the half of the trap HLD §6's snippet leaves open).
    #[test]
    fn an_explicit_config_survives_the_version_flag_too() {
        let dir = tempdir();
        let named = dir.join("other.toml");
        std::fs::write(&named, "command = \"true\"\n").expect("write");

        let found = preflight_config(
            &argv(&["hog", "--config", &named.display().to_string(), "--version"]),
            &env_at(&dir),
        );
        assert_eq!(found.as_deref(), Some(named.as_path()));
    }

    /// `--config` on a file that is not there: the help still prints, and it
    /// says why the template is missing rather than showing somebody else's.
    #[test]
    fn a_missing_explicit_config_is_reported_in_the_block() {
        let dir = tempdir();
        std::fs::write(config_in(&dir), "").ok();
        let missing = dir.join("nope.toml");

        let text = block(Some(&missing), &env_at(&dir));
        assert!(
            text.starts_with("Configured command (unavailable):"),
            "{text}"
        );
        assert!(text.contains("nope.toml"), "{text}");
        assert!(text.contains("hog config path"), "{text}");
        assert!(
            !text.contains("Takes"),
            "no arity for a config we cannot read: {text}"
        );
    }

    /// `$HOG_CONFIG` with no `--config`: clap folds the two into one argument,
    /// and so must the loose pass.
    #[test]
    fn hog_config_from_the_environment_is_used_when_the_flag_is_absent() {
        let dir = tempdir();
        let named = dir.join("from-env.toml");
        std::fs::write(&named, "command = \"kubectl logs -f -l app=api\"\n").expect("write");

        let env = Env {
            hog_config: Some(OsString::from(&named)),
            ..env_at(&dir)
        };

        let found = preflight_config(&argv(&["hog", "--help"]), &env);
        assert_eq!(found.as_deref(), Some(named.as_path()));

        let text = block(found.as_deref(), &env);
        assert!(text.contains("kubectl logs -f -l app=api"), "{text}");
        assert!(text.contains("Takes no arguments:"), "{text}");
        // A template that takes nothing needs no example: `hog` is the usage
        // line already.
        assert!(!text.contains("Example:"), "{text}");
    }

    /// `--config` beats `$HOG_CONFIG`, exactly as the search order says.
    #[test]
    fn the_flag_wins_over_the_environment() {
        let dir = tempdir();
        let flag = dir.join("flag.toml");
        std::fs::write(&flag, "command = \"true\"\n").expect("write");

        let env = Env {
            hog_config: Some(OsString::from(dir.join("env.toml"))),
            ..env_at(&dir)
        };
        let found = preflight_config(
            &argv(&["hog", "--config", &flag.display().to_string()]),
            &env,
        );
        assert_eq!(found.as_deref(), Some(flag.as_path()));
    }

    /// No config at all: the built-in, marked as built in, with the hint that
    /// HLD §5 asks for by name — which is now `command set` and the path,
    /// since the file itself is no longer something the user has to ask for.
    #[test]
    fn with_no_config_the_block_names_the_built_in_and_the_file() {
        let dir = tempdir();
        let env = env_at(&dir);

        assert_eq!(preflight_config(&argv(&["hog", "--help"]), &env), None);

        let text = block(None, &env);
        assert!(text.starts_with("Configured command (built in"), "{text}");
        assert!(text.contains("echo {@}"), "{text}");
        assert!(text.contains("Takes any number of arguments:"), "{text}");
        assert!(text.contains("hog [ARGS]..."), "{text}");
        assert!(text.contains("hog prod api"), "{text}");
        assert!(text.contains("hog config command set"), "{text}");
        assert!(
            !text.contains("config init"),
            "the deleted verb is still advertised: {text}"
        );
        assert!(
            text.contains(&config_in(&dir).display().to_string()),
            "the hint names the file the template goes in: {text}"
        );
    }

    /// A config that exists but sets no `command` is still the built-in, and
    /// the hint names that very file rather than a guessed path.
    #[test]
    fn a_config_without_a_command_key_still_gets_the_built_in() {
        let dir = tempdir();
        let file = config_in(&dir);
        std::fs::write(&file, "exclude = [\"trace_id\"]\n").expect("write");

        let text = block(None, &env_at(&dir));
        assert!(text.contains("echo {@}"), "{text}");
        assert!(text.contains(&file.display().to_string()), "{text}");
    }

    /// Nowhere to put a config: there is no file to name, and hog will not
    /// create one either, so the flag is the only honest suggestion.
    #[test]
    fn with_nowhere_to_put_a_config_the_hint_names_the_flag_instead() {
        let text = block(None, &Env::default());
        assert!(text.contains("echo {@}"), "{text}");
        assert!(!text.contains("hog config command set"), "{text}");
        assert!(text.contains("$HOME is not set"), "{text}");
        assert!(text.contains("--config PATH"), "{text}");
    }

    // ====================================================== the block's shapes

    /// The flagship variadic template of HLD §5: a floor, not an exact count.
    /// Printing "Takes 1 argument" for it would be a lie the usage line then
    /// repeats.
    #[test]
    fn a_variadic_template_shows_a_floor_and_a_tail() {
        let dir = tempdir();
        let named = dir.join("v.toml");
        std::fs::write(&named, "command = \"ssh -tt {0} 'docker logs -f {@}'\"\n").expect("write");

        let text = block(Some(&named), &env_at(&dir));
        assert!(text.contains("Takes at least 1 argument:"), "{text}");
        assert!(text.contains("hog <ARG0> [ARGS]..."), "{text}");
        // Two words past the floor, so the tail reads as a tail.
        assert!(text.contains("hog prod api web"), "{text}");
    }

    /// A template hog cannot split has no arity to print, and the block says
    /// so instead of guessing.
    #[test]
    fn a_broken_template_is_named_as_broken() {
        let dir = tempdir();
        let named = dir.join("broken.toml");
        std::fs::write(&named, "command = \"ssh {0} 'docker logs\"\n").expect("write");

        let text = block(Some(&named), &env_at(&dir));
        assert!(text.contains("ssh {0} 'docker logs"), "{text}");
        assert!(text.contains("does not parse"), "{text}");
        assert!(text.contains("unclosed quote"), "{text}");
        assert!(!text.contains("Takes"), "{text}");
    }

    /// A config file is user input and `--help` goes to a terminal: a template
    /// carrying a newline or an ESC must not break the block or repaint the
    /// screen.
    #[test]
    fn a_control_byte_in_the_template_cannot_reach_the_terminal() {
        let dir = tempdir();
        let named = dir.join("esc.toml");
        std::fs::write(&named, "command = \"echo \\u001b[2J\\nnext {@}\"\n").expect("write");

        let text = block(Some(&named), &env_at(&dir));
        assert!(!text.contains('\u{1b}'), "raw ESC reached the help text");
        let template_line = text.lines().nth(1).expect("the template line");
        assert!(template_line.contains("next {@}"), "{template_line}");
    }

    /// Every shape of the arity line and the usage tail, in one place.
    #[test]
    fn the_arity_line_covers_every_shape() {
        for (template, arity, tail, example) in [
            ("kubectl logs -f", "Takes no arguments:", "", None),
            ("ssh {0}", "Takes 1 argument:", " <ARG0>", Some("prod")),
            (
                "ssh {0} {1}",
                "Takes 2 arguments:",
                " <ARG0> <ARG1>",
                Some("prod api"),
            ),
            (
                "echo {@}",
                "Takes any number of arguments:",
                " [ARGS]...",
                Some("prod api"),
            ),
            (
                "ssh {0} {1} {2} {@}",
                "Takes at least 3 arguments:",
                " <ARG0> <ARG1> <ARG2> [ARGS]...",
                Some("prod api web arg3 arg4"),
            ),
        ] {
            let parsed = command::template::parse(template).expect("must parse");
            assert_eq!(arity_line(&parsed), arity, "{template}");
            assert_eq!(usage_tail(&parsed), tail, "{template}");
            assert_eq!(super::example(&parsed).as_deref(), example, "{template}");
        }
    }

    /// The block is what `--help` actually ends with, not just a string this
    /// module can build.
    #[test]
    fn the_block_is_attached_to_the_rendered_help() {
        let dir = tempdir();
        let named = dir.join("attached.toml");
        std::fs::write(&named, "command = \"ssh {0} 'logs {1}'\"\n").expect("write");

        let rendered = command_for(
            &argv(&["hog", "--config", &named.display().to_string(), "--help"]),
            &env_at(&dir),
        )
        .render_help()
        .to_string();

        // Not an exact-layout assertion: clap re-wraps `after_help` to the
        // terminal width, so a long enough path splits across lines. The
        // template line is what has to survive intact, and it does.
        assert!(rendered.contains("Configured command"), "{rendered}");
        assert!(rendered.contains("ssh {0} 'logs {1}'"), "{rendered}");
        assert!(rendered.contains("Takes 2 arguments:"), "{rendered}");
        // The standard sections are still there, above it.
        assert!(rendered.contains("Usage:"), "{rendered}");
        assert!(rendered.contains("--exclude"), "{rendered}");
    }

    /// A directory of our own, created without a crate for it: `tempfile` is
    /// not a dependency, and one test module is not a reason to add one.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let dir = std::env::temp_dir().join(format!(
            "hog-help-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the temp dir must be creatable");
        dir
    }
}
