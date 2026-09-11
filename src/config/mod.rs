//! The TOML config file: where it lives, how it is read, and how `hog config`
//! edits it without losing a comment (HLD §3).
//!
//! # Search order
//!
//! First hit wins, and there is **no merging between files**:
//!
//! 1. `--config PATH` — an error when the file is missing
//! 2. `$HOG_CONFIG` — an error when the file is missing
//! 3. `~/.hog.toml` — **created from the starter** when it is not there yet
//! 4. nothing found → built-in defaults, silently
//!
//! Layer 3 is the only one hog will create, and [`ensure_default`] is the whole
//! of it. A path the user *named* and misspelled stays an error: they asked for
//! that file, and a fresh empty config somewhere they did not expect answers a
//! question nobody asked.
//!
//! Note what is **not** on that list: no implicit `./hog.toml` or `./.hog.toml`,
//! and no walk up the directory tree. The config names a command that hog
//! executes, so picking one up from the current directory would turn
//! `git clone && cd && hog prod api` into remote code execution. A project that
//! genuinely wants a checked-in config passes `--config ./hog.toml` by hand.
//!
//! One dotfile in the home directory, on every platform. `hog` is a dev CLI
//! whose config travels in dotfiles, and **the same file has to work unchanged
//! on the Linux hosts it ssh's into** — so no `~/Library/Application Support`
//! on macOS, and nothing to compute beyond `$HOME` and a name.
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

/// Prefix on the two lines hog writes about creating its own config file.
///
/// It is there because these lines share stderr with whatever the user is
/// actually watching — a `kubectl logs -f` warning, a shell's own noise — and a
/// bare `created /Users/you/.hog.toml` would not say who said it. The
/// unknown-key warnings keep their `warning:` prefix instead: those are about
/// the user's file, not about hog.
const PREFIX: &str = "hog: ";

/// Creates `$HOME/.hog.toml` from the starter if it is not there yet.
///
/// Called once per run, from [`crate::run`], before anything reads a config —
/// so `cat app.log | hog` and `hog config path` both get a config file on a
/// fresh machine, and every later stage sees the same world whichever verb
/// created it.
///
/// Three rules, and all three are the point of the function:
///
/// 1. **only the default path.** `explicit` is `Cli::config`, which clap fills
///    from `--config` *and* `$HOG_CONFIG`; when it is set, hog creates nothing
///    and [`load`] reports the missing file as the error it is. The user named
///    a file — a silently invented one at their misspelled path would be a
///    worse answer than a refusal.
/// 2. **never fatal.** A read-only `$HOME`, a directory hog may not write, a
///    second `hog` that won the race — none of them are a reason to refuse to
///    render logs. The write failure is worth one line on stderr and nothing
///    more; the run carries on with the built-in defaults, which is what a run
///    without a config file does anyway.
/// 3. **`create_new`, never `exists()`.** Two `hog`s starting at the same
///    moment on a fresh machine both find no file; with a check-then-write one
///    of them would overwrite the other. The kernel answers the question at the
///    moment of creation instead, and the loser simply reads what the winner
///    wrote — see [`write::create_new`].
///
/// The starter is the built-in defaults plus the documentation of the format
/// (see [`edit::STARTER`]), so creating it changes **nothing** about how the
/// run behaves. That is what makes doing it unasked acceptable in the first
/// place.
///
/// `notes` is stderr in the binary. Never stdout: `hog … | grep` must not find
/// this line in the stream it is filtering.
pub fn ensure_default<W: Write>(explicit: Option<&Path>, env: &Env, notes: &mut W) {
    // Rule 1: a file the user named is not ours to invent.
    if explicit.is_some() {
        return;
    }
    let Some(location) = discover::default_path(env) else {
        // No usable `$HOME`: nowhere to put it, and nothing to say about it.
        return;
    };

    // hog creates a *file*, not a home directory. A `$HOME` that does not exist
    // is a broken environment, and `mkdir -p`-ing one on the way past would
    // hide that rather than fix it.
    if !location.path.parent().is_some_and(Path::is_dir) {
        return;
    }

    // Rule 2: every failure from here on is reported and forgotten.
    if let Err(err) = create_starter(&location.path, notes) {
        // `root_cause`, not `{err:#}`: the context chain this walks past says
        // "creating <path>" and this line already names the path. One line
        // means one line, and the interesting half of it is the `why`.
        let _ = writeln!(
            notes,
            "{PREFIX}could not create {}: {}",
            location.path.display(),
            err.root_cause()
        );
    }
}

/// Writes the starter config at `path` unless something is already there, and
/// says so on `notes` when it did.
///
/// The one place the "created …" line is spelled, shared by [`ensure_default`]
/// and `hog config edit` — the two paths that bring a config file into
/// existence. Both write [`edit::STARTER`] rather than an empty file: the
/// starter *is* the documentation of the format, and an empty buffer in an
/// editor means writing a config from memory.
pub(crate) fn create_starter<W: Write>(path: &Path, notes: &mut W) -> anyhow::Result<()> {
    if write::create_new(path, edit::STARTER)? == write::Init::Written {
        // Best effort, like every diagnostic: a closed stderr must not turn a
        // successful write into a failure.
        let _ = writeln!(notes, "{PREFIX}created {}", path.display());
    }
    Ok(())
}

/// Loads the config for a normal run, reporting unknown keys along the way.
///
/// `Ok(None)` is the documented fourth case of the search order: nothing was
/// configured and nothing exists at the default path, so the caller keeps the
/// built-in `Settings::default`. Since [`ensure_default`] runs first, that now
/// means the default file could not be created either — a read-only `$HOME`,
/// no `$HOME` at all — and the run carries on exactly as it did before hog
/// created anything. A file that *was* named explicitly and is missing is an
/// error instead — silently ignoring `--config ./ci.toml` would render the
/// whole stream with the wrong settings and no sign of it.
///
/// The unknown-key warnings are written to `warnings` (stderr in the binary)
/// rather than returned, so a caller that only wants the model cannot forget to
/// print them. [`Loaded::unknown`] still holds them for tests.
///
/// `env` is passed in rather than read here for the reason spelled out in
/// [`discover`]: a test cannot put `$HOME` into the process environment,
/// because `std::env::set_var` is `unsafe` and this package denies
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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panicking test is a failing test")]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A directory that removes itself (`test-fixture-raii`).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("hog-ensure-{tag}-{}-{serial}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the temp directory is creatable");
            Self { path }
        }

        fn config(&self) -> PathBuf {
            self.path.join(discover::CONFIG_FILE)
        }

        fn env(&self) -> Env {
            Env {
                hog_config: None,
                home: Some(OsString::from(self.path.clone())),
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Runs [`ensure_default`] and returns what it wrote to stderr.
    fn ensure(explicit: Option<&Path>, env: &Env) -> String {
        let mut notes = Vec::new();
        ensure_default(explicit, env, &mut notes);
        String::from_utf8(notes).expect("the note is UTF-8")
    }

    #[test]
    fn a_fresh_home_gets_the_starter_and_one_line_about_it() {
        let dir = TempDir::new("fresh");

        let notes = ensure(None, &dir.env());

        assert_eq!(notes, format!("hog: created {}\n", dir.config().display()));
        assert_eq!(
            fs::read_to_string(dir.config()).expect("the file is there"),
            edit::STARTER
        );
    }

    /// The second run says nothing at all: the line is news, and news is only
    /// news once.
    #[test]
    fn a_second_run_is_silent_and_leaves_the_file_alone() {
        let dir = TempDir::new("again");
        ensure(None, &dir.env());
        fs::write(dir.config(), "exclude = [\"mine\"]\n").expect("the setup write succeeds");

        assert_eq!(ensure(None, &dir.env()), "");
        assert_eq!(
            fs::read_to_string(dir.config()).expect("the file is there"),
            "exclude = [\"mine\"]\n",
            "the user's own config was clobbered"
        );
    }

    /// Rule 1: a file the user named is theirs to create. Misspell it and the
    /// error from [`load`] is the answer, not a new empty config beside it.
    #[test]
    fn a_named_file_is_never_created() {
        let dir = TempDir::new("named");
        let named = dir.path.join("ci.toml");

        assert_eq!(ensure(Some(&named), &dir.env()), "");
        assert!(!named.exists(), "`--config` created a file");
        assert!(
            !dir.config().exists(),
            "a named file sent hog to the default path instead"
        );
    }

    /// No `$HOME` is not an error and not a place to guess at.
    #[test]
    fn nowhere_to_write_is_silent() {
        assert_eq!(ensure(None, &Env::default()), "");
    }

    /// hog creates a config file, not the home directory it belongs in.
    #[test]
    fn a_home_that_does_not_exist_is_not_created_either() {
        let dir = TempDir::new("nohome");
        let missing = dir.path.join("not-here");
        let env = Env {
            hog_config: None,
            home: Some(OsString::from(missing.clone())),
        };

        assert_eq!(ensure(None, &env), "");
        assert!(!missing.exists(), "hog invented a home directory");
    }

    /// Rule 2: a `$HOME` hog may not write is worth one line and nothing more —
    /// no panic, no error, and the caller carries on with the defaults.
    #[cfg(unix)]
    #[test]
    fn a_read_only_home_is_reported_and_survived() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new("readonly");
        let locked = dir.path.join("locked");
        fs::create_dir_all(&locked).expect("the directory is creatable");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500))
            .expect("the mode is settable");

        let env = Env {
            hog_config: None,
            home: Some(OsString::from(locked.clone())),
        };
        let notes = ensure(None, &env);

        // Put the mode back before any assertion can fail: `Drop` has to be
        // able to remove the tree.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))
            .expect("the mode is settable");

        assert!(notes.starts_with("hog: could not create "), "{notes}");
        assert_eq!(notes.lines().count(), 1, "more than one line: {notes}");
        assert!(!locked.join(discover::CONFIG_FILE).exists());
    }

    /// The whole justification for creating a file nobody asked for: the run
    /// that created it resolves exactly like the run that did not.
    #[test]
    fn the_created_file_resolves_to_the_built_in_defaults() {
        let dir = TempDir::new("same");

        let without = load_for_run(None, &dir.env(), &mut Vec::new()).expect("no config is fine");
        assert!(without.is_none(), "the fixture started with a config");
        let defaults = crate::settings::Settings::default();

        ensure(None, &dir.env());
        let with = load_for_run(None, &dir.env(), &mut Vec::new())
            .expect("the starter parses")
            .expect("the file is there");
        assert!(with.unknown.is_empty(), "the starter warns about itself");
        let created = crate::settings::from_config(&with.model).expect("the starter is honourable");

        // `Settings` carries no `PartialEq` — it is never compared in
        // production — so the whole resolved struct is compared as `Debug`,
        // which covers every field at once and will keep covering a field
        // added tomorrow.
        assert_eq!(
            format!("{created:?}"),
            format!("{defaults:?}"),
            "the starter resolves to something other than the built-in defaults"
        );
    }
}
