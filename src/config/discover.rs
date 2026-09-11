//! Where the config file lives (HLD §3), computed without touching the disk.
//!
//! The whole search is a pure function of three values — the `--config` path
//! and two environment variables — and that is not incidental.
//! `std::env::set_var` is `unsafe` in edition 2024 and this package denies
//! `unsafe_code` outright, so a test **cannot** put `$HOME` into the process
//! environment to exercise the search order. Reading the environment once, at
//! the edge, into [`Env`] is what keeps the rules testable.
//!
//! One dotfile in one place: `$HOME/.hog.toml`, joined in two lines of `std`
//! rather than through `dirs` / `directories` / `etcetera`. Those crates
//! hardcode `~/Library/Application Support` on macOS as a matter of maintainer
//! policy, and `hog` wants the same file name on the mac it runs on and on the
//! Linux host it ssh's into; `etcetera` additionally demands a
//! `top_level_domain` and an `author` that appear in no path on any platform
//! hog supports.
//!
//! `$HOME` and nowhere else. The name is a dotfile, but it is never looked for
//! beside the working directory and there is no walk up the tree: the config
//! names a command hog executes, so picking one up from a directory somebody
//! else filled would turn `git clone && cd && hog prod api` into remote code
//! execution. A project that genuinely wants a checked-in config passes
//! `--config ./hog.toml` by hand.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The config file name inside `$HOME`: `~/`**`.hog.toml`**.
pub const CONFIG_FILE: &str = ".hog.toml";

/// Environment variable naming a config file outright (HLD §6).
pub const HOG_CONFIG_VAR: &str = "HOG_CONFIG";
/// Environment variable naming the user's home directory.
pub const HOME_VAR: &str = "HOME";

/// Which layer of the search order produced a path.
///
/// This exists for the error texts, and it earns its place there: "no such
/// file" is useless on its own, while "`$HOG_CONFIG` points at a file that does
/// not exist" tells the user which knob to turn. It also decides whether a
/// missing file is fatal at all — see [`Source::is_explicit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--config PATH` on the command line.
    Flag,
    /// `$HOG_CONFIG`.
    ///
    /// clap folds this into the same `Option<PathBuf>` as `--config`
    /// (`env = "HOG_CONFIG"` in `cli.rs`), so the two are told apart in
    /// [`locate`] by comparing the path against the variable. The distinction
    /// changes nothing but the wording of the message.
    Env,
    /// `$HOME/.hog.toml`, the default when nothing names a file.
    Home,
}

impl Source {
    /// Did the user name this file, rather than hog guessing it?
    ///
    /// `true` for [`Source::Flag`] and [`Source::Env`], and it is exactly the
    /// condition under which a missing file is an error: the user asked for
    /// *that* file, so rendering the stream with the built-in defaults instead
    /// would be a silent wrong answer. A missing file at a guessed path is the
    /// ordinary "no config yet" case and stays silent.
    pub fn is_explicit(self) -> bool {
        matches!(self, Self::Flag | Self::Env)
    }

    /// How to name this layer in a message: `--config`, `$HOG_CONFIG` or
    /// `$HOME`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "--config",
            Self::Env => "$HOG_CONFIG",
            Self::Home => "$HOME",
        }
    }
}

/// The config file hog will read, and the layer that named it.
///
/// The path is *not* promised to exist. `hog config path` prints it either way
/// — the answer to "where do I put my config?" is the same file that would have
/// been read — though for [`Source::Home`] it will have been created by the
/// time any verb runs (see [`ensure_default`](super::ensure_default)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub source: Source,
}

/// The environment variables discovery reads, sampled once.
///
/// Passed explicitly rather than read inside [`locate`] so that the search
/// order is a pure function: see the module docs on why a test cannot set these
/// in the process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `$HOG_CONFIG`. Only used to tell [`Source::Flag`] from [`Source::Env`];
    /// the value itself already reached us through clap.
    pub hog_config: Option<OsString>,
    /// `$HOME`.
    pub home: Option<OsString>,
}

impl Env {
    /// Samples the two variables from the process environment.
    ///
    /// The only impure function in this module, and the only place the rest of
    /// the config code is allowed to look at the environment.
    ///
    /// `var_os` rather than `var`: a `$HOME` that is not UTF-8 is unusual but
    /// perfectly legal on unix, and it should give the user their config rather
    /// than a silent fall-through to the built-in defaults.
    pub fn from_process() -> Self {
        Self {
            hog_config: std::env::var_os(HOG_CONFIG_VAR),
            home: std::env::var_os(HOME_VAR),
        }
    }
}

/// Resolves the config path per HLD §3, or `None` when there is nowhere to
/// look.
///
/// `explicit` is `Cli::config` — the value `--config` and `$HOG_CONFIG` share.
/// When it is `Some`, that path wins outright and the source is [`Source::Env`]
/// if it is byte-identical to `$HOG_CONFIG`, [`Source::Flag`] otherwise.
///
/// `None` means the default path could not be computed at all: no usable
/// `$HOME`. That is the "built-in defaults, silently" case of the search order,
/// and it is not an error — but note that there is then nowhere to create a
/// config either, and `hog config` must say so rather than print an empty path.
///
/// No filesystem call happens here, not even an `exists()`: TOCTOU aside, a
/// path that cannot be read is [`load`](super::load)'s news to break, and it
/// needs the [`Source`] to phrase it. In particular there is deliberately no
/// "try the next layer if this file does not exist" — the first hit wins, and
/// hog never silently reads a different file than the one it was pointed at.
pub fn locate(explicit: Option<&Path>, env: &Env) -> Option<Location> {
    match explicit {
        Some(path) => {
            // clap hands `--config` and `$HOG_CONFIG` over in the same field,
            // so the only way to tell them apart is to compare the value. An
            // exact match is the right test: `--config "$HOG_CONFIG"` is the
            // same file named twice, and calling it either name is honest.
            let source = if env.hog_config.as_deref() == Some(path.as_os_str()) {
                Source::Env
            } else {
                Source::Flag
            };
            Some(Location {
                path: path.to_path_buf(),
                source,
            })
        }
        None => default_path(env),
    }
}

/// The default config path: `$HOME/.hog.toml`.
///
/// An empty `$HOME` counts as unset — a shell profile that exports `HOME=`
/// leaves the file name `/.hog.toml`, which is not the user's config by any
/// reading — and then there is no default path at all.
pub fn default_path(env: &Env) -> Option<Location> {
    let home = env.home.as_deref().filter(|home| !home.is_empty())?;
    Some(Location {
        path: Path::new(home).join(CONFIG_FILE),
        source: Source::Home,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an [`Env`] from the two values, `""` meaning "not set".
    ///
    /// The empty string is not ambiguous here: an empty `$HOME` has its own
    /// test below that builds the `Env` by hand.
    fn env(hog_config: &str, home: &str) -> Env {
        fn var(value: &str) -> Option<OsString> {
            if value.is_empty() {
                None
            } else {
                Some(OsString::from(value))
            }
        }
        Env {
            hog_config: var(hog_config),
            home: var(home),
        }
    }

    fn located(explicit: Option<&str>, env: &Env) -> Option<(String, Source)> {
        let explicit = explicit.map(Path::new);
        locate(explicit, env).map(|location| {
            (
                location.path.to_string_lossy().into_owned(),
                location.source,
            )
        })
    }

    // The whole search order of HLD §3, one row per layer.
    #[test]
    fn the_search_order_table() {
        // 1. --config wins over everything, including a $HOG_CONFIG that is set.
        let all_set = env("/env/hog.toml", "/home/you");
        assert_eq!(
            located(Some("/flag/hog.toml"), &all_set),
            Some(("/flag/hog.toml".to_owned(), Source::Flag))
        );

        // 2. the same value as $HOG_CONFIG is reported as $HOG_CONFIG, because
        //    that is the knob the user would have to turn.
        assert_eq!(
            located(Some("/env/hog.toml"), &all_set),
            Some(("/env/hog.toml".to_owned(), Source::Env))
        );

        // 3. no explicit path: $HOME/.hog.toml.
        assert_eq!(
            located(None, &all_set),
            Some(("/home/you/.hog.toml".to_owned(), Source::Home))
        );

        // 4. nothing to go on at all: built-in defaults, silently.
        assert_eq!(located(None, &env("", "")), None);
    }

    // $HOG_CONFIG reaches us through clap as `explicit`, so an env var with no
    // explicit path cannot happen — but if it ever did, it must not be read as
    // a home directory.
    #[test]
    fn hog_config_alone_does_not_produce_a_location() {
        assert_eq!(located(None, &env("/env/hog.toml", "")), None);
    }

    #[test]
    fn an_explicit_path_is_taken_verbatim() {
        // Relative, with no directory prepended and no existence check.
        assert_eq!(
            located(Some("./ci.toml"), &env("", "/home/you")),
            Some(("./ci.toml".to_owned(), Source::Flag))
        );
    }

    #[test]
    fn an_explicit_path_is_the_flag_when_hog_config_is_unset() {
        assert_eq!(
            located(Some("/a/hog.toml"), &env("", "/home/you")),
            Some(("/a/hog.toml".to_owned(), Source::Flag))
        );
    }

    #[test]
    fn an_explicit_path_that_only_looks_like_hog_config_is_the_flag() {
        let env = env("/env/hog.toml", "/home/you");
        assert_eq!(
            located(Some("/env/hog.toml/"), &env).map(|(_, source)| source),
            Some(Source::Flag),
            "the comparison is on the raw value, not on path equivalence"
        );
    }

    /// An exported but empty `HOME` is no home: the default would otherwise be
    /// `/.hog.toml`, a file in the root directory that is nobody's config.
    #[test]
    fn an_empty_home_gives_nothing() {
        let env = Env {
            hog_config: None,
            home: Some(OsString::new()),
        };
        assert_eq!(located(None, &env), None);
    }

    /// The default is a file directly in `$HOME`, not in a directory below it
    /// and not relative to anything else.
    #[test]
    fn the_default_path_is_the_dotfile_in_home() {
        let location = default_path(&env("", "/home/you")).expect("a non-empty HOME is usable");
        assert_eq!(location.path, Path::new("/home/you/.hog.toml"));
        assert_eq!(location.path.file_name(), Some(CONFIG_FILE.as_ref()));
        assert_eq!(location.source, Source::Home);
    }

    /// hog does not expand `~`; the shell does. A `$HOME` that is relative is
    /// the user's own doing and is joined as written rather than guessed at.
    #[test]
    fn a_home_is_joined_exactly_as_it_is_written() {
        assert_eq!(
            located(None, &env("", "relative/home")),
            Some(("relative/home/.hog.toml".to_owned(), Source::Home))
        );
    }

    /// The dotfile is looked for in `$HOME` and nowhere else. A test that only
    /// checked the file *name* would pass for an implementation that also tried
    /// `./.hog.toml` — the two are spelled identically — so this one pins the
    /// whole path, and pins that no `$HOME` means no path rather than a bare
    /// name the OS would resolve against the working directory.
    #[test]
    fn the_default_is_never_relative_to_the_working_directory() {
        let (path, source) = located(None, &env("", "/home/you")).expect("a HOME gives a path");
        assert_eq!(path, "/home/you/.hog.toml");
        assert_eq!(source, Source::Home);
        assert!(Path::new(&path).is_absolute());

        assert_eq!(located(None, &env("", "")), None);
    }

    #[test]
    fn explicit_sources_are_the_ones_a_missing_file_is_fatal_for() {
        assert!(Source::Flag.is_explicit());
        assert!(Source::Env.is_explicit());
        assert!(!Source::Home.is_explicit());
    }

    #[test]
    fn every_source_names_the_knob_the_user_would_turn() {
        assert_eq!(Source::Flag.label(), "--config");
        assert_eq!(Source::Env.label(), "$HOG_CONFIG");
        assert_eq!(Source::Home.label(), "$HOME");
    }

    /// Not a tautology: it is the one test that would catch `home` being filled
    /// from `HOG_CONFIG`, which is a copy-paste away and would send every
    /// config read to the wrong file.
    #[test]
    fn from_process_reads_each_variable_from_its_own_name() {
        let env = Env::from_process();
        assert_eq!(env.hog_config, std::env::var_os(HOG_CONFIG_VAR));
        assert_eq!(env.home, std::env::var_os(HOME_VAR));
    }
}
