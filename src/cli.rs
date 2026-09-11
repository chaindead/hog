//! The verified clap grammar from HLD §6. Declaration only — no logic here.
//!
//! The mode is chosen by the **positional arguments**, not by a flag: with
//! arguments `hog` runs the `command` template from the config and substitutes
//! them by position (`{0}`, `{1}`, …); without them it reads stdin. HLD §11
//! removed `-r/--ssh`, `SERVICE`, `--since` and `--tail` outright — hog no
//! longer builds an ssh invocation, it runs whatever the config's `command`
//! says, and a variable interval is just one more positional argument.
//!
//! Three details are load-bearing and must not be "cleaned up":
//!
//! * `-e/--exclude` must **not** use `num_args = 1..`. A multi-value `-e` eats
//!   the positionals, so `hog -e a,b prod api` would stop parsing.
//! * `args_conflicts_with_subcommands` must **not** be set — see below.
//! * `-E` deliberately has **no** `conflicts_with = "exclude"`: `hog -E -e foo`
//!   is the documented way to replace the config's exclude list (HLD §10.4).
//!
//! # Why `args_conflicts_with_subcommands` is gone
//!
//! HLD §6 prints it in the verified grammar, and it is **wrong** — measured on
//! the built binary, not reasoned about. `--config` is `global = true`, so it
//! belongs to the parent command; with the flag set, clap counts it as "the
//! parent's args were given" and stops considering the subcommand at all:
//!
//! ```text
//! $ hog --config ci.toml config path      # with the flag
//! error: unexpected argument 'path' found         <- `config` became ARGS[0]
//! $ hog --config ci.toml config           # with the flag, and worse
//! config                                          <- silently the command mode:
//!                                                    `echo {@}` ran on ARGS=["config"]
//! ```
//!
//! The second one is the dangerous one: nothing names the mistake at all, the
//! subcommand simply never runs and the exit code is 0. (When this was measured
//! the built-in default did not exist yet and the same invocation printed
//! `error: no command configured`; the diagnosis was the same, and the default
//! has since removed even that clue.)
//!
//! Without the flag both parse as the subcommand, and every row of the HLD §6
//! parse table still holds — the subcommand keeps
//! winning over a first positional spelled `config`, and `hog -- config` is
//! still how that argument is passed. The flag was never what made `config` a
//! subcommand; clap matches a subcommand name before a positional on its own.
//!
//! The one row that changes is one the table never listed: `hog -e x config`
//! used to parse as `ARGS=["config"]` and now parses as the subcommand with a
//! stray `-e x`. That is the documented rule ("the subcommand wins over the
//! first positional") applied consistently, and `hog -e x -- config` says the
//! other thing.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Read JSON logs line by line and print them for humans.
///
/// With no arguments hog reads stdin, so `kubectl logs -f … | hog` and
/// `hog < app.log` both work. With arguments it runs the `command` template
/// from your config file, substituting them by position — see the block at the
/// end of this help for the template that is in effect right now.
///
/// One input line always produces exactly one output line, the tail keys are
/// sorted, and a key always gets the same colour, so the output stays usable
/// through `grep`, `head` and `diff`.
//
// Everything below this line is a note to the next maintainer, written with
// `//` rather than `///` on purpose: clap turns a doc comment on this struct
// into the `long_about` that every `hog --help` prints, and implementation
// rationale is not what a user asked for when they typed `--help`.
//
// `version` is `crate::VERSION`, not `CARGO_PKG_VERSION`: HLD §6 wants the tag
// the binary was built from, and `build.rs` resolves it. `disable_version_flag`
// turns off clap's generated flag so the declared one below can own `-v`, which
// clap's own flag spells `-V`.
#[derive(Debug, Parser)]
#[command(name = "hog", version = crate::VERSION, disable_version_flag = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,

    #[command(flatten)]
    pub run: RunArgs,

    /// Path to the config file (default: $HOME/.hog.toml).
    #[arg(long, global = true, env = "HOG_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Print the release tag this binary was built from.
    ///
    /// `-V` is accepted as well: it is clap's standard short form and people
    /// have the muscle memory. There is no `--verbose` in hog to collide with.
    //
    // The field is `()` because `ArgAction::Version` prints and exits during
    // parsing — nothing ever reads a value out of it. `help::preflight_config`
    // has to know about this argument by name: an action that ends the parse is
    // exactly what the two-pass `--help` must not trip over.
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    pub version: (),
}

/// The default mode: read a stream and render it.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Substituted into the config's `command` template by position:
    /// `{0}` is the first, `{1}` the second, and so on.
    ///
    /// With no arguments `hog` reads stdin instead. An argument spelled like a
    /// subcommand is passed after `--`: `hog -- config api`.
    #[arg(value_name = "ARGS")]
    pub args: Vec<String>,

    /// Hide FIELD and everything under it; repeatable and comma-separated.
    ///
    /// Paths are dotted and matched on segment boundaries: `-e grpc` hides
    /// `grpc.code`, but never `grpcStatus`. Adds to the config's list.
    #[arg(
        short = 'e',
        long = "exclude",
        value_name = "FIELD",
        value_delimiter = ','
    )]
    pub exclude: Vec<String>,

    /// Ignore the exclude list from the config file.
    ///
    /// Combine with `-e` to replace it outright: `hog -E -e foo`.
    #[arg(short = 'E', long = "reset-exclude")]
    pub reset_exclude: bool,

    /// When to colourise output; the default is `auto`.
    ///
    /// `NO_COLOR` and `CLICOLOR_FORCE` are honoured, and the config file's
    /// `output.color` sets it when this flag is absent.
    // An `Option` rather than `default_value_t`, and that is load-bearing: with
    // a clap default, "not given" and "given as auto" are the same value, the
    // flag would win over `output.color` on every run, and the key could never
    // take effect (HLD §3, layers). Kept as a `//` comment so that the reason
    // does not end up in `--help`.
    #[arg(long, value_enum, value_name = "COLOR")]
    pub color: Option<ColorChoiceArg>,

    /// Print the command that would run, with the arguments substituted,
    /// and exit without running it.
    #[arg(long)]
    pub dry_run: bool,

    /// JSON field holding the timestamp; replaces the candidate list.
    #[arg(long, value_name = "FIELD")]
    pub ts_field: Option<String>,

    /// Timestamp output format: a strftime pattern, or `raw`, or `none`.
    #[arg(long, value_name = "FMT")]
    pub ts_format: Option<String>,

    /// JSON field holding the level; replaces the candidate list.
    #[arg(long, value_name = "FIELD")]
    pub level_field: Option<String>,

    /// JSON field holding the message; replaces the candidate list.
    #[arg(long, value_name = "FIELD")]
    pub msg_field: Option<String>,

    /// Time zone for the timestamp column: `local`, `utc`, or an IANA name.
    #[arg(long, value_name = "TZ")]
    pub timezone: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Inspect and edit the config file.
    ///
    /// With no verb it prints the resolved configuration and the path it came
    /// from. Every verb below names one thing to look at or change.
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },

    /// Emit a shell completion script (v1.0).
    #[command(hide = true)]
    Completions { shell: clap_complete::Shell },
}

/// The verbs of `hog config` (HLD §6).
///
/// Subcommands rather than flags, and that is the whole point of the shape:
/// each one gets its own line with its own description in `hog config --help`,
/// and `hog config command set` can take a template that starts with a dash
/// without an `--` dance. This replaced the v0.2 flags `-e` / `-d` / `--path` /
/// `--init`, which are gone rather than deprecated — hog has no released
/// version to keep faith with.
///
/// There is no `init` verb either, and its absence is the feature: hog writes
/// `$HOME/.hog.toml` on the first run that finds it missing
/// (`config::ensure_default`), so a verb whose whole job was to ask for that
/// file would only ever report that it already existed.
#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print the path of the config file that is actually read.
    ///
    /// One line on stdout and nothing else, so `$(hog config path)` is a path.
    Path,

    /// Open the config file in $VISUAL or $EDITOR.
    ///
    /// The file is created from the starter first if it is not there yet, and
    /// checked for syntax, unknown keys and a broken `command` once the editor
    /// exits.
    Edit,

    /// Show the persistent exclude list, or change it.
    Exclude {
        #[command(subcommand)]
        op: Option<ExcludeOp>,
    },

    /// Show the command template, or change it.
    Command {
        #[command(subcommand)]
        op: Option<CommandOp>,
    },
}

/// `hog config exclude add|rm FIELD…`.
#[derive(Debug, Subcommand)]
pub enum ExcludeOp {
    /// Append FIELDs to the exclude list, keeping every comment in the file.
    Add {
        /// Dotted JSON paths; repeatable and comma-separated.
        #[arg(required = true, value_name = "FIELD", value_delimiter = ',')]
        fields: Vec<String>,
    },

    /// Remove FIELDs from the exclude list.
    Rm {
        /// Dotted JSON paths; repeatable and comma-separated.
        #[arg(required = true, value_name = "FIELD", value_delimiter = ',')]
        fields: Vec<String>,
    },
}

/// `hog config command set "<template>"`.
#[derive(Debug, Subcommand)]
pub enum CommandOp {
    /// Write a new command template, after checking that it works.
    ///
    /// The template is split by shell rules and its placeholders are checked
    /// before anything is written, so a template that could only fail at the
    /// next run never reaches the file (HLD §3).
    Set {
        /// The template, e.g. `ssh {0} 'docker logs -f {@}'`.
        ///
        /// `allow_hyphen_values` is on: a template is a command line, and one
        /// starting with a flag (`--follow …`) is not a hog flag.
        #[arg(value_name = "TEMPLATE", allow_hyphen_values = true)]
        template: String,
    },
}

/// `--color` as the user typed it. Resolved into an actual stream policy by
/// `crate::output`, which also consults `NO_COLOR` / `CLICOLOR_FORCE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ColorChoiceArg {
    #[default]
    Auto,
    Always,
    Never,
}

impl From<ColorChoiceArg> for anstream::ColorChoice {
    fn from(value: ColorChoiceArg) -> Self {
        match value {
            ColorChoiceArg::Auto => Self::Auto,
            ColorChoiceArg::Always => Self::Always,
            ColorChoiceArg::Never => Self::Never,
        }
    }
}
