//! The command-line surface: the argv grammar, the exit-code table, the
//! terminal gate, `--help` / `--version`, and the two colour environment
//! variables.
//!
//! Two layers, on purpose:
//!
//! * **grammar** — `Cli::try_parse_from` in-process, because the HLD §6 table
//!   says what each invocation *parses to*, and several rows are
//!   indistinguishable once the process runs: with no config they all reach the
//!   built-in
//!   `echo {@}` and print their arguments, so `args=["prod","api"]
//!   exclude=["x"]` is only observable from the parsed struct.
//! * **behaviour** — the real binary through `assert_cmd`, because exit codes,
//!   the stdout byte stream and the terminal gate only exist once the process
//!   is assembled. `invalid_utf8_survives_the_real_stdout` is not hypothetical:
//!   the write path used to run every byte through `anstream`'s stripping
//!   stream, which silently swallowed non-UTF-8 bytes while every unit test in
//!   `render` passed.
//!
//! The timestamp column is pinned with `--timezone utc` wherever it shows: the
//! default is the machine's local zone (HLD §10.3), so an expectation written
//! any other way would only pass in one place. For the same reason `hog()`
//! strips `NO_COLOR`, `CLICOLOR_FORCE` and `COLORTERM` from the child's
//! environment — otherwise the suite would pass or fail depending on which
//! terminal the developer happened to run it from.

use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt as _;
use clap::Parser as _;

use hog::Cli;
use hog::cli::{Cmd, CommandOp, ConfigCmd, ExcludeOp};

/// The reference line from HLD §1, and the one the README quotes.
const REFERENCE: &str = concat!(
    r#"{"ts":"2025-06-15T10:32:01Z","level":"info","msg":"server started","#,
    r#""port":8080,"grpc":{"code":"OK","time_ms":1.5}}"#,
);

/// An absolute config home with no `hog/config.toml` under it.
///
/// Nothing is created: discovery only computes the path, and a missing file at
/// a *guessed* path is the ordinary "no config yet" case (HLD §3). Pointing the
/// child at one is what keeps this suite from reading — or worse, editing — the
/// developer's own `~/.config/hog/config.toml`.
fn no_config_home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hog-cli-surface-no-config-{}", std::process::id()))
}

/// Strips everything from the child's environment that would make the suite
/// pass or fail depending on the machine it runs on.
fn isolate(command: &mut Command) -> &mut Command {
    // The colour trio: the first two decide whether there is colour at all,
    // the third decides whether it is 24-bit or the ansi256 downgrade.
    command
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CLICOLOR")
        .env_remove("COLORTERM")
        // And the config trio, so that every test below starts from the
        // built-in defaults. `HOME` is set too: it is the fallback discovery
        // uses when `XDG_CONFIG_HOME` is unusable.
        .env_remove("HOG_CONFIG")
        .env("XDG_CONFIG_HOME", no_config_home())
        .env("HOME", no_config_home())
}

fn hog() -> Command {
    let mut command = Command::cargo_bin("hog").expect("the binary is built by `cargo test`");
    isolate(&mut command);
    command
}

/// Runs `hog <args>` over `input`, returning (stdout, stderr, exit code).
fn run(args: &[&str], input: &[u8]) -> (Vec<u8>, String, i32) {
    run_with_env(args, &[], input)
}

/// The same, with extra environment variables set for the child.
fn run_with_env(args: &[&str], env: &[(&str, &str)], input: &[u8]) -> (Vec<u8>, String, i32) {
    let mut command = hog();
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hog must start");

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input)
        .expect("hog must accept the input");

    let out = child.wait_with_output().expect("hog must finish");
    (
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().expect("hog must not die from a signal"),
    )
}

/// The same, for the common case of UTF-8 in and UTF-8 out.
fn render(args: &[&str], input: &str) -> String {
    let (stdout, stderr, code) = run(args, input.as_bytes());
    assert_eq!(code, 0, "stderr: {stderr}");
    String::from_utf8(stdout).expect("output stays UTF-8")
}

fn plain(input: &str) -> String {
    render(&["--color", "never", "--timezone", "utc"], input)
}

/// Runs the binary with a **pseudo-terminal** on stdin and stdout, which is the
/// only way to exercise the gate from HLD §6: "stdin is a tty and no `-r`".
///
/// `script(1)` is the portable-enough way to get a pty without a crate and
/// without `unsafe` (the package denies it). The two dialects differ: BSD takes
/// the command as trailing argv, util-linux needs `-c` plus `-e` to propagate
/// the child's status. Returns `None` when `script` cannot be spawned at all,
/// so the suite still runs somewhere exotic instead of failing for the wrong
/// reason; the caller says so out loud.
fn run_on_a_pty(args: &[&str]) -> Option<(String, i32)> {
    let binary = Command::cargo_bin("hog").expect("the binary is built by `cargo test`");
    let binary = binary.get_program().to_string_lossy().into_owned();

    let mut command = Command::new("script");
    isolate(&mut command);

    if cfg!(target_os = "macos") {
        // BSD: script [-q] [file [command ...]]; exits with the child's status.
        command.arg("-q").arg("/dev/null").arg(&binary).args(args);
    } else {
        // util-linux: the command is one shell word, and -e is what makes the
        // exit status the child's rather than script's own.
        let mut line = shell_quote(&binary);
        for arg in args {
            line.push(' ');
            line.push_str(&shell_quote(arg));
        }
        command.arg("-qe").arg("-c").arg(line).arg("/dev/null");
    }

    let out = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    // A pty echoes, adds ONLCR and prints the EOF that closing stdin causes, so
    // the text is only ever matched with `contains`.
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some((text, out.status.code()?))
}

/// Single-quotes one argument for the util-linux `script -c` form.
fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

// ===================================================================== grammar

/// The verified parse table in HLD §6 — all twelve rows it lists, one test
/// each, in the order of the table.
///
/// They are checked against the parser rather than the process because that is
/// what the table states: with no config every row carrying positional
/// arguments reaches the same built-in `echo {@}`, so at process level those
/// rows would be indistinguishable and the interesting part — the positionals
/// survived `-e`, the excludes landed — would go untested.
mod grammar {
    use super::*;

    fn parse(argv: &[&str]) -> Cli {
        Cli::try_parse_from(argv)
            .unwrap_or_else(|err| panic!("`{}` must parse:\n{err}", argv.join(" ")))
    }

    fn rejected(argv: &[&str]) -> clap::Error {
        Cli::try_parse_from(argv)
            .err()
            .unwrap_or_else(|| panic!("`{}` must be rejected", argv.join(" ")))
    }

    /// A comparable shadow of [`ConfigCmd`], because the real enum is a tree of
    /// three `Subcommand`s that would need `PartialEq` on all of them just to
    /// be asserted against. Flattening it here keeps the derive off the
    /// production type and makes a failing row readable.
    #[derive(Debug, PartialEq, Eq)]
    enum Action {
        Summary,
        Path,
        Init,
        Edit,
        /// `None` for the bare verb, `Some(fields)` for `add` / `rm`.
        Exclude(Option<(bool, Vec<String>)>),
        /// `None` for the bare verb, `Some(template)` for `set`.
        Command(Option<String>),
    }

    fn add(fields: &[&str]) -> Action {
        Action::Exclude(Some((true, owned(fields))))
    }

    fn rm(fields: &[&str]) -> Action {
        Action::Exclude(Some((false, owned(fields))))
    }

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// The parsed `config` verb, or a panic naming what was parsed instead.
    fn config_action(cli: Cli) -> Action {
        let Some(Cmd::Config { action }) = cli.command else {
            panic!("expected the `config` subcommand, got {:?}", cli.command);
        };
        match action {
            None => Action::Summary,
            Some(ConfigCmd::Path) => Action::Path,
            Some(ConfigCmd::Init) => Action::Init,
            Some(ConfigCmd::Edit) => Action::Edit,
            Some(ConfigCmd::Exclude { op }) => Action::Exclude(op.map(|op| match op {
                ExcludeOp::Add { fields } => (true, fields),
                ExcludeOp::Rm { fields } => (false, fields),
            })),
            Some(ConfigCmd::Command { op }) => {
                Action::Command(op.map(|CommandOp::Set { template }| template))
            }
        }
    }

    /// Row 1: `hog` — stdin mode, everything empty.
    #[test]
    fn bare_invocation_is_the_stdin_mode() {
        let cli = parse(&["hog"]);
        assert!(cli.command.is_none());
        assert!(cli.run.args.is_empty(), "no positionals means stdin mode");
        assert!(cli.run.exclude.is_empty());
        assert!(!cli.run.reset_exclude);
        assert!(!cli.run.dry_run);
        assert!(cli.run.ts_field.is_none());
        assert!(cli.run.ts_format.is_none());
        assert!(cli.run.level_field.is_none());
        assert!(cli.run.msg_field.is_none());
        assert!(cli.run.timezone.is_none());
    }

    /// Row 2: `hog prod api` — positionals alone select the command mode.
    /// There is no `-r/--ssh` any more (HLD §11.1).
    #[test]
    fn positionals_select_the_command_mode() {
        let cli = parse(&["hog", "prod", "api"]);
        assert_eq!(cli.run.args, ["prod", "api"]);
        assert!(cli.command.is_none());
    }

    /// Row 3: `hog prod api 30m` — arity is the template's business, not the
    /// parser's, so a third argument is simply a third argument.
    #[test]
    fn any_number_of_positionals_is_accepted() {
        assert_eq!(
            parse(&["hog", "prod", "api", "30m"]).run.args,
            ["prod", "api", "30m"]
        );
    }

    /// Row 4: `hog -e a,b` and `hog -e a -e b` are the same thing.
    #[test]
    fn exclude_is_repeatable_and_comma_separated() {
        for argv in [
            ["hog", "-e", "a,b"].as_slice(),
            ["hog", "-e", "a", "-e", "b"].as_slice(),
            ["hog", "--exclude", "a,b"].as_slice(),
        ] {
            let cli = parse(argv);
            assert_eq!(cli.run.exclude, ["a", "b"], "argv: {argv:?}");
            assert!(cli.run.args.is_empty(), "argv: {argv:?}");
        }
    }

    /// Row 5: `hog -e a,b prod api` — the row that `num_args = 1..` on `-e`
    /// would break by swallowing the positionals.
    #[test]
    fn exclude_does_not_swallow_the_positionals() {
        let cli = parse(&["hog", "-e", "a,b", "prod", "api"]);
        assert_eq!(
            cli.run.args,
            ["prod", "api"],
            "the positionals were eaten by -e"
        );
        assert_eq!(cli.run.exclude, ["a", "b"]);
    }

    /// Row 6: `hog prod api -e x` — a flag after the positionals still works.
    #[test]
    fn a_flag_may_follow_the_positionals() {
        let cli = parse(&["hog", "prod", "api", "-e", "x"]);
        assert_eq!(cli.run.args, ["prod", "api"]);
        assert_eq!(cli.run.exclude, ["x"]);
    }

    /// Row 7: `hog -E -e foo prod api` — all three at once. `-E` deliberately
    /// carries **no** `conflicts_with = "exclude"`: this pair is the documented
    /// way to replace the config's list outright (HLD §10.4).
    #[test]
    fn reset_and_exclude_are_compatible_and_keep_the_positionals() {
        let cli = parse(&["hog", "-E", "-e", "foo", "prod", "api"]);
        assert!(cli.run.reset_exclude);
        assert_eq!(cli.run.exclude, ["foo"]);
        assert_eq!(cli.run.args, ["prod", "api"]);
    }

    /// `-E` on its own: reset on, nothing excluded.
    #[test]
    fn reset_alone_sets_the_flag_and_nothing_else() {
        for argv in [
            ["hog", "-E"].as_slice(),
            ["hog", "--reset-exclude"].as_slice(),
        ] {
            let cli = parse(argv);
            assert!(cli.run.reset_exclude, "argv: {argv:?}");
            assert!(cli.run.exclude.is_empty(), "argv: {argv:?}");
        }
    }

    /// Row 8: `hog --dry-run prod api`.
    #[test]
    fn dry_run_keeps_the_positionals() {
        let cli = parse(&["hog", "--dry-run", "prod", "api"]);
        assert!(cli.run.dry_run);
        assert_eq!(cli.run.args, ["prod", "api"]);
    }

    /// Rows 9 and 10: `hog config` and `hog config exclude add grpc.code` are
    /// the subcommand, and nothing of theirs may land in `run` — `config` must
    /// not become a positional argument, and the fields must not become `-e`.
    #[test]
    fn config_is_a_subcommand() {
        let cli = parse(&["hog", "config"]);
        assert!(matches!(cli.command, Some(Cmd::Config { action: None })));
        assert!(cli.run.args.is_empty());

        let cli = parse(&["hog", "config", "exclude", "add", "grpc.code"]);
        assert!(cli.run.exclude.is_empty(), "the fields belong to the verb");
        assert!(cli.run.args.is_empty(), "`config` is not a positional here");
        assert_eq!(config_action(cli), add(&["grpc.code"]));
    }

    /// The verbs of HLD §6, one parse each. This is the table the wave
    /// replaced the v0.2 flags with, so it is checked value by value.
    #[test]
    fn every_config_verb_parses_to_itself() {
        for (argv, expected) in [
            (["hog", "config", "path"].as_slice(), Action::Path),
            (["hog", "config", "init"].as_slice(), Action::Init),
            (["hog", "config", "edit"].as_slice(), Action::Edit),
            (
                ["hog", "config", "exclude"].as_slice(),
                Action::Exclude(None),
            ),
            (
                ["hog", "config", "exclude", "add", "a,b", "c"].as_slice(),
                add(&["a", "b", "c"]),
            ),
            (
                ["hog", "config", "exclude", "rm", "a,b"].as_slice(),
                rm(&["a", "b"]),
            ),
            (
                ["hog", "config", "command"].as_slice(),
                Action::Command(None),
            ),
            (
                ["hog", "config", "command", "set", "ssh {0}"].as_slice(),
                Action::Command(Some("ssh {0}".to_owned())),
            ),
        ] {
            assert_eq!(config_action(parse(argv)), expected, "argv: {argv:?}");
        }
    }

    /// The v0.2 flags are **gone**, not deprecated: each of them is now a usage
    /// error rather than a silently different action (HLD §6, §11.8).
    #[test]
    fn the_old_config_flags_are_rejected() {
        for argv in [
            ["hog", "config", "--path"].as_slice(),
            ["hog", "config", "--init"].as_slice(),
            ["hog", "config", "-e", "trace_id"].as_slice(),
            ["hog", "config", "-d", "trace_id"].as_slice(),
            ["hog", "config", "-c", "ssh {0}"].as_slice(),
        ] {
            let err = rejected(argv);
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "argv: {argv:?}"
            );
        }
    }

    /// `add` and `rm` take at least one field, so a bare verb is a usage error
    /// rather than a write of nothing.
    #[test]
    fn an_exclude_verb_without_fields_is_a_usage_error() {
        for argv in [
            ["hog", "config", "exclude", "add"].as_slice(),
            ["hog", "config", "exclude", "rm"].as_slice(),
            ["hog", "config", "command", "set"].as_slice(),
        ] {
            let err = rejected(argv);
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "argv: {argv:?}"
            );
        }
    }

    /// A template is a command line: one that starts with a dash is the
    /// template, not a hog flag (`allow_hyphen_values`).
    #[test]
    fn a_template_may_start_with_a_dash() {
        assert_eq!(
            config_action(parse(&["hog", "config", "command", "set", "--follow {0}"])),
            Action::Command(Some("--follow {0}".to_owned()))
        );
    }

    /// Rows 11 and 12: `hog -- config` and `hog -- config api`. The subcommand
    /// wins over the first positional, so an argument spelled like one is
    /// passed after `--`. This is the single ambiguity in the grammar, and
    /// `--` is how HLD §6 resolves it.
    #[test]
    fn a_double_dash_passes_an_argument_named_like_the_subcommand() {
        let cli = parse(&["hog", "--", "config"]);
        assert!(cli.command.is_none(), "this is not the subcommand");
        assert_eq!(cli.run.args, ["config"]);

        let cli = parse(&["hog", "--", "config", "api"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.run.args, ["config", "api"]);
    }

    /// Past the table: the subcommand wins over a first positional spelled
    /// `config` **whatever came before it**, and `--` is the one way to say the
    /// other thing.
    ///
    /// This is the row that changed when `args_conflicts_with_subcommands` was
    /// dropped: it used to parse as `ARGS=["config"]`. The rule the HLD states
    /// ("the subcommand wins over the first positional") now holds everywhere
    /// instead of only when no flag was typed first.
    #[test]
    fn the_subcommand_wins_even_after_a_run_flag() {
        let cli = parse(&["hog", "-e", "x", "config"]);
        assert!(cli.command.is_some(), "the subcommand lost to a positional");

        let cli = parse(&["hog", "-e", "x", "--", "config"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.run.args, ["config"]);
        assert_eq!(cli.run.exclude, ["x"]);
    }

    /// The regression that `args_conflicts_with_subcommands` caused, measured
    /// on the built binary and the reason HLD §6's grammar block is wrong:
    /// `--config` is `global = true`, so with that setting clap counted it as
    /// "the parent's args were given" and stopped looking for a subcommand.
    /// `hog --config x config path` failed with "unexpected argument 'path'",
    /// and `hog --config x config` silently ran the command mode with
    /// `ARGS=["config"]` — a wrong answer with no diagnostic at all.
    #[test]
    fn a_global_flag_before_the_subcommand_keeps_the_subcommand() {
        for argv in [
            ["hog", "--config", "/tmp/hog.toml", "config"].as_slice(),
            ["hog", "--config", "/tmp/hog.toml", "config", "path"].as_slice(),
            [
                "hog",
                "--config",
                "/tmp/hog.toml",
                "config",
                "exclude",
                "add",
                "a",
            ]
            .as_slice(),
        ] {
            let cli = parse(argv);
            assert_eq!(
                cli.config.as_deref(),
                Some(std::path::Path::new("/tmp/hog.toml")),
                "argv: {argv:?}"
            );
            assert!(
                matches!(cli.command, Some(Cmd::Config { .. })),
                "argv: {argv:?} parsed as the command mode instead"
            );
            assert!(cli.run.args.is_empty(), "argv: {argv:?}");
        }
    }

    /// Past the table, but the same layer: a value `--color` does not know is a
    /// usage error (exit 2), never a runtime one.
    #[test]
    fn an_unknown_color_value_is_rejected() {
        let err = rejected(&["hog", "--color", "bogus"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
        assert!(err.to_string().contains("auto"), "{err}");
    }

    /// `--config` is `global = true`, so it is accepted on either side.
    #[test]
    fn config_path_is_global() {
        for argv in [
            ["hog", "--config", "/tmp/hog.toml"].as_slice(),
            ["hog", "config", "--config", "/tmp/hog.toml"].as_slice(),
        ] {
            let cli = parse(argv);
            assert_eq!(
                cli.config.as_deref(),
                Some(std::path::Path::new("/tmp/hog.toml")),
                "argv: {argv:?}"
            );
        }
    }
}

// ================================================================= exit codes

/// The table in HLD §6. 127 (the command not found in `PATH`) and a propagated
/// command exit code belong to `tests/cmd_fake.rs`, which has a stub script to
/// produce them; everything here runs with no config, so the only thing that
/// ever spawns is the built-in `echo {@}`, and it succeeds.
mod exit_codes {
    use super::*;

    /// 0 — the input stream ended.
    #[test]
    fn success_is_zero() {
        let (_, stderr, code) = run(&["--color", "never"], b"{\"a\":1}\n");
        assert_eq!((code, stderr.as_str()), (0, ""));
    }

    /// 0 — including when there was nothing to read at all.
    #[test]
    fn an_empty_stream_succeeds_silently() {
        let (stdout, stderr, code) = run(&[], b"");
        assert_eq!(
            (code, stdout.as_slice(), stderr.as_str()),
            (0, &b""[..], "")
        );
    }

    /// 1 — a runtime error. Every one of these prints `error: …` on stderr and
    /// nothing at all on stdout.
    #[test]
    fn runtime_errors_are_one() {
        for (args, needle) in [
            (vec!["--ts-format", "HH:MM"], "invalid time format"),
            (vec!["--timezone", "Nowhere/Land"], "unknown time zone"),
            // A hostile argument is refused before anything is assembled, even
            // by the built-in `echo {@}` (HLD §5).
            (vec!["--", "api; rm -rf /"], "are not allowed"),
            // `completions bash` used to belong here, as the placeholder that
            // refused until v1.0. It is implemented now and exits 0; see
            // `completions::*` below.
        ] {
            let (stdout, stderr, code) = run(&args, REFERENCE.as_bytes());
            assert_eq!(code, 1, "args {args:?}, stderr: {stderr}");
            assert!(stdout.is_empty(), "args {args:?} rendered something");
            assert!(stderr.starts_with("error: "), "args {args:?}: {stderr}");
            assert!(stderr.contains(needle), "args {args:?}: {stderr}");
        }
    }

    /// A typo in `--ts-format` must fail once, at start-up, rather than
    /// printing itself once per line for the rest of the stream.
    #[test]
    fn a_bad_time_format_fails_before_the_first_line() {
        let (stdout, stderr, code) = run(&["--ts-format", "HH:MM"], REFERENCE.as_bytes());
        assert_eq!(code, 1);
        assert!(stdout.is_empty(), "nothing should have been rendered");
        assert!(stderr.contains("invalid time format"), "stderr: {stderr}");
    }

    /// 2 — usage. clap prints these and exits on its own; the point of the
    /// assertion is that nothing downstream re-maps them to 1.
    #[test]
    fn usage_errors_are_two() {
        for args in [
            vec!["--color", "bogus"],
            vec!["--nope"],
            vec!["-Z"],
            vec!["--exclude"],
            vec!["config", "--nope"],
            vec!["completions", "not-a-shell"],
        ] {
            let (stdout, stderr, code) = run(&args, b"");
            assert_eq!(code, 2, "args {args:?}, stderr: {stderr}");
            assert!(stdout.is_empty(), "args {args:?} wrote to stdout");
            assert!(stderr.contains("error:"), "args {args:?}: {stderr}");
        }
    }

    /// 0, not 2: `-e a,b prod api` is a *valid* command line, and with no
    /// config it runs the built-in `echo {@}` (HLD §5). A 2 here would mean
    /// `-e` had swallowed the positionals after all, and a non-empty stderr
    /// would mean the excludes had been read as something other than excludes.
    #[test]
    fn a_complete_command_invocation_is_not_a_usage_error() {
        for args in [
            vec!["prod", "api"],
            vec!["-e", "a,b", "prod", "api"],
            vec!["-E", "-e", "x", "prod", "api"],
        ] {
            let (stdout, stderr, code) = run(&args, b"");
            assert_eq!(code, 0, "args {args:?}, stderr: {stderr}");
            assert_eq!(
                String::from_utf8_lossy(&stdout).trim_end(),
                "prod api",
                "args {args:?}"
            );
        }
    }

    /// The whole point of the default, from HLD §5: a freshly installed hog
    /// answers "where do my arguments go?" on the first run instead of
    /// refusing to do anything.
    ///
    /// `hog -- config` is in the list because it is the documented escape for
    /// an argument spelled like a subcommand, and it has to reach the same
    /// built-in rather than the `config` verb.
    #[test]
    fn the_built_in_echo_prints_the_arguments() {
        for (args, expected) in [
            (vec!["a", "b", "c"], "a b c"),
            (vec!["prod"], "prod"),
            (vec!["--", "config"], "config"),
        ] {
            let (stdout, stderr, code) = run(&args, b"");
            assert_eq!((code, stderr.as_str()), (0, ""), "args {args:?}");
            assert_eq!(
                String::from_utf8_lossy(&stdout).trim_end(),
                expected,
                "args {args:?}"
            );
        }
    }

    /// The built-in is a template like any other, so `--dry-run` shows it —
    /// which is also the only way to see it without running it.
    #[test]
    fn the_built_in_shows_up_in_a_dry_run() {
        let (stdout, stderr, code) = run(&["--dry-run", "prod", "api"], b"");
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert_eq!(String::from_utf8_lossy(&stdout), "echo\nprod\napi\n");
    }

    /// 141 — `hog … | head -3` must end quietly with the code a shell would
    /// have seen from a SIGPIPE death, and must not print a panic, which is
    /// what `println!` would have done.
    #[test]
    fn a_closed_stdout_exits_141_without_a_panic() {
        let mut child = hog()
            .args(["--color", "never"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("hog must start");

        let mut stdin = child.stdin.take().expect("stdin was piped");
        // A writer thread: hog blocks on a full stdout pipe long before this
        // finishes, which is exactly the state the test needs.
        let writer = std::thread::spawn(move || {
            for index in 0..200_000u32 {
                if writeln!(stdin, r#"{{"msg":"line","n":{index}}}"#).is_err() {
                    break;
                }
            }
        });

        // Read a little, then hang up — the downstream `head` going away.
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut head = [0u8; 64];
        let _ = stdout.read(&mut head);
        drop(stdout);

        let output = child.wait_with_output().expect("hog must finish");
        let _ = writer.join();

        assert_eq!(output.status.code(), Some(141));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.is_empty(), "a broken pipe must be quiet: {stderr}");
    }

    /// The same rule for `hog config`, which is the surface most likely to be
    /// piped: its own module docs advertise `hog config exclude | wc -l`, and
    /// `| head` on a long list is the same keystroke.
    ///
    /// These verbs used to print `error: Broken pipe (os error 32)` and exit 1
    /// — the one noisy corner of an exit-code table whose whole point is that a
    /// vanished reader is not an error worth a message.
    #[test]
    fn a_closed_stdout_exits_141_for_the_config_verbs_too() {
        let dir = std::env::temp_dir().join(format!("hog-cfg-pipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the fixture directory is created");
        let path = dir.join("config.toml");
        // Long enough that the write cannot all fit in the pipe buffer, which
        // is what makes the failing write happen at all.
        let fields: Vec<String> = (0..20_000).map(|n| format!("\"field_{n:05}\"")).collect();
        std::fs::write(&path, format!("exclude = [{}]\n", fields.join(", ")))
            .expect("the fixture config is written");

        // Only the verbs whose output cannot fit in a pipe buffer: a write that
        // never fails is not evidence of anything. `hog config command` prints
        // one short line, so it finishes before the reader is missed and exits
        // 0 — which is right, and is why it is not in this list.
        for verb in [vec!["config", "exclude"], vec!["config"]] {
            let mut child = hog()
                .arg("--config")
                .arg(&path)
                .args(&verb)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("hog must start");

            // Read one buffer's worth, then hang up: `| head -1`.
            let mut stdout = child.stdout.take().expect("stdout was piped");
            let mut head = [0u8; 64];
            let _ = stdout.read(&mut head);
            drop(stdout);

            let output = child.wait_with_output().expect("hog must finish");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(141), "{verb:?}: {stderr}");
            assert!(stderr.is_empty(), "{verb:?} must be quiet: {stderr}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}

// ============================================================== terminal gate

/// HLD §6, "Режимы ввода". The row that matters is the last one: with stdin on
/// a terminal, no positional arguments and no `command` template, a naive
/// implementation waits forever for the user to type JSON at it.
mod terminal_gate {
    use super::*;

    #[test]
    fn a_terminal_on_stdin_is_a_usage_error_not_a_wait() {
        let Some((text, code)) = run_on_a_pty(&["--color", "never"]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };

        assert_eq!(code, 2, "the gate is a usage error: {text}");
        assert!(text.contains("stdin is a terminal"), "got: {text}");
        // The usage hint is the whole point: the user gets told what to pipe.
        assert!(text.contains("Usage: hog"), "no usage hint: {text}");
    }

    /// The command mode ignores stdin entirely, so the gate must not fire in
    /// front of it — otherwise `hog prod api` from an interactive shell, the
    /// flagship use case, would be refused for the wrong reason.
    ///
    /// With no config that is the built-in `echo {@}`, so the run succeeds and
    /// prints the arguments: HLD §5's "первый запуск без конфига", on a real
    /// terminal.
    #[test]
    fn the_gate_does_not_fire_for_the_command_mode() {
        let Some((text, code)) = run_on_a_pty(&["prod", "api"]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };

        assert_eq!(code, 0, "got: {text}");
        assert!(text.contains("prod api"), "got: {text}");
        assert!(!text.contains("stdin is a terminal"), "got: {text}");
    }

    /// `hog config …` reads no input either: it answers from the file system
    /// and exits 0, rather than being refused for having a terminal on stdin.
    #[test]
    fn the_gate_does_not_fire_for_the_config_subcommand() {
        let Some((text, code)) = run_on_a_pty(&["config", "path"]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };

        assert_eq!(code, 0, "got: {text}");
        assert!(text.contains("config.toml"), "got: {text}");
        assert!(!text.contains("stdin is a terminal"), "got: {text}");
    }

    /// The other half of the gate, and the common case: a pipe or a file on
    /// stdin is read, not refused.
    #[test]
    fn a_pipe_on_stdin_is_read() {
        let (stdout, stderr, code) =
            run(&["--color", "never", "--timezone", "utc"], b"{\"a\":1}\n");
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert_eq!(stdout, b"a=1\n");
    }
}

// =========================================================== help and version

mod help_and_version {
    use super::*;

    /// `--version` prints the **release tag the binary was built from**, not
    /// `CARGO_PKG_VERSION` (HLD §6): `build.rs` resolves `$HOG_VERSION` →
    /// `git describe` → `dev`, and `hog::VERSION` is the answer it compiled in.
    ///
    /// The expectation is that constant rather than a literal: the string is
    /// machine- and checkout-dependent (`dev (a1b2c3d, dirty)` here, `v1.0.0`
    /// under CI), so a golden file would only pass in one place.
    ///
    /// All three spellings are checked. `-v` is the one HLD §6 asks for; `-V`
    /// is the alias, because it is clap's standard short form.
    #[test]
    fn version_prints_the_tag_the_binary_was_built_from() {
        for flag in ["--version", "-v", "-V"] {
            let (stdout, stderr, code) = run(&[flag], b"");
            let text = String::from_utf8(stdout).expect("version text is UTF-8");
            assert_eq!((code, stderr.as_str()), (0, ""), "flag {flag}");
            assert_eq!(
                text.trim_end(),
                format!("hog {}", hog::VERSION),
                "flag {flag}"
            );
        }
    }

    /// Whatever the version string is, it is not the one `CARGO_PKG_VERSION`
    /// would have given — that is the entire reason `build.rs` exists.
    #[test]
    fn the_version_is_not_the_cargo_manifest_version() {
        assert_ne!(hog::VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!hog::VERSION.is_empty());
        // One line, and no control characters: `build.rs` strips them so a
        // `$HOG_VERSION` carrying a newline cannot forge a second `cargo::`
        // directive, and so `--version` cannot repaint a terminal.
        assert!(
            !hog::VERSION.chars().any(char::is_control),
            "{:?}",
            hog::VERSION
        );
    }

    /// `--help` goes to **stdout** with exit 0 — it was asked for, so it is
    /// output, not a diagnostic. It has to name every flag the HLD §6 grammar
    /// declares, because that list is the CLI's contract.
    #[test]
    fn help_documents_the_whole_grammar() {
        let (stdout, stderr, code) = run(&["--help"], b"");
        let text = String::from_utf8(stdout).expect("help text is UTF-8");

        assert_eq!((code, stderr.as_str()), (0, ""));
        for needle in [
            "Usage: hog",
            "[ARGS]...",
            "-e, --exclude <FIELD>",
            "-E, --reset-exclude",
            "--color <COLOR>",
            "--dry-run",
            "--ts-field <FIELD>",
            "--ts-format <FMT>",
            "--level-field <FIELD>",
            "--msg-field <FIELD>",
            "--timezone <TZ>",
            "--config <PATH>",
            "-v, --version",
            "config",
        ] {
            assert!(text.contains(needle), "`--help` never mentions {needle:?}");
        }
        // `completions` is `hide = true` until v1.0.
        assert!(
            !text.contains("completions"),
            "the hidden subcommand leaked into --help"
        );
        // The flags HLD §11 removed must not come back.
        for gone in ["--ssh", "SERVICE", "--since", "--tail"] {
            assert!(
                !text.contains(gone),
                "`--help` still offers {gone:?}, which HLD §11 removed"
            );
        }
    }

    /// `-h` is the short form, and clap renders it shorter. Both exit 0.
    #[test]
    fn the_short_help_is_also_help() {
        let (stdout, _, code) = run(&["-h"], b"");
        let short = String::from_utf8(stdout).expect("help text is UTF-8");
        let (stdout, _, _) = run(&["--help"], b"");
        let long = String::from_utf8(stdout).expect("help text is UTF-8");

        assert_eq!(code, 0);
        assert!(short.contains("Usage: hog"));
        assert!(short.len() < long.len(), "-h should be the summary");
    }

    /// The `help` subcommand clap adds for free.
    #[test]
    fn the_help_subcommand_works_too() {
        let (stdout, _, code) = run(&["help"], b"");
        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).expect("help text is UTF-8");
        assert!(text.contains("Usage: hog"), "{text}");
    }
}

// =============================================================== completions

/// `hog completions <shell>` (HLD §4, `clap_complete`; HLD §9, v1.0).
///
/// Hidden from `--help` because it is run once at install time, which is also
/// why it needs a test of its own: nothing else in the surface would notice it
/// breaking.
mod completions {
    use super::*;

    /// Every shell clap_complete knows produces a non-empty script, on stdout,
    /// with exit 0 and a silent stderr — `> _hog` has to be a usable file.
    #[test]
    fn every_shell_writes_its_script_to_stdout() {
        for (shell, needle) in [
            ("bash", "_hog"),
            ("elvish", "hog"),
            ("fish", "complete"),
            ("powershell", "hog"),
            ("zsh", "#compdef hog"),
        ] {
            let (stdout, stderr, code) = run(&["completions", shell], b"");
            let script = String::from_utf8(stdout).expect("a completion script is UTF-8");
            assert_eq!(code, 0, "{shell}: {stderr}");
            assert!(stderr.is_empty(), "{shell} wrote to stderr: {stderr}");
            assert!(script.contains(needle), "{shell} script: {script}");
            // The script describes the real grammar, not a stub. Spelled
            // without the dashes: fish writes flags as `-l reset-exclude`.
            assert!(script.contains("reset-exclude"), "{shell} script: {script}");
        }
    }

    /// The script is built in memory before anything is written, because
    /// clap_complete's own writers `unwrap` their write errors: generating
    /// straight into a closed pipe would panic instead of exiting 141.
    #[test]
    fn a_closed_stdout_exits_141_without_a_panic() {
        let mut child = hog()
            .args(["completions", "bash"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("hog must start");

        drop(child.stdout.take().expect("stdout was piped"));
        let out = child.wait_with_output().expect("hog must finish");
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert_eq!(out.status.code(), Some(141), "stderr: {stderr}");
        assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    }
}

// ============================================================== dynamic help

/// HLD §6, "Динамический help": `--help` prints the user's own `command`
/// template, because `[ARGS]...` on its own says nothing about what to type.
///
/// The four branches below are the ones HLD §6 asks for by name, and the
/// reason it asks is a trap: the pre-pass that finds `--config` has to run with
/// the help *and* version flags disabled, or it fails before it reaches the
/// flag and the block silently describes a different file. That failure is
/// invisible from the inside — the help text still looks perfectly plausible —
/// so it can only be caught by comparing what `--help` said against a file the
/// test wrote itself.
mod dynamic_help {
    use super::*;

    /// Writes a config under a directory of this test's own, and returns both.
    fn config_with(name: &str, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("hog-dynamic-help-{}-{name}", std::process::id()));
        std::fs::create_dir_all(dir.join("hog")).expect("the setup mkdir succeeds");
        let file = dir.join("hog").join("config.toml");
        std::fs::write(&file, body).expect("the setup write succeeds");
        (dir, file)
    }

    fn help_text(args: &[&str], env: &[(&str, &str)]) -> String {
        let (stdout, stderr, code) = run_with_env(args, env, b"");
        assert_eq!((code, stderr.as_str()), (0, ""), "args {args:?}");
        String::from_utf8(stdout).expect("help text is UTF-8")
    }

    /// Branch 1 — `--config` on a file that exists. This is the branch the
    /// trap breaks: without `disable_help_flag`, `--help` ends the pre-pass
    /// before `--config` is seen and the block below would show `echo {@}`.
    #[test]
    fn an_explicit_config_on_the_command_line_is_the_one_described() {
        let (_, file) = config_with("explicit", "command = \"ssh {0} 'docker logs {1}'\"\n");

        let text = help_text(&["--config", &file.display().to_string(), "--help"], &[]);

        assert!(text.contains("ssh {0} 'docker logs {1}'"), "{text}");
        assert!(text.contains("Takes 2 arguments:"), "{text}");
        assert!(text.contains("hog <ARG0> <ARG1>"), "{text}");
        assert!(text.contains("hog prod api"), "{text}");
        assert!(
            !text.contains("echo {@}"),
            "the pre-pass lost --config and fell back to the built-in: {text}"
        );
    }

    /// Branch 2 — `--config` on a file that is not there. `--help` still
    /// prints, exit 0, and it names the file rather than quietly describing
    /// somebody else's config.
    #[test]
    fn a_missing_explicit_config_is_named_rather_than_replaced() {
        let missing = std::env::temp_dir().join("hog-dynamic-help-nowhere.toml");
        let _ = std::fs::remove_file(&missing);

        let text = help_text(&["--config", &missing.display().to_string(), "--help"], &[]);

        assert!(text.contains("Usage: hog"), "{text}");
        assert!(text.contains("hog-dynamic-help-nowhere.toml"), "{text}");
        assert!(
            !text.contains("echo {@}"),
            "a config we were told to read and could not is not the built-in: {text}"
        );
    }

    /// Branch 3 — `$HOG_CONFIG`. clap folds it into the same argument as
    /// `--config`, and the pre-pass has to resolve it the same way.
    #[test]
    fn hog_config_from_the_environment_reaches_the_block() {
        let (_, file) = config_with("from-env", "command = \"kubectl logs -f -l app={0}\"\n");

        let text = help_text(&["--help"], &[("HOG_CONFIG", &file.display().to_string())]);

        assert!(text.contains("kubectl logs -f -l app={0}"), "{text}");
        assert!(text.contains("Takes 1 argument:"), "{text}");
        assert!(text.contains("hog <ARG0>"), "{text}");
    }

    /// Branch 4 — no config at all: the built-in, marked as built in, with the
    /// `hog config init` hint HLD §5 asks for by name.
    #[test]
    fn with_no_config_the_block_is_the_built_in_plus_config_init() {
        let text = help_text(&["--help"], &[]);

        assert!(text.contains("Configured command"), "{text}");
        assert!(text.contains("built in"), "{text}");
        assert!(text.contains("echo {@}"), "{text}");
        assert!(text.contains("Takes any number of arguments:"), "{text}");
        assert!(text.contains("hog config init"), "{text}");
    }

    /// The block is not `--help`-only: the last row of the mode table prints
    /// the same text on **stderr** with exit 2, which is where a bare `hog` at
    /// a prompt with no config learns what to do next.
    #[test]
    fn the_usage_error_carries_the_same_block() {
        let Some((text, code)) = run_on_a_pty(&[]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };

        assert_eq!(code, 2, "got: {text}");
        assert!(text.contains("stdin is a terminal"), "got: {text}");
        assert!(text.contains("Configured command"), "got: {text}");
        assert!(text.contains("hog config init"), "got: {text}");
    }

    /// A template the config sets but hog cannot split: `--help` still prints,
    /// and says the template is broken instead of inventing an arity for it.
    #[test]
    fn a_broken_template_is_reported_in_the_block() {
        let (_, file) = config_with("broken", "command = \"ssh {0} 'docker logs\"\n");

        let text = help_text(&["--config", &file.display().to_string(), "--help"], &[]);

        assert!(text.contains("does not parse"), "{text}");
        assert!(text.contains("unclosed quote"), "{text}");
        assert!(!text.contains("Takes"), "{text}");
    }

    /// `--version` must not be collateral damage of the two-pass parse: it is
    /// the *other* flag implemented as a parse error, and the pre-pass has to
    /// step around it too.
    #[test]
    fn the_version_flag_still_works_alongside_an_explicit_config() {
        let (_, file) = config_with("version", "command = \"true\"\n");

        let (stdout, stderr, code) =
            run(&["--config", &file.display().to_string(), "--version"], b"");
        let text = String::from_utf8(stdout).expect("version text is UTF-8");
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert_eq!(text.trim_end(), format!("hog {}", hog::VERSION));
    }
}

// =============================================================== environment

/// `NO_COLOR` and `CLICOLOR_FORCE` from HLD §6. Both are resolved before any
/// tty test, so they are observable even with stdout on a pipe — which is
/// exactly what makes them testable here.
mod environment {
    use super::*;

    const LINE: &str = r#"{"level":"info","msg":"m","port":1}"#;
    const ESC: &str = "\u{1b}[";

    fn output(args: &[&str], env: &[(&str, &str)]) -> String {
        let (stdout, stderr, code) = run_with_env(args, env, LINE.as_bytes());
        assert_eq!(code, 0, "stderr: {stderr}");
        String::from_utf8(stdout).expect("output stays UTF-8")
    }

    /// The baseline this module compares against: a pipe is not a terminal, so
    /// `--color auto` produces no escapes at all.
    #[test]
    fn a_piped_stdout_gets_no_colour_by_default() {
        let text = output(&[], &[]);
        assert!(!text.contains(ESC), "unexpected colour: {text:?}");
    }

    /// `CLICOLOR_FORCE` is how a caller says "yes, I know it is a pipe, colour
    /// it anyway" — the `hog … | less -R` case.
    #[test]
    fn clicolor_force_colours_a_pipe() {
        for value in ["1", "yes", "true"] {
            let text = output(&[], &[("CLICOLOR_FORCE", value)]);
            assert!(
                text.contains(ESC),
                "CLICOLOR_FORCE={value} produced no colour: {text:?}"
            );
        }
    }

    /// `NO_COLOR` wins over `CLICOLOR_FORCE`. Not an accident of ordering: the
    /// no-color.org rule is that the variable is honoured whenever it is set.
    #[test]
    fn no_color_beats_clicolor_force() {
        let text = output(&[], &[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")]);
        assert!(!text.contains(ESC), "NO_COLOR was ignored: {text:?}");
    }

    /// Any non-empty value counts, `0` included — the spec is about the
    /// variable being present, not about what it says.
    #[test]
    fn any_non_empty_no_color_disables_colour() {
        for value in ["1", "0", "no"] {
            let text = output(&[], &[("NO_COLOR", value), ("CLICOLOR_FORCE", "1")]);
            assert!(
                !text.contains(ESC),
                "NO_COLOR={value} was ignored: {text:?}"
            );
        }
    }

    /// The other half of that spec: an **empty** `NO_COLOR` means "not set".
    /// Getting this backwards silently disables colour for everyone whose shell
    /// exports empty variables.
    #[test]
    fn an_empty_no_color_is_not_set() {
        let text = output(&[], &[("NO_COLOR", ""), ("CLICOLOR_FORCE", "1")]);
        assert!(
            text.contains(ESC),
            "empty NO_COLOR disabled colour: {text:?}"
        );
    }

    /// An explicit `--color` outranks the environment in both directions: the
    /// flag is a per-invocation decision by someone who can see the pipe, and
    /// `--color never` still has to win over `CLICOLOR_FORCE`.
    #[test]
    fn an_explicit_color_flag_outranks_the_environment() {
        let forced = output(&["--color", "always"], &[("NO_COLOR", "1")]);
        assert!(forced.contains(ESC), "--color always lost: {forced:?}");

        let suppressed = output(&["--color", "never"], &[("CLICOLOR_FORCE", "1")]);
        assert!(
            !suppressed.contains(ESC),
            "--color never lost: {suppressed:?}"
        );
    }

    /// `HOG_CONFIG` is wired through clap's `env`, so it fills in `--config`
    /// without a `std::env::var` call anywhere in the crate.
    ///
    /// Checked through the child process rather than `Cli::try_parse_from`,
    /// because the value has to be in *the parsing process's* environment and
    /// `std::env::set_var` is `unsafe` — which this package denies outright.
    /// clap prints the variable it read into the flag's help, so the wiring is
    /// visible from outside.
    #[test]
    fn hog_config_feeds_the_config_flag() {
        let path = "/nonexistent/hog.toml";

        let (stdout, _, code) = run_with_env(&["--help"], &[("HOG_CONFIG", path)], b"");
        let help = String::from_utf8(stdout).expect("help text is UTF-8");
        assert_eq!(code, 0);
        assert!(
            help.contains(&format!("[env: HOG_CONFIG={path}]")),
            "--config did not pick the variable up: {help}"
        );

        // And the variable is not decoration: a file the user named and that is
        // not there is an error, not a silent fall-through to the defaults
        // (HLD §3). The message has to name the knob, because `--config` and
        // `$HOG_CONFIG` arrive in the same clap field.
        let (stdout, stderr, code) = run_with_env(&[], &[("HOG_CONFIG", path)], b"");
        assert_eq!(code, 1, "stderr: {stderr}");
        assert!(stdout.is_empty(), "nothing should have been rendered");
        assert!(stderr.contains("$HOG_CONFIG"), "stderr: {stderr}");
        assert!(stderr.contains(path), "stderr: {stderr}");
    }
}

// ======================================================================= shape

/// A handful of end-to-end renders. `tests/render_golden.rs` owns the corpus;
/// these exist to prove the assembled process produces the documented line,
/// and that the pieces are wired to each other in the right order.
#[test]
fn the_reference_line() {
    assert_eq!(
        plain(REFERENCE),
        "10:32:01 [INF] server started grpc.code=OK grpc.time_ms=1.5 port=8080\n"
    );
}

/// hulog wrote a space after the timestamp and the level tag whether or not
/// anything followed, so this line came out as `10:32:01 [INF]  port=8080`.
#[test]
fn an_absent_part_takes_its_separator_with_it() {
    assert_eq!(
        plain(r#"{"ts":"2025-06-15T10:32:01Z","level":"info","port":8080}"#),
        "10:32:01 [INF] port=8080\n"
    );
}

#[test]
fn one_input_line_is_always_one_output_line() {
    let input = concat!(
        "{\"msg\":\"a\\nb\"}\n",
        "not json at all\n",
        "{}\n",
        "{\"a\":1}\n",
        "[1,2]\n",
    );
    assert_eq!(plain(input).lines().count(), 5);
}

// ---------------------------------------------------------------- invariants

/// HLD §6, invariant 1.
#[test]
fn the_tail_is_sorted_by_full_dotted_path() {
    assert_eq!(
        plain(r#"{"z":1,"grpc":{"time_ms":2,"code":3},"a":4}"#),
        "a=4 grpc.code=3 grpc.time_ms=2 z=1\n"
    );
}

/// HLD §6, invariant 2: the colour of a key depends on the key alone, so two
/// runs of the same input are byte-identical.
#[test]
fn colour_is_stable_between_runs() {
    // `COLORTERM` is what decides truecolor vs the 256-colour downgrade, and
    // the expectations below are the 24-bit sequences read off hulog.
    let run_once = || {
        let (stdout, stderr, code) = run_with_env(
            &["--color", "always", "--timezone", "utc"],
            &[("COLORTERM", "truecolor")],
            REFERENCE.as_bytes(),
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        String::from_utf8(stdout).expect("output stays UTF-8")
    };

    let first = run_once();
    assert_eq!(first, run_once(), "the same line must colour the same way");
    // Pinned against the real hulog binary's escape sequences: `port` is
    // palette entry 0, `grpc.code` entry 8 and `grpc.time_ms` entry 9.
    for expected in [
        "\u{1b}[38;2;255;51;102mport",
        "\u{1b}[38;2;153;204;102mgrpc.code",
        "\u{1b}[38;2;153;153;204mgrpc.time_ms",
    ] {
        assert!(
            first.contains(expected),
            "missing {expected:?} in {first:?}"
        );
    }
}

/// The truecolor palette is unusable on Apple Terminal.app, so a terminal that
/// does not announce 24-bit colour gets the `anstyle-lossy` downgrade instead —
/// still ten distinct entries, still keyed by the same hash.
#[test]
fn colour_downgrades_to_256_without_colorterm() {
    let (stdout, _, code) = run(
        &["--color", "always", "--timezone", "utc"],
        REFERENCE.as_bytes(),
    );
    assert_eq!(code, 0);
    let text = String::from_utf8(stdout).expect("output stays UTF-8");
    assert!(text.contains("\u{1b}[38;5;"), "expected ansi256: {text:?}");
    assert!(!text.contains("\u{1b}[38;2;"), "no truecolor: {text:?}");
}

/// HLD §6, invariant 3, including the column that matters: `grpcStatus` shares
/// a prefix with `grpc` but is a different segment and survives.
#[test]
fn an_exclusion_prunes_the_subtree_and_spares_a_look_alike() {
    let line = r#"{"msg":"m","grpc":{"code":"OK","request":{"deadline":"1s"}},"grpcStatus":2}"#;
    assert_eq!(
        render(
            &["--color", "never", "--timezone", "utc", "-e", "grpc"],
            line
        ),
        "m grpcStatus=2\n"
    );
}

// ------------------------------------------------------------------ excludes

/// The §6 exclude table, this time end to end: what the parser accepted has to
/// reach the renderer with the same meaning.
#[test]
fn the_exclude_table_reaches_the_output() {
    let line = r#"{"foo":1,"bar":2}"#;
    let base = ["--color", "never"];

    // `hog` — nothing hidden.
    assert_eq!(render(&base, line), "bar=2 foo=1\n");
    // `hog -e foo` — foo hidden, whatever else the config had.
    assert_eq!(render(&["--color", "never", "-e", "foo"], line), "bar=2\n");
    // `hog -E` — no config list in v0.1, so everything is still shown.
    assert_eq!(render(&["--color", "never", "-E"], line), "bar=2 foo=1\n");
    // `hog -E -e foo` — exactly ["foo"].
    assert_eq!(
        render(&["--color", "never", "-E", "-e", "foo"], line),
        "bar=2\n"
    );
}

#[test]
fn exclude_is_repeatable_and_comma_separated() {
    let line = r#"{"a":1,"b":2,"c":3}"#;
    for args in [
        vec!["--color", "never", "-e", "a,b"],
        vec!["--color", "never", "-e", "a", "-e", "b"],
    ] {
        assert_eq!(render(&args, line), "c=3\n", "args: {args:?}");
    }
}

// ------------------------------------------------------------- byte fidelity

/// The regression that unit tests could not see. A log line is not required to
/// be UTF-8, and the bytes must reach the fd exactly as they came.
#[test]
fn invalid_utf8_survives_the_real_stdout() {
    let input: &[u8] = b"{\"a\":1}\n\xff\xfe\xfd bad bytes\nafter\n";

    // Without colour the whole stream is byte-for-byte what went in, modulo the
    // one line that was JSON.
    for args in [
        vec!["--color", "never"],
        vec![], // auto, resolving to "never" because stdout is a pipe
    ] {
        let (stdout, stderr, code) = run(&args, input);
        assert_eq!(code, 0, "args {args:?}, stderr: {stderr}");
        assert_eq!(
            stdout, b"a=1\n\xff\xfe\xfd bad bytes\nafter\n",
            "args: {args:?}"
        );
    }

    // With colour the rendered line gains escapes, but the pass-through line —
    // the one that is not UTF-8 — is still emitted untouched.
    let (stdout, stderr, code) = run(&["--color", "always"], input);
    assert_eq!(code, 0, "stderr: {stderr}");
    let tail: &[u8] = b"\n\xff\xfe\xfd bad bytes\nafter\n";
    assert!(stdout.ends_with(tail), "raw bytes were mangled: {stdout:?}");
}

/// hulog's headline bug: one line over the 1 MiB scanner limit ended the loop,
/// nobody checked `scanner.Err()`, and the rest of the log vanished with exit
/// code 0. Here the over-long line costs its own tail and nothing else.
#[test]
fn an_over_long_line_does_not_end_the_stream() {
    let mut input = b"{\"msg\":\"before\"}\n".to_vec();
    input.extend_from_slice(b"{\"msg\":\"huge\",\"pad\":\"");
    input.extend(std::iter::repeat_n(b'x', 3 * 1024 * 1024));
    input.extend_from_slice(b"\"}\n{\"msg\":\"after\"}\n");

    let (stdout, stderr, code) = run(&["--color", "never"], &input);

    assert_eq!(code, 0);
    // `split` yields one piece more than there are separators, and the output
    // ends with a newline — so three lines are four pieces. Written this way
    // rather than counting bytes so that nothing in the tree needs an
    // `expect(clippy::naive_bytecount)` to hold `-D warnings`: `bytecount` is
    // not a dependency and three newlines once in a test will never justify one.
    let pieces = stdout.split(|byte| *byte == b'\n').count();
    assert_eq!(pieces, 4, "three rendered lines");
    assert!(stdout.starts_with(b"before\n"), "the first line rendered");
    assert!(stdout.ends_with(b"\nafter\n"), "the stream survived");
    assert!(stderr.contains("longer than 1 MiB"), "stderr: {stderr}");
}
