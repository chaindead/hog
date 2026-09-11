//! The one thing `hog` needs from build time: the version string `-v/--version`
//! prints (HLD §6).
//!
//! `CARGO_PKG_VERSION` cannot answer that question. It says `0.1.0` on every
//! build of this working copy — on the tagged commit CI released, on the commit
//! after it, and on a tree with uncommitted edits. What a bug report needs is
//! *which build this is*, so the chain is:
//!
//! 1. `$HOG_VERSION` — set by the release workflow from the tag (HLD §10), used
//!    verbatim, so `hog --version` prints `hog v1.0.0`;
//! 2. otherwise `git describe --tags --always --dirty`, rendered as
//!    `dev (a1b2c3d, dirty)` — a build nobody released, and it says so;
//! 3. otherwise the bare word `dev`.
//!
//! **A missing git is not a build failure.** Building from a release tarball,
//! from a vendored copy or inside a container with no `git` binary has to work,
//! so every step here degrades to the next instead of returning an error.
//!
//! `proj-build-rs-minimal`: no code generation, no crates, no network, three
//! lines of output. It stays that way — a build script runs on every consumer's
//! machine and is the one part of the build nobody reads before running.
//!
//! `println!` is not used, here or anywhere else in this package: `clippy.toml`
//! bans it because it panics when stdout is closed. Cargo reads this script's
//! stdout, so the ban is not really load-bearing at build time — but a build
//! script exempting itself from a rule the rest of the tree keeps is the kind of
//! exception that gets copied, and `emit` is one line.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Without this, a `HOG_VERSION` that changed between two builds would not
    // reach the second one: cargo caches the script's output and only reruns it
    // when something it was told to watch moves.
    emit("cargo::rerun-if-env-changed=HOG_VERSION");

    for path in watched_git_files() {
        // `rerun-if-changed` on a path that does not exist makes cargo rerun the
        // script on every build, so the caller filters to files it found.
        emit(&format!("cargo::rerun-if-changed={}", path.display()));
    }

    emit(&format!("cargo::rustc-env=HOG_VERSION={}", version()));
}

/// Writes one line of instructions to cargo.
///
/// Errors are dropped: if cargo has gone away there is nobody left to tell.
fn emit(line: &str) {
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{line}");
}

/// The version string to compile in.
fn version() -> String {
    one_line(&raw_version())
}

/// Drops every control character from the version string.
///
/// Two reasons, and the first one is the sharp one. Instructions to cargo are
/// **newline-delimited**, so a `$HOG_VERSION` containing a line break would be
/// read as a second instruction — and since Rust 1.77 cargo acts on the
/// `cargo::` directives it recognises, which is a build-time code path opened by
/// an environment variable. The second: this string is printed to a terminal by
/// `--version`, and hog escapes ESC everywhere else for exactly that reason
/// (HLD §8). Dropping rather than escaping is right here — a version is a name,
/// and none of these characters belong in one.
fn one_line(value: &str) -> String {
    value.chars().filter(|char| !char.is_control()).collect()
}

/// The version string before it is made safe to print and to hand to cargo.
fn raw_version() -> String {
    // `trim`, because a tag piped in through a workflow's `${{ }}` arrives with
    // whatever whitespace the YAML left on it, and an empty variable is how a
    // shell spells "unset".
    if let Ok(from_ci) = std::env::var("HOG_VERSION") {
        let from_ci = from_ci.trim();
        if !from_ci.is_empty() {
            return from_ci.to_owned();
        }
    }

    describe().map_or_else(|| "dev".to_owned(), |describe| render(&describe))
}

/// `git describe --tags --always --dirty`, or `None` for any reason at all.
///
/// Every failure is the same answer: no git binary, no repository, a repository
/// with no commits, a `git` that printed nothing. None of them are build errors
/// (see the module docs), and none of them are worth telling the user about —
/// the version string itself will say `dev`.
fn describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    Some(text.to_owned())
}

/// Turns `git describe` output into the version of a build that was never
/// released.
///
/// Reaching this function at all means `$HOG_VERSION` was unset, and HLD §10
/// makes the release workflow set it from the tag — so this is a local build
/// whatever the tree says, and the string leads with `dev` to keep it from being
/// quoted in a bug report as a release. What git found goes in the parentheses:
///
/// ```text
/// a1b2c3d-dirty      -> dev (a1b2c3d, dirty)   no tags yet, edited tree
/// a1b2c3d            -> dev (a1b2c3d)          no tags yet
/// v1.0.0             -> dev (v1.0.0)           sitting on the tag, built locally
/// v1.0.0-5-ga1b2c3d  -> dev (v1.0.0-5-ga1b2c3d)
/// ```
///
/// `-dirty` becomes `, dirty` because it is not part of the name of anything: it
/// is a second fact about the build, and the comma is what says so.
fn render(describe: &str) -> String {
    match describe.strip_suffix("-dirty") {
        Some(committed) => format!("dev ({committed}, dirty)"),
        None => format!("dev ({describe})"),
    }
}

/// The files whose contents decide what `git describe` will say next time.
///
/// `.git/HEAD` alone is not enough, and that is worth spelling out: it changes
/// when the branch changes, but committing on the *same* branch only moves the
/// ref it points at. Watching both means a new commit reruns this script.
///
/// What no file can cover is `--dirty`: it reflects the working tree, so the
/// first build after an edit still reports the previous state. Nothing short of
/// `rerun-if-changed=.` (which would rebuild the crate on every keystroke) would
/// fix that, and the trade is not worth it for one word in `--version`.
///
/// A missing or non-directory `.git` (a worktree, a submodule, an unpacked
/// tarball) yields an empty list rather than an error.
fn watched_git_files() -> Vec<PathBuf> {
    // Cargo runs this script with the package root as the working directory.
    let git_dir = Path::new(".git");
    if !git_dir.is_dir() {
        return Vec::new();
    }

    let head = git_dir.join("HEAD");
    let Ok(contents) = std::fs::read_to_string(&head) else {
        return Vec::new();
    };

    let mut watched = vec![head];

    // `ref: refs/heads/master` — a symbolic HEAD. A detached HEAD holds a commit
    // id instead, and then there is no second file to watch.
    if let Some(reference) = contents.trim().strip_prefix("ref:") {
        let reference = git_dir.join(reference.trim());
        if reference.is_file() {
            watched.push(reference);
        }
    }

    watched
}
