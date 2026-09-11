//! Внутренний крейт бинарника hog. Не стабильное API.
//!
//! `hog` reads JSON logs line by line and prints them for humans. The crate is
//! split into a library plus a thin `main.rs` so that the pure, valuable parts
//! (`render`, and later `command::template` / `config::edit`) are reachable from
//! `tests/` and `benches/` without spawning a process.
//!
//! # Visibility policy
//!
//! `pub(crate)` by default. `pub` only where an integration test or a bench
//! genuinely calls the item — today that is [`Cli`], [`run`], [`report`],
//! [`settings::Settings`], [`render::Renderer`], and the [`config`] tree, whose
//! pure functions (`discover::locate`, `edit::append_exclude`, `load::parse`)
//! are exactly the ones HLD §2 names as the reason this crate has a lib target
//! at all — the unit tests under `src/config/` drive them directly rather than
//! by spawning a process and reading back a file, and `tests/config_cli.rs`
//! checks only what the process itself adds: the environment, the two output
//! streams and the exit codes.
//!
//! # Milestones
//!
//! * **v0.1**: `cat f.log | hog`, `hog -e field`.
//! * **v0.2**: TOML config, `hog config …` — module [`config`].
//! * **v0.3**: command mode — module [`command`].
//! * **v1.0**: `hog completions <shell>`, the generated `man/hog.1`, README.

use std::io::Write as _;
use std::process::ExitCode;

pub mod cli;
// `pub`, like `config` and for the same reason: HLD §2 gives command mode a
// pure core (`template`, `validate`) and an impure shell around it, and
// `tests/cmd_template.rs` drives that core directly rather than by spawning a
// process. A `pub(crate)` module would put the substitution and the whitelist
// out of an integration test's reach.
pub mod command;
pub mod config;
pub mod error;
// `pub` for `parse`, which is what `main` calls in place of `Cli::parse()`:
// `--help` carries the user's own command template, so the command line is
// parsed twice and the second pass needs a block built from the first.
pub mod help;
pub mod render;
pub mod settings;

pub(crate) mod input;
pub(crate) mod output;
pub(crate) mod pipeline;

pub use cli::Cli;
pub use error::{Error, report};

/// What `-v/--version` prints, decided at build time by `build.rs` (HLD §6).
///
/// Not `CARGO_PKG_VERSION`: that says `0.1.0` for the tagged release, for the
/// commit after it and for a tree with uncommitted edits alike, which is the one
/// thing a version string must not do. `build.rs` resolves `$HOG_VERSION` (set
/// by the release workflow from the tag) → `git describe` → `dev`, so a release
/// prints `hog v1.0.0` and everything else prints `hog dev (a1b2c3d, dirty)`.
///
/// `cli.rs` is the only caller: clap prints `{bin} {version}` for this string.
pub const VERSION: &str = env!("HOG_VERSION");

use crate::cli::Cmd;

/// Runs the whole program for an already-parsed command line.
///
/// Returns the process exit code on success. Errors travel as `anyhow::Error`
/// and are turned into an exit code by [`report`]; the codes themselves come
/// from the typed [`Error`] enum, never from error text.
pub fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    // The one place the process environment is read for config discovery.
    // Everything below takes it as an argument, which is what makes the search
    // order testable without `std::env::set_var` (this package denies unsafe).
    let env = config::Env::from_process();

    // And the one place the config file is created. It happens here, above the
    // mode switch, because "on the first run" means *every* first run — the
    // stdin pipeline, command mode and `hog config …` alike — and because
    // nothing below may read a config before it exists. It cannot fail: a
    // `$HOME` hog cannot write is worth a line on stderr, not a tool that
    // refuses to show logs (see `config::ensure_default`).
    {
        let mut stderr = std::io::stderr().lock();
        config::ensure_default(cli.config.as_deref(), &env, &mut stderr);
    }

    if let Some(command) = cli.command {
        return match command {
            Cmd::Config { action } => {
                let request = config::cmd::Request {
                    action: action.as_ref(),
                    explicit: cli.config.as_deref(),
                    env: &env,
                    // The second and last read of the process environment, for
                    // the same reason as `Env::from_process` above: sampled at
                    // the edge so that `config::cmd::run` stays a function of
                    // its argument and a test can hand it an editor.
                    editor: config::cmd::editor_from_env(),
                };
                // Two streams, not one: `config path` is meant to be captured
                // in a shell substitution, so the unknown-key warnings must not
                // land inside `$(hog config path)`.
                let mut stdout = std::io::stdout().lock();
                // Same broken-pipe rule as every other stdout writer: these
                // verbs are documented to be piped (`hog config exclude | wc
                // -l`), and `| head` on a long list must exit 141 in silence
                // like the log stream does, not print `error: Broken pipe` and
                // exit 1 (HLD §6's exit-code table).
                config::cmd::run(&request, &mut stdout, &mut std::io::stderr().lock())
                    .and_then(|()| stdout.flush().map_err(anyhow::Error::new))
                    .map_err(config_write_failed)?;
                Ok(ExitCode::SUCCESS)
            }
            Cmd::Completions { shell } => {
                print_completions(shell)?;
                Ok(ExitCode::SUCCESS)
            }
        };
    }

    // The config layer plugs in exactly here, and nowhere else: the file is
    // located, read and audited once, `resolve` folds it between the built-in
    // defaults and the CLI flags, and no later stage looks at a `Model` again.
    // Unknown keys are warned about on stderr, never fatal (HLD §7.2).
    let loaded = {
        let mut stderr = std::io::stderr().lock();
        config::load_for_run(cli.config.as_deref(), &env, &mut stderr)?
    };
    let settings = settings::resolve(&cli.run, loaded.as_ref().map(|loaded| &loaded.model))?;

    // Mode selection and the terminal gate both live in `input::select`, and it
    // runs first: "stdin is a terminal" and every refusal in HLD §5 are
    // usage-time answers, and they should surface before we lock stdout, probe
    // the terminal for colour or spawn anything.
    match input::select(&cli.run, &settings)? {
        // `--dry-run` never reaches here: `input::select` sends it down the
        // command branch whatever stdin is, so the flag cannot end up silently
        // doing nothing (or, worse, blocking on input).
        input::Mode::Stdin => {
            let mut input = input::Input::stdin();
            stream(&mut input, settings)?;
        }

        input::Mode::Command(plan) if cli.run.dry_run => {
            // HLD §5: print the assembled argv and exit, running nothing.
            print_dry_run(&plan)?;
        }

        input::Mode::Command(plan) => {
            // Declared here so that every path out of this arm — including the
            // `?` on a broken pipe — drops it, and `Session::drop` kills and
            // reaps the child. See `command::session`.
            let mut session = command::session::spawn(&plan)?;
            let lines = stream(session.input(), settings)?;
            // The line count is the loud line's whole point (HLD §5): a VPN
            // that dropped after 1423 lines must not read like "the log ended".
            session.finish(lines)?;
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// Builds the output stack and runs the streaming loop, returning the number of
/// lines written.
///
/// Output is built before the renderer: the stream decides how much colour the
/// terminal can take, and the theme needs that answer up front so it can
/// precompute the palette instead of downgrading a colour per key per line.
fn stream(input: &mut input::Input, settings: settings::Settings) -> anyhow::Result<u64> {
    let mut output = output::Output::stdout(settings.color);
    let mut renderer = render::Renderer::new(settings, output.color())?;
    pipeline::run(input, &mut renderer, &mut output)
}

/// Prints the argv `--dry-run` would have run: one word per line, exit 0.
///
/// Locks stdout directly rather than going through [`output`], which exists to
/// be the single write site for the *log stream* — `hog config --path` already
/// takes the same route for the same reason. `println!` stays banned: it panics
/// on a closed stdout, and `hog --dry-run prod api | head -1` is an ordinary
/// thing to type.
fn print_dry_run(plan: &command::CommandPlan) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    let failed = |err| stdout_write_failed(err, "writing the dry run to stdout");
    writeln!(stdout, "{}", plan.dry_run_text()).map_err(failed)?;
    stdout.flush().map_err(failed)?;
    Ok(())
}

/// Prints the shell completion script for `shell`: `hog completions zsh`.
///
/// The script is built in memory before a byte of it is written, and that is
/// not tidiness. `clap_complete`'s shell writers `unwrap` their own write
/// errors, so generating straight into a closed stdout — `hog completions zsh |
/// head` — would panic and bypass the exit-code table entirely. Generating into
/// a `Vec` first leaves exactly one fallible write, which takes the same
/// broken-pipe route as everything else (HLD §6: exit 141).
///
/// [`Cli::command`] rather than [`help::parse`]'s command: the `after_help`
/// block carries the user's own template, which has no business in a completion
/// script, and building it would read the config file for nothing.
fn print_completions(shell: clap_complete::Shell) -> anyhow::Result<()> {
    use clap::CommandFactory as _;

    let mut command = Cli::command();
    let mut script: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut command, "hog", &mut script);

    let mut stdout = std::io::stdout().lock();
    let failed = |err| stdout_write_failed(err, "writing the completion script to stdout");
    stdout.write_all(&script).map_err(failed)?;
    stdout.flush().map_err(failed)?;
    Ok(())
}

/// The broken-pipe rule for `hog config`, whose writes are spread across the
/// whole of [`config::cmd`] rather than made at one call site.
///
/// The `io::Error` is recovered from the chain instead of being matched where
/// it was raised: every verb writes through a `W: Write` it was handed, and
/// threading a pipe-aware error type through all of them would put the same
/// three lines in a dozen functions. Anything that is not a broken pipe is
/// returned untouched, so a failure to *read* the config still reports itself.
fn config_write_failed(err: anyhow::Error) -> anyhow::Error {
    let broken = err
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe);

    if broken {
        Error::BrokenPipe.into()
    } else {
        err
    }
}

/// The broken-pipe rule, for the writes that do not go through the pipeline.
fn stdout_write_failed(err: std::io::Error, doing: &'static str) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::BrokenPipe {
        return Error::BrokenPipe.into();
    }
    anyhow::Error::new(err).context(doing)
}
