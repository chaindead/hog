//! The TOML config file: where it lives, how it is read, and how `hog config`
//! edits it without losing a comment (HLD §3).
//!
//! # Search order
//!
//! First hit wins, and there is **no merging between files**:
//!
//! 1. `--config PATH` — an error when the file is missing
//! 2. `$HOG_CONFIG` — an error when the file is missing
//! 3. `$XDG_CONFIG_HOME/hog/config.toml`, else `~/.config/hog/config.toml`
//! 4. nothing found → built-in defaults, silently
//!
//! Note what is **not** on that list: no implicit `./hog.toml`, and no walk up
//! the directory tree. The config names a command that hog executes, so picking
//! one up from the current directory would turn `git clone && cd && hog prod
//! api` into remote code execution. A project that genuinely wants a
//! checked-in config passes `--config ./hog.toml` by hand.
//!
//! XDG wins on macOS too. `~/Library/Application Support` is the convention for
//! GUI apps with a reverse-DNS identity; `hog` is a dev CLI that belongs in
//! dotfiles next to `~/.config/gh`, and **the same file has to work unchanged
//! on the Linux hosts it ssh's into**.
//!
//! # Layers
//!
//! `defaults < config file < CLI flags`, folded by `settings::resolve`. The
//! exclude list is the one layer that adds rather than replaces: the config
//! gives a base list, `-e` appends to it, `-E` drops it (HLD §11.6).
//!
//! # Module map
//!
//! | module | job |
//! |---|---|
//! | [`discover`] | the search order above, as a pure function of the environment |
//! | [`model`] | serde types mirroring the TOML, plus the list of known keys |
//! | [`load`] | read + parse + audit unknown keys with line numbers |
//! | [`edit`] | `DocumentMut` surgery that preserves comments and array style |
//! | [`write`] | canonicalize → temp file alongside → fsync → rename |
//! | [`cmd`] | the `hog config` subcommand on top of the five above |

use std::io::Write;
use std::path::Path;

pub mod cmd;
pub mod discover;
pub mod edit;
pub mod load;
pub mod model;
pub mod write;

pub use discover::{Env, Location, Source};
pub use load::{Loaded, UnknownKey};
pub use model::Model;

/// Loads the config for a normal run, reporting unknown keys along the way.
///
/// `Ok(None)` is the documented fourth case of the search order: nothing was
/// configured and nothing exists at the default path, so the caller keeps the
/// built-in `Settings::default`. A file that *was* named explicitly and is
/// missing is an error instead — silently ignoring `--config ./ci.toml` would
/// render the whole stream with the wrong settings and no sign of it.
///
/// The unknown-key warnings are written to `warnings` (stderr in the binary)
/// rather than returned, so a caller that only wants the model cannot forget to
/// print them. [`Loaded::unknown`] still holds them for tests.
///
/// `env` is passed in rather than read here for the reason spelled out in
/// [`discover`]: a test cannot put `XDG_CONFIG_HOME` into the process
/// environment, because `std::env::set_var` is `unsafe` and this package denies
/// `unsafe_code`. The binary calls [`Env::from_process`] once, at the edge.
pub fn load_for_run<W: Write>(
    explicit: Option<&Path>,
    env: &Env,
    warnings: &mut W,
) -> anyhow::Result<Option<Loaded>> {
    let loaded = load::load(explicit, env)?;
    if let Some(loaded) = &loaded {
        loaded.warn_unknown_keys(warnings);
    }
    Ok(loaded)
}
