//! Saving the config file without ever leaving it half-written.
//!
//! Four steps, and each one is there because of a specific way the naive
//! version loses data (HLD §3):
//!
//! 1. **canonicalize the target.** `~/.hog.toml` is very often a symlink into
//!    a dotfiles repo. Renaming onto the link path would replace
//!    the link with a regular file, quietly detaching the user's config from
//!    the repo they track it in. Resolving first means the write lands on the
//!    real file and the link survives.
//! 2. **temp file in the same directory.** `rename` is only atomic within a
//!    filesystem, and `$TMPDIR` is routinely a different one — on macOS it
//!    always is. A sibling temp file is the only portable way to keep the
//!    rename atomic.
//! 3. **fsync before the rename.** Without it a crash can leave the renamed
//!    file present and empty: the directory entry is durable, its contents are
//!    not. This is the difference between "old config" and "no config".
//! 4. **rename over the target.** The only step readers can observe, and it is
//!    all-or-nothing: a concurrent `hog` either reads the old file or the new
//!    one, never a truncated one.
//!
//! # Why the temp file is opened `O_EXCL`
//!
//! `--config` points anywhere the user likes, including a world-writable
//! directory like `/tmp`. Creating the temp file with `create_new` means the
//! kernel refuses to follow a symlink someone else planted under the name we
//! are about to use, so hog cannot be tricked into writing a config through it.
//! The cost is a name collision with a temp file left by an earlier crash,
//! which is why [`create_temp`] tries a handful of names before giving up.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use toml_edit::DocumentMut;

/// Extension given to the temp file that precedes the rename.
///
/// The name is `<target>.<pid>.tmp` in the target's own directory: the pid
/// keeps two concurrent `hog config` runs from writing the same temp file, and
/// the dot prefix on the original name keeps it out of the way if a crash ever
/// leaves one behind.
pub const TMP_EXTENSION: &str = "tmp";

/// How many temp names to try before reporting that the directory is cluttered
/// with stale files. One is enough unless a previous run died between creating
/// the temp file and renaming it, *and* the pid has since been reused.
const TMP_ATTEMPTS: u32 = 8;

/// Mode for a config file hog creates. The file records a command that hog
/// executes, so it is not group- or world-writable.
#[cfg(unix)]
pub const NEW_FILE_MODE: u32 = 0o600;

/// Mode for a config directory hog creates, matching `~/.ssh` and `~/.gnupg`.
#[cfg(unix)]
pub const NEW_DIR_MODE: u32 = 0o700;

/// What [`create_new`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    /// The file did not exist and now holds the starter config.
    Written,
    /// A config was already there and was left exactly as it was.
    AlreadyExists,
}

/// Writes `document` back to `path`, atomically and with comments intact.
///
/// The rendered text is `document.to_string()`, which reproduces everything
/// `toml_edit` parsed — comments, blank lines, key alignment — with only the
/// edits applied.
pub fn save(path: &Path, document: &DocumentMut) -> anyhow::Result<()> {
    write_atomic(path, &document.to_string())
}

/// Replaces the file at `path` with `contents`, atomically.
///
/// The four steps in the module docs, in order. The parent directory is
/// expected to exist — [`ensure_parent_dir`] is a separate call so that
/// creating a directory for a `--config` path is a deliberate act of
/// `hog config`, not a side effect of any write.
///
/// Permissions follow the target: replacing an existing file keeps that file's
/// mode, and a new one gets [`NEW_FILE_MODE`]. Losing a user's `0600` because
/// we wrote through a fresh temp file would be a quiet downgrade.
///
/// The temp file is removed on every failure path, so a full disk or a denied
/// write does not leave `config.toml.4711.tmp` behind for the user to find.
pub fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    let target = resolve_target(path)?;
    let (temp, file) = create_temp(&target)?;

    let written = fill(file, contents, &temp, &target).and_then(|()| {
        fs::rename(&temp, &target)
            .with_context(|| format!("replacing {} with {}", target.display(), temp.display()))
    });
    if written.is_err() {
        // Best effort: the write already failed, and a failure to clean up
        // after it is not the error worth reporting.
        let _ = fs::remove_file(&temp);
    }
    written
}

/// Creates `path` with `contents` only if nothing is there yet.
///
/// Uses `OpenOptions::create_new`, so the "does it exist?" question is answered
/// by the kernel at the moment of creation rather than by an `exists()` call
/// that a second process can invalidate. That is not a formality here: hog
/// creates the default config by itself on a first run, so two `hog`s started
/// at the same moment on a fresh machine race for this exact file, and the
/// loser has to read what the winner wrote rather than write over it. An
/// existing config is never overwritten.
///
/// Missing parent directories are created first: the default `~/.hog.toml`
/// needs none, but `hog --config build/ci/hog.toml config edit` does.
pub fn create_new(path: &Path, contents: &str) -> anyhow::Result<Init> {
    ensure_parent_dir(path)?;
    let mut file = match open_exclusive(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::AlreadyExists => return Ok(Init::AlreadyExists),
        Err(err) => {
            return Err(err).with_context(|| format!("creating {}", path.display()));
        }
    };
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(Init::Written)
}

/// Resolves `path` through symlinks, without requiring it to exist.
///
/// An existing path is canonicalized outright. A missing one has its **parent**
/// canonicalized and the file name joined back on, which is what makes writing
/// a new file inside a symlinked home or dotfiles directory land in the right
/// place.
///
/// Note the deliberate asymmetry with `fs::canonicalize`, which fails outright
/// on a missing path: this has to work for a config that does not exist yet.
pub fn resolve_target(path: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(resolved) = path.canonicalize() {
        return Ok(resolved);
    }
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} does not name a file", path.display()))?;
    let parent = parent_dir(path);
    let resolved = parent.canonicalize().with_context(|| {
        format!(
            "the directory {} does not exist (create it, or name another file \
             with `hog --config PATH`)",
            parent.display()
        )
    })?;
    Ok(resolved.join(name))
}

/// Creates the parent directory chain of `path` if it is missing.
///
/// The directory a `--config` path names on a fresh machine. On unix it is
/// created `0700`, matching what `ssh` and `gnupg` do with their own config
/// directories.
pub fn ensure_parent_dir(path: &Path) -> anyhow::Result<()> {
    let parent = parent_dir(path);
    if parent.is_dir() {
        return Ok(());
    }
    create_dir_chain(parent).with_context(|| format!("creating the directory {}", parent.display()))
}

/// Writes `contents` into an already-created temp file and flushes it to disk.
///
/// Split out of [`write_atomic`] so that every failure between "the temp file
/// exists" and "the rename happened" travels through one `Result`, and the
/// cleanup is written once.
fn fill(mut file: File, contents: &str, temp: &Path, target: &Path) -> anyhow::Result<()> {
    inherit_mode(&file, target)
        .with_context(|| format!("setting the permissions of {}", temp.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", temp.display()))?;
    // Step 3: the bytes have to be on the disk *before* the directory entry
    // points at them, or a crash turns "old config" into "empty config".
    file.sync_all()
        .with_context(|| format!("flushing {} to disk", temp.display()))?;
    Ok(())
}

/// Creates a temp file next to `target`, failing rather than reusing a name
/// that is already taken.
fn create_temp(target: &Path) -> anyhow::Result<(PathBuf, File)> {
    let mut taken = None;
    for attempt in 0..TMP_ATTEMPTS {
        let candidate = temp_path(target, attempt)?;
        match open_exclusive(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => taken = Some(candidate),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("creating the temp file {}", candidate.display()));
            }
        }
    }
    bail!(
        "cannot write {}: {} is in the way — remove the leftover `.{TMP_EXTENSION}` files beside it",
        target.display(),
        taken.unwrap_or_else(|| target.to_path_buf()).display()
    )
}

/// `<target>.<pid>.tmp`, with a counter appended once the plain name is taken.
fn temp_path(target: &Path, attempt: u32) -> anyhow::Result<PathBuf> {
    let mut name = target
        .file_name()
        .ok_or_else(|| anyhow!("{} does not name a file", target.display()))?
        .to_os_string();
    let pid = std::process::id();
    if attempt == 0 {
        name.push(format!(".{pid}.{TMP_EXTENSION}"));
    } else {
        name.push(format!(".{pid}-{attempt}.{TMP_EXTENSION}"));
    }
    Ok(target.with_file_name(name))
}

/// The directory `path` lives in, with the empty parent of a bare file name
/// read as the current directory.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Opens a brand-new file for writing, refusing to touch an existing one.
#[cfg(unix)]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(NEW_FILE_MODE)
        .open(path)
}

#[cfg(not(unix))]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Gives the temp file the mode of the file it is about to replace, falling
/// back to [`NEW_FILE_MODE`] when there is nothing to inherit from.
#[cfg(unix)]
fn inherit_mode(file: &File, target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = fs::metadata(target).map_or(NEW_FILE_MODE, |metadata| {
        metadata.permissions().mode() & 0o7777
    });
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn inherit_mode(_file: &File, _target: &Path) -> std::io::Result<()> {
    Ok(())
}

/// `mkdir -p`, with the private mode on unix.
#[cfg(unix)]
fn create_dir_chain(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(NEW_DIR_MODE)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_chain(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::super::edit::{STARTER, append_exclude, starter_document};
    use super::*;

    /// A directory under `$TMPDIR` that deletes itself at the end of the test.
    ///
    /// Hand-rolled because `tempfile` is not a dependency of this package, and
    /// adding one for eight tests would be a poor trade. The name carries the
    /// pid and a counter, so tests running in parallel — in this process or
    /// beside it — never meet.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("hog-{tag}-{}-{serial}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the temp directory is creatable");
            // $TMPDIR is itself a symlink on macOS (/var -> /private/var), and
            // every path this module returns is canonical, so the test's own
            // expectations have to be canonical too.
            let path = path.canonicalize().expect("the temp directory resolves");
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }

        /// File names in the directory, sorted, so a leftover temp file shows.
        fn entries(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.path)
                .expect("the temp directory is readable")
                .map(|entry| {
                    entry
                        .expect("the entry is readable")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("the file is readable")
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;

        fs::metadata(path)
            .expect("the file exists")
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn a_write_creates_the_file_and_leaves_no_temp_behind() {
        let dir = TempDir::new("create");
        let target = dir.join("config.toml");

        write_atomic(&target, "exclude = []\n").expect("the write succeeds");

        assert_eq!(read(&target), "exclude = []\n");
        assert_eq!(dir.entries(), vec!["config.toml".to_owned()]);
    }

    #[test]
    fn a_write_replaces_the_previous_contents_whole() {
        let dir = TempDir::new("replace");
        let target = dir.join("config.toml");
        fs::write(&target, "exclude = [\"a\", \"b\", \"c\"]\n").expect("the setup write succeeds");

        write_atomic(&target, "exclude = []\n").expect("the write succeeds");

        assert_eq!(read(&target), "exclude = []\n");
        assert_eq!(dir.entries(), vec!["config.toml".to_owned()]);
    }

    /// The point of the whole module: a document read, edited and saved comes
    /// back with every comment in it.
    #[test]
    fn saving_a_document_keeps_its_comments() {
        let dir = TempDir::new("comments");
        let target = dir.join("config.toml");
        let document = starter_document().expect("the starter parses");

        save(&target, &document).expect("the save succeeds");

        assert_eq!(read(&target), STARTER);
    }

    /// The whole `hog config -e` path in one go: seed from the starter, add a
    /// field, save, and read the file back off the disk. This is the guarantee
    /// the wave is about — the user's documentation of their own config is not
    /// the price of editing it.
    #[test]
    fn an_edited_starter_reaches_the_disk_with_every_comment() {
        let dir = TempDir::new("roundtrip");
        let target = dir.join("config.toml");
        let mut document = starter_document().expect("the starter parses");
        append_exclude(&mut document, "trace_id").expect("the append succeeds");

        save(&target, &document).expect("the save succeeds");

        let written = read(&target);
        assert_eq!(
            written,
            STARTER.replace("exclude = []", "exclude = [\"trace_id\"]")
        );
        assert!(
            written.contains("# exclude = [\n#   \"serviceName\""),
            "the commented example survived the write: {written}"
        );
        written
            .parse::<DocumentMut>()
            .expect("the file that was written parses again");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_private_and_an_existing_mode_survives() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new("modes");

        let fresh = dir.join("fresh.toml");
        write_atomic(&fresh, "exclude = []\n").expect("the write succeeds");
        assert_eq!(mode_of(&fresh), NEW_FILE_MODE);

        let shared = dir.join("shared.toml");
        fs::write(&shared, "exclude = []\n").expect("the setup write succeeds");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o644))
            .expect("the setup chmod succeeds");
        write_atomic(&shared, "exclude = [\"a\"]\n").expect("the write succeeds");
        assert_eq!(mode_of(&shared), 0o644);
    }

    /// A config that is a symlink into a dotfiles repo has to stay a symlink:
    /// the rename lands on the file the link points at, not on the link.
    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlink_updates_the_target_and_keeps_the_link() {
        let dir = TempDir::new("symlink");
        let repo = dir.join("dotfiles");
        fs::create_dir_all(&repo).expect("the repo directory is creatable");
        let real = repo.join("hog.toml");
        fs::write(&real, "exclude = []\n").expect("the setup write succeeds");
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).expect("the symlink is creatable");

        write_atomic(&link, "exclude = [\"a\"]\n").expect("the write succeeds");

        assert!(
            fs::symlink_metadata(&link)
                .expect("the link is still there")
                .file_type()
                .is_symlink(),
            "the symlink was replaced by a regular file"
        );
        assert_eq!(read(&real), "exclude = [\"a\"]\n");
        assert_eq!(read(&link), "exclude = [\"a\"]\n");
    }

    /// A directory in the target's place fails the rename, which is the one
    /// failure that is easy to stage — and the temp file has to go with it.
    #[test]
    fn a_failed_write_cleans_up_after_itself() {
        let dir = TempDir::new("cleanup");
        let target = dir.join("config.toml");
        fs::create_dir_all(&target).expect("the directory is creatable");

        let error = write_atomic(&target, "exclude = []\n").expect_err("a directory is not a file");

        assert!(
            format!("{error:#}").contains("config.toml"),
            "the error names the path: {error:#}"
        );
        assert_eq!(dir.entries(), vec!["config.toml".to_owned()]);
    }

    #[test]
    fn resolve_target_follows_a_symlinked_directory_for_a_file_that_is_not_there_yet() {
        let dir = TempDir::new("resolve");
        let real = dir.join("real");
        fs::create_dir_all(&real).expect("the directory is creatable");

        let existing = real.join("here.toml");
        fs::write(&existing, "").expect("the setup write succeeds");
        assert_eq!(
            resolve_target(&existing).expect("an existing file resolves"),
            existing
        );

        #[cfg(unix)]
        {
            let link = dir.join("link");
            std::os::unix::fs::symlink(&real, &link).expect("the symlink is creatable");
            assert_eq!(
                resolve_target(&link.join("missing.toml")).expect("a missing file resolves"),
                real.join("missing.toml"),
                "the parent is resolved even though the file is not there"
            );
        }
    }

    #[test]
    fn a_missing_directory_is_named_in_the_error() {
        let dir = TempDir::new("nodir");
        let error = resolve_target(&dir.join("nope").join("config.toml"))
            .expect_err("a missing directory cannot be resolved");
        let message = format!("{error:#}");
        assert!(message.contains("nope"), "{message}");
        assert!(message.contains("--config"), "{message}");
    }

    #[test]
    fn create_new_writes_once_and_then_refuses() {
        let dir = TempDir::new("init");
        let target = dir.join("nested").join("config.toml");

        assert_eq!(
            create_new(&target, STARTER).expect("the first creation succeeds"),
            Init::Written
        );
        assert_eq!(read(&target), STARTER);

        assert_eq!(
            create_new(&target, "exclude = [\"clobbered\"]\n")
                .expect("the second creation succeeds"),
            Init::AlreadyExists
        );
        assert_eq!(read(&target), STARTER, "the existing config survived");
    }

    #[cfg(unix)]
    #[test]
    fn a_created_config_and_its_directory_are_private() {
        let dir = TempDir::new("private");
        let target = dir.join("hog").join("config.toml");

        create_new(&target, STARTER).expect("the creation succeeds");

        assert_eq!(mode_of(&target), NEW_FILE_MODE);
        assert_eq!(mode_of(&dir.join("hog")), NEW_DIR_MODE);
    }

    #[test]
    fn ensure_parent_dir_is_idempotent_and_accepts_a_bare_name() {
        let dir = TempDir::new("parent");
        let target = dir.join("a").join("b").join("config.toml");

        ensure_parent_dir(&target).expect("the chain is creatable");
        ensure_parent_dir(&target).expect("a second call is a no-op");
        assert!(dir.join("a").join("b").is_dir());

        // A bare file name means the current directory, which always exists.
        ensure_parent_dir(Path::new("config.toml")).expect("the current directory is there");
    }

    #[test]
    fn the_temp_file_is_a_sibling_of_the_target() {
        let target = Path::new("/etc/hog/config.toml");
        let temp = temp_path(target, 0).expect("the target names a file");

        assert_eq!(temp.parent(), target.parent());
        let name = temp
            .file_name()
            .expect("the temp path names a file")
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("config.toml."), "{name}");
        assert_eq!(
            Path::new(&name)
                .extension()
                .and_then(std::ffi::OsStr::to_str),
            Some("tmp"),
            "{name}"
        );
        assert!(
            name.contains(&std::process::id().to_string()),
            "the pid keeps concurrent runs apart: {name}"
        );
        assert_ne!(temp, temp_path(target, 1).expect("the second name differs"));
    }
}
