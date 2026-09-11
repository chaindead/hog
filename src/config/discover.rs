//! Where the config file lives (HLD §3), computed without touching the disk.
//!
//! The whole search is a pure function of four values — the `--config` path and
//! three environment variables — and that is not incidental. `std::env::set_var`
//! is `unsafe` in edition 2024 and this package denies `unsafe_code` outright,
//! so a test **cannot** put `XDG_CONFIG_HOME` into the process environment to
//! exercise the search order. Reading the environment once, at the edge, into
//! [`Env`] is what keeps the rules testable.
//!
//! Six lines of `std` instead of `dirs` / `directories` / `etcetera`: those
//! crates hardcode `~/Library/Application Support` on macOS as a matter of
//! maintainer policy, and `hog` wants the same `~/.config/hog/config.toml` on
//! the mac it runs on and the Linux host it ssh's into. `etcetera` additionally
//! demands a `top_level_domain` and an `author` that appear in no path on any
//! platform hog supports.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Directory under the config home: `~/.config/`**`hog`**`/config.toml`.
pub const CONFIG_DIR: &str = "hog";
/// File name inside it: `~/.config/hog/`**`config.toml`**.
pub const CONFIG_FILE: &str = "config.toml";
/// The fallback config home relative to `$HOME`: `~/`**`.config`**.
pub const XDG_FALLBACK_DIR: &str = ".config";

/// Environment variable naming a config file outright (HLD §6).
pub const HOG_CONFIG_VAR: &str = "HOG_CONFIG";
/// Environment variable naming the XDG config home.
pub const XDG_CONFIG_HOME_VAR: &str = "XDG_CONFIG_HOME";
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
    /// `$XDG_CONFIG_HOME/hog/config.toml`.
    XdgConfigHome,
    /// `$HOME/.config/hog/config.toml`.
    HomeConfig,
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

    /// How to name this layer in a message: `--config`, `$HOG_CONFIG`,
    /// `$XDG_CONFIG_HOME` or `~/.config`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "--config",
            Self::Env => "$HOG_CONFIG",
            Self::XdgConfigHome => "$XDG_CONFIG_HOME",
            Self::HomeConfig => "~/.config",
        }
    }
}

/// The config file hog will read, and the layer that named it.
///
/// The path is *not* promised to exist. `hog config --path` prints it either
/// way — the answer to "where do I put my config?" is the same file that would
/// have been read — and `hog config --init` creates it.
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
    /// `$XDG_CONFIG_HOME`.
    pub xdg_config_home: Option<OsString>,
    /// `$HOME`.
    pub home: Option<OsString>,
}

impl Env {
    /// Samples the three variables from the process environment.
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
            xdg_config_home: std::env::var_os(XDG_CONFIG_HOME_VAR),
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
/// `$XDG_CONFIG_HOME` and no `$HOME`. That is the "built-in defaults, silently"
/// case of the search order, and it is not an error — but note that
/// `hog config --init` has nowhere to write and must say so.
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

/// The default config path: `$XDG_CONFIG_HOME/hog/config.toml`, else
/// `$HOME/.config/hog/config.toml`.
///
/// `$XDG_CONFIG_HOME` is ignored when it is empty or relative, as the XDG base
/// directory spec requires, and the search falls through to `$HOME` — which is
/// what a shell profile that sets `XDG_CONFIG_HOME=""` actually means.
pub fn default_path(env: &Env) -> Option<Location> {
    let (home_dir, source) = match env.xdg_config_home.as_deref() {
        Some(value) if is_usable_config_home(value) => {
            (PathBuf::from(value), Source::XdgConfigHome)
        }
        // Both the "unset" and the "unusable" cases land here, which is the
        // whole point of the spec's rule: a broken `XDG_CONFIG_HOME` behaves
        // like no `XDG_CONFIG_HOME` at all.
        _ => {
            let home = env.home.as_deref().filter(|home| !home.is_empty())?;
            (Path::new(home).join(XDG_FALLBACK_DIR), Source::HomeConfig)
        }
    };

    Some(Location {
        path: home_dir.join(CONFIG_DIR).join(CONFIG_FILE),
        source,
    })
}

/// Is this `$XDG_CONFIG_HOME` value usable — non-empty and absolute?
///
/// Split out because it is the rule most likely to be got wrong twice: once
/// here and once in a test that only passes because it repeats the same
/// mistake.
pub fn is_usable_config_home(value: &OsStr) -> bool {
    !value.is_empty() && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an [`Env`] from the three values, `""` meaning "not set".
    ///
    /// The empty string is not ambiguous here: an empty `$XDG_CONFIG_HOME` has
    /// its own test below that builds the `Env` by hand.
    fn env(hog_config: &str, xdg: &str, home: &str) -> Env {
        fn var(value: &str) -> Option<OsString> {
            if value.is_empty() {
                None
            } else {
                Some(OsString::from(value))
            }
        }
        Env {
            hog_config: var(hog_config),
            xdg_config_home: var(xdg),
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
        let all_set = env("/env/hog.toml", "/xdg", "/home/you");
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

        // 3a. no explicit path: $XDG_CONFIG_HOME.
        assert_eq!(
            located(None, &all_set),
            Some(("/xdg/hog/config.toml".to_owned(), Source::XdgConfigHome))
        );

        // 3b. no $XDG_CONFIG_HOME: ~/.config.
        assert_eq!(
            located(None, &env("", "", "/home/you")),
            Some((
                "/home/you/.config/hog/config.toml".to_owned(),
                Source::HomeConfig
            ))
        );

        // 4. nothing to go on at all: built-in defaults, silently.
        assert_eq!(located(None, &env("", "", "")), None);
    }

    // $HOG_CONFIG reaches us through clap as `explicit`, so an env var with no
    // explicit path cannot happen — but if it ever did, it must not be read as
    // a config home.
    #[test]
    fn hog_config_alone_does_not_produce_a_location() {
        assert_eq!(located(None, &env("/env/hog.toml", "", "")), None);
    }

    #[test]
    fn an_explicit_path_is_taken_verbatim() {
        // Relative, with no config dir appended and no existence check.
        assert_eq!(
            located(Some("./ci.toml"), &env("", "/xdg", "/home/you")),
            Some(("./ci.toml".to_owned(), Source::Flag))
        );
    }

    #[test]
    fn an_explicit_path_is_the_flag_when_hog_config_is_unset() {
        assert_eq!(
            located(Some("/a/hog.toml"), &env("", "", "/home/you")),
            Some(("/a/hog.toml".to_owned(), Source::Flag))
        );
    }

    #[test]
    fn an_explicit_path_that_only_looks_like_hog_config_is_the_flag() {
        let env = env("/env/hog.toml", "", "/home/you");
        assert_eq!(
            located(Some("/env/hog.toml/"), &env).map(|(_, source)| source),
            Some(Source::Flag),
            "the comparison is on the raw value, not on path equivalence"
        );
    }

    // The XDG spec: a relative or empty XDG_CONFIG_HOME is to be ignored.
    #[test]
    fn an_unusable_xdg_config_home_falls_through_to_home() {
        for xdg in ["relative/config", ".", ""] {
            let env = Env {
                hog_config: None,
                xdg_config_home: Some(OsString::from(xdg)),
                home: Some(OsString::from("/home/you")),
            };
            assert_eq!(
                located(None, &env),
                Some((
                    "/home/you/.config/hog/config.toml".to_owned(),
                    Source::HomeConfig
                )),
                "XDG_CONFIG_HOME={xdg:?} should be ignored"
            );
        }
    }

    #[test]
    fn an_unusable_xdg_config_home_with_no_home_gives_nothing() {
        let env = Env {
            hog_config: None,
            xdg_config_home: Some(OsString::from("relative/config")),
            home: Some(OsString::new()),
        };
        assert_eq!(located(None, &env), None);
    }

    #[test]
    fn usable_config_homes_are_absolute_and_non_empty() {
        assert!(is_usable_config_home(OsStr::new("/home/you/.config")));
        assert!(!is_usable_config_home(OsStr::new("")));
        assert!(!is_usable_config_home(OsStr::new("config")));
        assert!(!is_usable_config_home(OsStr::new("./config")));
        assert!(
            !is_usable_config_home(OsStr::new("~/config")),
            "hog does not expand ~; the shell does"
        );
    }

    #[test]
    fn default_path_appends_the_program_directory_and_file_name() {
        let location = default_path(&env("", "/xdg", "")).expect("an absolute XDG home is usable");
        assert!(
            location
                .path
                .ends_with(Path::new(CONFIG_DIR).join(CONFIG_FILE))
        );
    }

    #[test]
    fn explicit_sources_are_the_ones_a_missing_file_is_fatal_for() {
        assert!(Source::Flag.is_explicit());
        assert!(Source::Env.is_explicit());
        assert!(!Source::XdgConfigHome.is_explicit());
        assert!(!Source::HomeConfig.is_explicit());
    }

    #[test]
    fn every_source_names_the_knob_the_user_would_turn() {
        assert_eq!(Source::Flag.label(), "--config");
        assert_eq!(Source::Env.label(), "$HOG_CONFIG");
        assert_eq!(Source::XdgConfigHome.label(), "$XDG_CONFIG_HOME");
        assert_eq!(Source::HomeConfig.label(), "~/.config");
    }

    /// Not a tautology: it is the one test that would catch `home` being filled
    /// from `XDG_CONFIG_HOME`, which is a copy-paste away and would send every
    /// config read to the wrong directory.
    #[test]
    fn from_process_reads_each_variable_from_its_own_name() {
        let env = Env::from_process();
        assert_eq!(env.hog_config, std::env::var_os(HOG_CONFIG_VAR));
        assert_eq!(env.xdg_config_home, std::env::var_os(XDG_CONFIG_HOME_VAR));
        assert_eq!(env.home, std::env::var_os(HOME_VAR));
    }
}
