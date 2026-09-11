//! The v0.2 config layer, exercised through the real binary.
//!
//! The unit tests under `src/config/` drive the pure functions directly. What
//! this file owns is the promise a user can check for themselves with `cat`:
//!
//! * `hog config exclude add` gives the file back with **every comment, every blank line
//!   and the array's own style** intact, whichever of the seven shapes an
//!   `exclude` array is written in (HLD §3);
//! * the write is atomic and goes **through** a symlink rather than over it, so
//!   a config tracked in a dotfiles repo stays a link to that repo;
//! * the search order of HLD §3 is the one the process actually performs —
//!   flag, `$HOG_CONFIG`, `$XDG_CONFIG_HOME`, `~/.config`, built-in defaults —
//!   with no merging between files;
//! * the exclude layers of HLD §11.6: the file is the base, `-e` adds, `-E`
//!   resets, and `-E -e foo` is exactly `["foo"]`;
//! * an unknown key is a **warning with a line number** and the run carries on,
//!   while a file that does not parse is exit 1 with a line number and leaves
//!   the file on disk untouched.
//!
//! Every test owns a temporary directory and gets `$HOME`, `$XDG_CONFIG_HOME`
//! and the working directory pointed into it. Nothing here reads the real
//! `$HOME`: `std::env::set_var` is `unsafe` and this package denies unsafe
//! code, so the environment is built on the *parent* side, which is also why
//! `config::discover` takes an `Env` instead of reading one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use assert_cmd::cargo::CommandCargoExt as _;

// ==================================================================== fixture

/// A temporary tree with the three directories the search order cares about.
///
/// ```text
/// <root>/home            $HOME        → <root>/home/.config/hog/config.toml
/// <root>/xdg             $XDG_CONFIG_HOME → <root>/xdg/hog/config.toml
/// <root>/store           files named by --config / $HOG_CONFIG
/// ```
///
/// The working directory of every child is `<root>` itself, which is what makes
/// "hog never picks a config up from the CWD" observable rather than assumed.
struct Root {
    path: PathBuf,
}

impl Root {
    /// Creates the tree. Removed again by [`Drop`] (`test-fixture-raii`).
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hog-config-edit-{tag}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        for dir in ["home", "xdg", "store"] {
            fs::create_dir_all(path.join(dir)).expect("the temp tree is creatable");
        }
        Self { path }
    }

    /// `$XDG_CONFIG_HOME/hog/config.toml` — the file hog reads by default here.
    fn config(&self) -> PathBuf {
        self.path.join("xdg").join("hog").join("config.toml")
    }

    /// `$HOME/.config/hog/config.toml`, the fallback when `$XDG_CONFIG_HOME` is
    /// unset or unusable.
    fn home_config(&self) -> PathBuf {
        self.path
            .join("home")
            .join(".config")
            .join("hog")
            .join("config.toml")
    }

    /// A path inside the tree, e.g. `root.at("store/ci.toml")`.
    fn at(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }

    /// Writes a file, creating the directories above it.
    ///
    /// Associated rather than a method: the path is always absolute already —
    /// `at` or `config` built it — so there is nothing left for `self` to add,
    /// and a `&self` it never reads would be a claim that there is.
    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the directory is creatable");
        }
        fs::write(path, text).expect("the file is writable");
    }

    /// Writes `$XDG_CONFIG_HOME/hog/config.toml`.
    fn write_config(&self, text: &str) {
        let path = self.config();
        Self::write(&path, text);
    }

    /// Reads a file. Associated, for the same reason as [`Root::write`].
    fn read(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("{} is readable: {err}", path.display()))
    }

    /// Reads `$XDG_CONFIG_HOME/hog/config.toml`.
    fn read_config(&self) -> String {
        let path = self.config();
        Self::read(&path)
    }

    /// A `hog` invocation isolated from the developer's own environment.
    fn hog(&self) -> Run {
        let mut command = Command::cargo_bin("hog").expect("the binary is built by `cargo test`");
        command
            .current_dir(&self.path)
            .env_remove("HOG_CONFIG")
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("COLORTERM")
            .env("HOME", self.path.join("home"))
            .env("XDG_CONFIG_HOME", self.path.join("xdg"));
        Run {
            command,
            input: Vec::new(),
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// One child process, configured fluently.
struct Run {
    command: Command,
    input: Vec<u8>,
}

impl Run {
    fn args(mut self, args: &[&str]) -> Self {
        self.command.args(args);
        self
    }

    fn env(mut self, key: &str, value: &Path) -> Self {
        self.command.env(key, value);
        self
    }

    fn without(mut self, key: &str) -> Self {
        self.command.env_remove(key);
        self
    }

    fn stdin(mut self, input: &[u8]) -> Self {
        self.input = input.to_vec();
        self
    }

    fn output(mut self) -> Out {
        use std::io::Write as _;

        let mut child = self
            .command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("hog must start");
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(&self.input)
            .expect("hog must accept the input");
        let out = child.wait_with_output().expect("hog must finish");

        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().expect("hog must not die from a signal"),
        }
    }

    /// Runs and asserts exit code 0.
    fn ok(self) -> Out {
        self.output().exited(0)
    }

    /// Runs and asserts exit code 1 — the runtime-failure code of HLD §6.
    fn fails(self) -> Out {
        self.output().exited(1)
    }
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Out {
    fn exited(self, code: i32) -> Self {
        assert_eq!(
            self.code, code,
            "unexpected exit code\nstdout: {}\nstderr: {}",
            self.stdout, self.stderr
        );
        self
    }

    /// The single line `hog config path` prints.
    fn line(&self) -> &str {
        self.stdout.trim_end_matches('\n')
    }

    fn stderr_has(&self, needle: &str) -> &Self {
        assert!(
            self.stderr.contains(needle),
            "stderr does not mention {needle:?}:\n{}",
            self.stderr
        );
        self
    }
}

/// The key names still visible on a rendered line, in printed order.
fn visible(rendered: &str) -> Vec<&str> {
    rendered
        .split_whitespace()
        .filter_map(|word| word.split_once('='))
        .map(|(key, _)| key)
        .collect()
}

/// Every comment line of a config file, trimmed, in file order.
///
/// The generic guard against the failure this whole wave exists to prevent: an
/// edit that rewrites the file from a parsed model would come back with this
/// list empty, whatever else it got right.
fn comments(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with('#'))
        .collect()
}

/// The keys and `[table]` headers of a config file, in the order they are
/// written.
///
/// Lines that start with whitespace are the inside of a multi-line array and
/// carry no key, so they are skipped; that is also what keeps an entry like
/// `"grpc.code",` from being mistaken for one.
fn structure(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            if line.starts_with('[') {
                Some(line)
            } else {
                line.split_once('=').map(|(key, _)| key.trim())
            }
        })
        .collect()
}

/// The `.tmp` files left behind in `dir` — always expected to be none.
fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("the directory is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "tmp"))
        .collect()
}

// ================================================================ array style

/// Every shape an `exclude` array is written in, and what one `hog config exclude add`
/// does to it.
///
/// The rule these rows encode (HLD §3, `config::edit`): a new entry copies the
/// style of the entry it follows and takes over the padding that used to sit
/// before the `]`. Nothing else on the line moves — not the indent, not the
/// trailing comma, not a comment somebody wrote beside an entry.
const STYLES: &[(&str, &str, &str)] = &[
    (
        "an empty array grows the single-line form",
        "exclude = []\n",
        "exclude = [\"new\"]\n",
    ),
    (
        "a single-line array keeps its separator",
        "exclude = [\"a\", \"b\"]\n",
        "exclude = [\"a\", \"b\", \"new\"]\n",
    ),
    (
        "a tight single-line array stays tight",
        "exclude = [\"a\",\"b\"]\n",
        "exclude = [\"a\",\"b\",\"new\"]\n",
    ),
    (
        // The one entry's prefix is the array's opening padding, not a
        // separator, so there is nothing to copy and `, ` is the default.
        "an array of one grows the default separator",
        "exclude = [\"a\"]\n",
        "exclude = [\"a\", \"new\"]\n",
    ),
    (
        "padding inside the brackets stays outside the new entry",
        "exclude = [ \"a\" ]\n",
        "exclude = [ \"a\", \"new\" ]\n",
    ),
    (
        "padding survives a longer single-line array too",
        "exclude = [ \"a\", \"b\" ]\n",
        "exclude = [ \"a\", \"b\", \"new\" ]\n",
    ),
    (
        // The trailing comma belongs to the array, not to its last entry.
        "a single-line trailing comma survives",
        "exclude = [\"a\",]\n",
        "exclude = [\"a\", \"new\",]\n",
    ),
    (
        "a multi-line array gains a line, indent and trailing comma included",
        concat!("exclude = [\n", "  \"a\",\n", "  \"b\",\n", "]\n"),
        concat!(
            "exclude = [\n",
            "  \"a\",\n",
            "  \"b\",\n",
            "  \"new\",\n",
            "]\n"
        ),
    ),
    (
        "a multi-line array without a trailing comma does not grow one",
        concat!("exclude = [\n", "  \"a\",\n", "  \"b\"\n", "]\n"),
        concat!(
            "exclude = [\n",
            "  \"a\",\n",
            "  \"b\",\n",
            "  \"new\"\n",
            "]\n"
        ),
    ),
    (
        "the indent is whatever the file already uses",
        concat!("exclude = [\n", "    \"a\",\n", "]\n"),
        concat!("exclude = [\n", "    \"a\",\n", "    \"new\",\n", "]\n"),
    ),
    (
        "a tab indent is copied as a tab",
        concat!("exclude = [\n", "\t\"a\",\n", "]\n"),
        concat!("exclude = [\n", "\t\"a\",\n", "\t\"new\",\n", "]\n"),
    ),
    (
        // `# loud` lives in the *prefix* of `"b"`, so a naive copy of the
        // neighbouring decor would print it a second time.
        "a comment beside an entry is not duplicated onto the new one",
        concat!("exclude = [\n", "  \"a\", # loud\n", "  \"b\",\n", "]\n"),
        concat!(
            "exclude = [\n",
            "  \"a\", # loud\n",
            "  \"b\",\n",
            "  \"new\",\n",
            "]\n"
        ),
    ),
    (
        // The comma has to be printed before the comment, or it lands inside
        // it and the file stops parsing.
        "a comment closing the last line keeps the comma out of itself",
        concat!("exclude = [\n", "  \"a\"  # note\n", "]\n"),
        concat!("exclude = [\n", "  \"a\",  # note\n", "  \"new\"\n", "]\n"),
    ),
    (
        "a comment on its own line before the bracket stays there",
        concat!("exclude = [\n", "  \"a\",\n", "  # why\n", "]\n"),
        concat!(
            "exclude = [\n",
            "  \"a\",\n",
            "  \"new\",\n",
            "  # why\n",
            "]\n"
        ),
    ),
    (
        "a blank line inside the array is left where it is",
        concat!("exclude = [\n", "  \"a\",\n", "\n", "  \"b\",\n", "]\n"),
        concat!(
            "exclude = [\n",
            "  \"a\",\n",
            "\n",
            "  \"b\",\n",
            "  \"new\",\n",
            "]\n"
        ),
    ),
    (
        // Nothing to copy an indent from, so the default two spaces of the
        // starter file are used — but the array stays multi-line.
        "an empty multi-line array stays multi-line",
        concat!("exclude = [\n", "]\n"),
        concat!("exclude = [\n", "  \"new\"\n", "]\n"),
    ),
];

#[test]
fn every_array_style_survives_an_append() {
    for (name, before, after) in STYLES {
        let root = Root::new("style");
        root.write_config(before);

        let out = root.hog().args(&["config", "exclude", "add", "new"]).ok();
        assert_eq!(out.stdout, "added `new`\n", "{name}");
        assert_eq!(root.read_config(), *after, "{name}");
    }
}

/// The mirror image: `hog config exclude rm` gives the file back byte for byte.
///
/// This is the strongest statement available about formatting, and it needs no
/// expected text at all — whatever the append did to the file, the matching
/// removal has to undo it exactly, comments and all.
#[test]
fn an_append_and_a_removal_cancel_out_byte_for_byte() {
    for (name, before, _) in STYLES {
        // One documented exception, with its own test below: an array emptied
        // by a removal collapses back to `[]`.
        if before.starts_with("exclude = [\n]") {
            continue;
        }

        let root = Root::new("roundtrip");
        root.write_config(before);
        root.hog().args(&["config", "exclude", "add", "new"]).ok();
        root.hog().args(&["config", "exclude", "rm", "new"]).ok();

        assert_eq!(root.read_config(), *before, "{name}");
    }
}

/// Removing the last entry of an array whose *previous* entry ends in a comment
/// keeps the comment **on the line it was written on**.
///
/// ```text
/// exclude = [        exclude = [
///   "a",  # note  →    "a"  # note
///   "new"            ]
/// ]
/// ```
///
/// Both halves of `remove_exclude` fire on the same gap: the removed entry's
/// prefix carries `# note`, which goes to the array's trailing decor, and its
/// suffix carries the newline before `]`. Handing both on printed the newline
/// first and pushed the note a line below where it was — the file still parsed
/// and the note was still there, but it stopped reading as being about `"a"`,
/// which is a spurious diff in a dotfiles repo. The transfer of the closing
/// padding is now skipped when the carry already put a comment in front of the
/// `]`, since that carry ends in a newline itself.
///
/// This case is also in `STYLES`, so the byte-for-byte round trip covers it;
/// the test stays because it is the one that names the failure.
#[test]
fn a_comment_closing_the_last_line_survives_a_removal() {
    /// What the file has to come back as.
    const IDEAL: &str = concat!("exclude = [\n", "  \"a\"  # note\n", "]\n");

    let root = Root::new("last-comment");
    root.write_config(IDEAL);
    root.hog().args(&["config", "exclude", "add", "new"]).ok();
    root.hog().args(&["config", "exclude", "rm", "new"]).ok();

    assert_eq!(
        root.read_config(),
        IDEAL,
        "the note did not come back on its own line"
    );
    // The file also has to mean what it did before.
    let out = root.hog().args(&["config"]).ok();
    assert!(out.stdout.contains("exclude:   a\n"), "{}", out.stdout);
    assert_eq!(
        out.stderr, "",
        "the round trip left a file hog complains about"
    );
}

/// `exclude = [\n]` is the exception, and it is a deliberate one: an array left
/// empty collapses to `[]` instead of keeping a bracket on a line of its own.
#[test]
fn an_emptied_array_collapses_to_the_short_form() {
    let root = Root::new("collapse");
    root.write_config("exclude = [\n]\n");

    root.hog().args(&["config", "exclude", "add", "new"]).ok();
    root.hog().args(&["config", "exclude", "rm", "new"]).ok();

    assert_eq!(root.read_config(), "exclude = []\n");
}

/// A file with no `exclude` key at all gets one — and it has to land **above**
/// the first `[table]` header, or TOML would read it as `output.exclude`.
#[test]
fn a_missing_key_is_created_above_the_first_table_header() {
    let root = Root::new("missing-key");
    let before = concat!(
        "# mine\n",
        "command = \"echo {0}\"\n",
        "\n",
        "[output]\n",
        "color = \"never\"\n",
    );
    root.write_config(before);

    root.hog().args(&["config", "exclude", "add", "new"]).ok();

    let after = root.read_config();
    assert_eq!(
        after,
        concat!(
            "# mine\n",
            "command = \"echo {0}\"\n",
            "exclude = [\"new\"]\n",
            "\n",
            "[output]\n",
            "color = \"never\"\n",
        )
    );
    // Read back by hog itself: the key is top-level, not `output.exclude`.
    let out = root.hog().args(&["config"]).ok();
    assert!(
        out.stdout.contains("exclude:   new"),
        "the new key was not read back as a top-level exclude:\n{}",
        out.stdout
    );
    assert_eq!(out.stderr, "", "the edited file warned about itself");
}

/// No file at all is the ordinary first command on a new machine: it seeds the
/// commented starter with the field already in it, rather than a one-line file
/// that explains nothing.
#[test]
fn a_missing_file_is_seeded_with_the_commented_starter() {
    let root = Root::new("missing-file");
    assert!(!root.config().exists());

    root.hog()
        .args(&["config", "exclude", "add", "trace_id"])
        .ok();

    let written = root.read_config();
    assert!(
        written.contains("exclude = [\"trace_id\"]"),
        "the active array did not get the field:\n{written}"
    );
    assert!(
        comments(&written).len() > 20,
        "the seeded file is the documented starter, not a one-liner:\n{written}"
    );
    // And it is the same file `config init` would have written, bar the array.
    let other = Root::new("missing-file-init");
    other.hog().args(&["config", "init"]).ok();
    assert_eq!(
        written,
        other.read_config().replace("[]", "[\"trace_id\"]"),
        "seeding drifted from `hog config init`"
    );
}

/// The starter's own shape: an active `exclude = []` with a commented-out
/// example block directly underneath. The edit must find the live array and
/// leave the commented one exactly as it is — including the `#` prefixes, which
/// a `toml` round-trip would delete outright.
#[test]
fn a_commented_out_block_beside_the_live_array_is_untouched() {
    let root = Root::new("commented-block");
    let before = concat!(
        "# what this list is for\n",
        "exclude = []\n",
        "# exclude = [\n",
        "#   \"serviceName\", \"trace_id\",\n",
        "#   \"grpc.code\",\n",
        "# ]\n",
        "\n",
        "[output]\n",
        "color = \"auto\"   # trailing note\n",
    );
    root.write_config(before);

    root.hog()
        .args(&["config", "exclude", "add", "grpc.method"])
        .ok();

    let after = root.read_config();
    assert_eq!(after, before.replace("[]", "[\"grpc.method\"]"));
    assert_eq!(
        comments(&after),
        comments(before),
        "a comment was rewritten, moved or lost"
    );
    // The commented block is still a comment, not a second array hog reads.
    let out = root.hog().args(&["config"]).ok();
    assert!(
        out.stdout.contains("exclude:   grpc.method\n"),
        "the commented example leaked into the effective list:\n{}",
        out.stdout
    );
}

// ================================================================ preservation

/// The headline guarantee of the wave, on a file that has something of
/// everything in it: after two edits, the only thing that changed is the
/// `exclude` array.
#[test]
fn foreign_comments_and_key_order_survive_an_edit() {
    let root = Root::new("preserve");
    let before = concat!(
        "# a header comment\n",
        "# spanning two lines\n",
        "\n",
        "exclude = [\"keep\", \"drop\"]\n",
        "\n",
        "# why this command and not another\n",
        "command = \"ssh -tt {0} 'docker logs -f {1}'\"   # inline, after the value\n",
        "\n",
        "[fields]\n",
        "# candidates, first one present wins\n",
        "ts    = [\"ts\", \"time\"]\n",
        "level = [\"level\", \"severity\"]\n",
        "msg   = [\"msg\", \"message\"]\n",
        "\n",
        "[output]\n",
        "time_format = \"%H:%M:%S\"   # strftime\n",
        "time_zone   = \"utc\"\n",
        "sort_keys   = false\n",
        "\n",
        "# pino sends numbers\n",
        "[output.levels]\n",
        "\"30\" = \"info\"\n",
        "\"50\" = \"error\"\n",
    );
    root.write_config(before);

    let out = root.hog().args(&["config", "exclude", "rm", "drop"]).ok();
    assert_eq!(out.stdout, "removed `drop`\n");
    assert_eq!(out.stderr, "", "a hand-written valid file warned");

    let out = root.hog().args(&["config", "exclude", "add", "added"]).ok();
    assert_eq!(out.stdout, "added `added`\n");
    assert_eq!(out.stderr, "");

    let after = root.read_config();
    assert_eq!(
        after,
        before.replace("[\"keep\", \"drop\"]", "[\"keep\", \"added\"]"),
        "something other than the exclude array moved"
    );
    assert_eq!(comments(&after), comments(before));
    assert_eq!(structure(&after), structure(before), "key order changed");

    // And the file still means what it said: every other key is still in force.
    let summary = root.hog().args(&["config"]).ok();
    for needle in [
        // Sorted, because that is the shape the resolved set has; the file
        // itself keeps its own order, as the byte comparison above proved.
        "exclude:   added, keep",
        "ts:        ts, time",
        "time:      %H:%M:%S, utc",
        "sort_keys: false",
        "levels:    30 -> info, 50 -> error",
    ] {
        assert!(
            summary.stdout.contains(needle),
            "missing {needle:?}:\n{}",
            summary.stdout
        );
    }
}

/// Nothing to do means nothing written: a repeated `-e` must not rewrite the
/// file, so its mtime stays honest and a dotfiles repo shows no diff.
#[test]
fn an_edit_that_changes_nothing_does_not_touch_the_file() {
    let root = Root::new("no-op");
    root.write_config("exclude = [\"a\"]\n");
    let before = fs::metadata(root.config())
        .and_then(|meta| meta.modified())
        .expect("the modification time is readable");

    let out = root.hog().args(&["config", "exclude", "rm", "b"]).ok();
    assert_eq!(out.stdout, "`b` was not excluded\n");

    let out = root.hog().args(&["config", "exclude", "add", "a"]).ok();
    assert_eq!(out.stdout, "`a` was already excluded\n");

    let after = fs::metadata(root.config())
        .and_then(|meta| meta.modified())
        .expect("the modification time is readable");
    assert_eq!(before, after, "a no-op edit rewrote the file");
    assert_eq!(root.read_config(), "exclude = [\"a\"]\n");
}

// =============================================================== atomic write

/// HLD §3, step 1: `~/.config/hog/config.toml` is very often a symlink into a
/// dotfiles repo, and renaming onto the link path would replace the link with a
/// regular file — silently detaching the config from the repo it is tracked in.
#[test]
fn writing_through_a_symlinked_file_keeps_the_symlink() {
    let root = Root::new("symlink-file");
    let real = root.at("store/dotfiles/config.toml");
    Root::write(&real, "# tracked in git\nexclude = [\"a\"]\n");

    let link = root.config();
    fs::create_dir_all(link.parent().expect("the link has a parent"))
        .expect("the directory is creatable");
    std::os::unix::fs::symlink(&real, &link).expect("the symlink is creatable");

    root.hog().args(&["config", "exclude", "add", "new"]).ok();

    let meta = fs::symlink_metadata(&link).expect("the link is still there");
    assert!(
        meta.file_type().is_symlink(),
        "the write replaced the symlink with a regular file"
    );
    assert_eq!(
        fs::read_link(&link).expect("the link is readable"),
        real,
        "the link now points somewhere else"
    );
    assert_eq!(
        Root::read(&real),
        "# tracked in git\nexclude = [\"a\", \"new\"]\n",
        "the edit did not reach the file the link points at"
    );
    assert!(
        leftovers(&root.at("store/dotfiles")).is_empty()
            && leftovers(link.parent().expect("the link has a parent")).is_empty(),
        "a temp file was left behind"
    );
}

/// The same for a symlinked *directory* — `~/.config/hog -> ~/dotfiles/hog`,
/// which is how a whole config tree is usually tracked.
#[test]
fn writing_into_a_symlinked_directory_keeps_the_symlink() {
    let root = Root::new("symlink-dir");
    let real = root.at("store/dotfiles/hog/config.toml");
    Root::write(&real, "exclude = []\n");

    let link = root.at("xdg/hog");
    std::os::unix::fs::symlink(root.at("store/dotfiles/hog"), &link)
        .expect("the symlink is creatable");

    root.hog().args(&["config", "exclude", "add", "new"]).ok();

    assert!(
        fs::symlink_metadata(&link)
            .expect("the link is still there")
            .file_type()
            .is_symlink(),
        "the directory symlink was replaced"
    );
    assert_eq!(Root::read(&real), "exclude = [\"new\"]\n");
}

/// The temp file is a means, not an artefact: it is created beside the target
/// (a rename is only atomic within one filesystem) and it is gone afterwards.
#[test]
fn the_write_leaves_no_temp_file_behind() {
    let root = Root::new("temp");
    root.write_config("exclude = []\n");

    root.hog().args(&["config", "exclude", "add", "new"]).ok();

    let dir = root.config();
    let dir = dir.parent().expect("the config has a parent");
    let names: Vec<String> = fs::read_dir(dir)
        .expect("the directory is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["config.toml"],
        "the directory holds more than the config"
    );
}

/// Replacing a file through a fresh temp file must not quietly hand the user a
/// different mode than the one they chose.
#[test]
fn an_existing_file_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = Root::new("mode");
    root.write_config("exclude = []\n");
    fs::set_permissions(root.config(), fs::Permissions::from_mode(0o640))
        .expect("the mode is settable");

    root.hog().args(&["config", "exclude", "add", "new"]).ok();

    let mode = fs::metadata(root.config())
        .expect("the config is there")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o640, "the mode changed under the user");
}

/// A file hog creates records a command hog executes, so it is not readable by
/// the rest of the machine.
#[test]
fn a_created_file_is_private() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = Root::new("init-mode");
    root.hog().args(&["config", "init"]).ok();

    let mode = fs::metadata(root.config())
        .expect("`config init` wrote the file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

// ================================================================== discovery

/// The whole search order of HLD §3, one row at a time, proved by which file's
/// exclude list actually took effect.
///
/// Each candidate hides a different key, so the rendered line names the winner.
/// There is no merging anywhere in this table: the first hit wins outright.
#[test]
fn the_search_order_table() {
    const LINE: &[u8] = br#"{"flag":1,"env":2,"xdg":3,"home":4}"#;
    let render = ["--color", "never"];

    // 1. --config wins over everything, including a $HOG_CONFIG that is set.
    let root = Root::new("search");
    let flag = root.at("store/flag.toml");
    let env = root.at("store/env.toml");
    Root::write(&flag, "exclude = [\"flag\"]\n");
    Root::write(&env, "exclude = [\"env\"]\n");
    root.write_config("exclude = [\"xdg\"]\n");
    Root::write(&root.home_config(), "exclude = [\"home\"]\n");

    let out = root
        .hog()
        .args(&render)
        .args(&["--config", &flag.display().to_string()])
        .env("HOG_CONFIG", &env)
        .stdin(LINE)
        .ok();
    assert_eq!(visible(&out.stdout), ["env", "home", "xdg"]);

    // 2. $HOG_CONFIG, when no flag names a file.
    let out = root
        .hog()
        .args(&render)
        .env("HOG_CONFIG", &env)
        .stdin(LINE)
        .ok();
    assert_eq!(visible(&out.stdout), ["flag", "home", "xdg"]);

    // 3. $XDG_CONFIG_HOME/hog/config.toml.
    let out = root.hog().args(&render).stdin(LINE).ok();
    assert_eq!(visible(&out.stdout), ["env", "flag", "home"]);

    // 4. ~/.config/hog/config.toml, when $XDG_CONFIG_HOME is unset.
    let out = root
        .hog()
        .args(&render)
        .without("XDG_CONFIG_HOME")
        .stdin(LINE)
        .ok();
    assert_eq!(visible(&out.stdout), ["env", "flag", "xdg"]);

    // 4b. …and when it is set to something the XDG spec says to ignore.
    for unusable in ["", "relative/config"] {
        let out = root
            .hog()
            .args(&render)
            .env("XDG_CONFIG_HOME", Path::new(unusable))
            .stdin(LINE)
            .ok();
        assert_eq!(
            visible(&out.stdout),
            ["env", "flag", "xdg"],
            "XDG_CONFIG_HOME={unusable:?} should have been ignored"
        );
    }

    // 5. Nothing anywhere: the built-in defaults, silently.
    let bare = Root::new("search-bare");
    let out = bare.hog().args(&render).stdin(LINE).ok();
    assert_eq!(visible(&out.stdout), ["env", "flag", "home", "xdg"]);
    assert_eq!(out.stderr, "", "a missing config is not worth a word");
}

/// `hog config path` answers the same question the table above answers, one
/// layer at a time — and says which knob produced the answer.
#[test]
fn the_path_and_its_source_follow_the_same_order() {
    let root = Root::new("path");
    let named = root.at("store/ci.toml");
    Root::write(&named, "exclude = []\n");

    let out = root
        .hog()
        .args(&["config", "path", "--config", &named.display().to_string()])
        .ok();
    assert_eq!(out.line(), named.display().to_string());

    let out = root
        .hog()
        .args(&["config", "path"])
        .env("HOG_CONFIG", &named)
        .ok();
    assert_eq!(out.line(), named.display().to_string());
    // `--config` and `$HOG_CONFIG` arrive in one clap field and are told apart
    // by comparing the value, so only the wording of `source:` changes.
    let summary = root.hog().args(&["config"]).env("HOG_CONFIG", &named).ok();
    assert!(
        summary.stdout.contains("source:    $HOG_CONFIG"),
        "{}",
        summary.stdout
    );

    let out = root.hog().args(&["config", "path"]).ok();
    assert_eq!(out.line(), root.config().display().to_string());

    let out = root
        .hog()
        .args(&["config", "path"])
        .without("XDG_CONFIG_HOME")
        .ok();
    assert_eq!(out.line(), root.home_config().display().to_string());
}

/// A file the user **named** and that is not there is a failure, not a silent
/// fall-through to the defaults: the alternative is rendering the whole stream
/// with settings nobody asked for, and no sign of it (HLD §3).
#[test]
fn a_named_file_that_is_missing_is_an_error_that_names_the_knob() {
    let root = Root::new("missing-named");
    root.write_config("exclude = [\"xdg\"]\n");
    let nowhere = root.at("store/nope.toml");

    let out = root
        .hog()
        .args(&["--config", &nowhere.display().to_string()])
        .stdin(b"{\"msg\":\"hi\"}")
        .fails();
    out.stderr_has("--config").stderr_has("does not exist");
    assert!(out.stdout.is_empty(), "the stream was rendered anyway");

    let out = root.hog().env("HOG_CONFIG", &nowhere).stdin(b"{}").fails();
    out.stderr_has("$HOG_CONFIG")
        .stderr_has(&nowhere.display().to_string());

    // The *guessed* path is the opposite case: missing is the normal state.
    let bare = Root::new("missing-guessed");
    let out = bare.hog().stdin(b"{\"msg\":\"hi\"}").ok();
    assert_eq!(out.stdout, "hi\n");
    assert_eq!(out.stderr, "");
}

/// With neither `$XDG_CONFIG_HOME` nor `$HOME` there is no path to guess. A
/// plain run carries on with the built-in defaults; `hog config`, whose every
/// branch is about a specific file, has to say so instead of printing an empty
/// line.
#[test]
fn no_config_home_at_all_still_renders_but_cannot_name_a_file() {
    let root = Root::new("nohome");

    let out = root
        .hog()
        .without("XDG_CONFIG_HOME")
        .without("HOME")
        .stdin(b"{\"msg\":\"hi\",\"a\":1}")
        .ok();
    assert_eq!(out.stdout, "hi a=1\n");
    assert_eq!(out.stderr, "");

    let out = root
        .hog()
        .args(&["config", "path"])
        .without("XDG_CONFIG_HOME")
        .without("HOME")
        .fails();
    out.stderr_has("XDG_CONFIG_HOME").stderr_has("HOME");
    assert!(out.stdout.is_empty(), "an empty path was printed anyway");
}

// ============================================================ exclude layers

/// The table of HLD §11.6, end to end: the file is the base list, `-e` adds to
/// it, `-E` drops it, and `-E -e foo` is exactly `["foo"]`.
#[test]
fn the_exclude_layers() {
    const LINE: &[u8] = br#"{"a":1,"b":2,"c":3,"d":4}"#;

    let root = Root::new("layers");
    root.write_config("exclude = [\"a\", \"b\"]\n");

    let render = |args: &[&str]| -> Vec<String> {
        let mut all = vec!["--color", "never"];
        all.extend_from_slice(args);
        let out = root.hog().args(&all).stdin(LINE).ok();
        visible(&out.stdout)
            .into_iter()
            .map(str::to_owned)
            .collect()
    };

    assert_eq!(render(&[]), ["c", "d"], "the file's list alone");
    assert_eq!(render(&["-e", "c"]), ["d"], "`-e` adds to the file's list");
    assert_eq!(
        render(&["-e", "c,d"]),
        Vec::<String>::new(),
        "`-e` is comma-separated as well as repeatable"
    );
    assert_eq!(
        render(&["-e", "c", "-e", "d"]),
        Vec::<String>::new(),
        "`-e` is repeatable"
    );
    assert_eq!(
        render(&["-E"]),
        ["a", "b", "c", "d"],
        "`-E` drops the file's list"
    );
    assert_eq!(
        render(&["-E", "-e", "a"]),
        ["b", "c", "d"],
        "`-E -e a` is exactly [\"a\"]"
    );
    assert_eq!(
        render(&["-e", "a"]),
        ["c", "d"],
        "a field already in the file is not a second exclusion"
    );
}

/// The persistent half of the same story: what `hog config exclude add` writes is what
/// the next run starts from.
#[test]
fn the_persistent_list_is_the_base_of_the_next_run() {
    const LINE: &[u8] = br#"{"a":1,"b":2,"c":3}"#;

    let root = Root::new("persist");
    root.hog().args(&["config", "exclude", "add", "a,b"]).ok();

    let out = root.hog().args(&["--color", "never"]).stdin(LINE).ok();
    assert_eq!(visible(&out.stdout), ["c"]);

    root.hog().args(&["config", "exclude", "rm", "a"]).ok();
    let out = root.hog().args(&["--color", "never"]).stdin(LINE).ok();
    assert_eq!(visible(&out.stdout), ["a", "c"]);
}

// ================================================================ diagnostics

/// HLD §7.2: an unknown key is a **warning with a line number**, never an
/// error. Denying it would break a config written for a newer hog, and ignoring
/// it would swallow a typo — the line number is what buys both.
#[test]
fn unknown_keys_warn_with_the_line_they_are_written_on() {
    let root = Root::new("unknown");
    root.write_config(concat!(
        "exclude = []\n",      // 1
        "typo = true\n",       // 2 — unknown, top level
        "\n",                  // 3
        "[fields]\n",          // 4
        "ts = [\"ts\"]\n",     // 5
        "msg2 = [\"m\"]\n",    // 6 — unknown, inside a known table
        "\n",                  // 7
        "[outpout]\n",         // 8 — a mistyped table
        "color = \"never\"\n", // 9 — must NOT warn a second time
        "\n",                  // 10
        "[output.levels]\n",   // 11
        "\"30\" = \"info\"\n", // 12 — user data, never a warning
    ));

    let out = root
        .hog()
        .args(&["--color", "never"])
        .stdin(b"{\"msg\":\"hi\"}")
        .ok();

    let path = root.config().display().to_string();
    assert_eq!(
        out.stderr,
        format!(
            "warning: {path}:2: unknown key `typo`\n\
             warning: {path}:6: unknown key `fields.msg2`\n\
             warning: {path}:8: unknown key `outpout`\n"
        )
    );
    assert_eq!(out.stdout, "hi\n", "the run carried on");
}

/// The warnings belong on stderr even when stdout is a single line meant for
/// `$(hog config path)`.
#[test]
fn warnings_never_land_in_the_captured_path() {
    let root = Root::new("warn-path");
    root.write_config("typo = 1\n");

    let out = root.hog().args(&["config", "path"]).ok();
    assert_eq!(out.stdout, format!("{}\n", root.config().display()));
    assert_eq!(out.stderr, "");

    // …and `hog config` proper reports them, still on stderr.
    let out = root.hog().args(&["config"]).ok();
    out.stderr_has("unknown key `typo`");
    assert!(!out.stdout.contains("warning"), "{}", out.stdout);
}

/// A file that does not parse is exit 1 with the line the parser stopped on,
/// in the `path:line: message` shape an editor already knows how to jump to.
#[test]
fn a_broken_file_fails_with_its_line_number() {
    let root = Root::new("broken");
    root.write_config(concat!(
        "exclude = []\n", // 1
        "[output\n",      // 2 — the header never closes
        "color = \"a\"\n",
    ));

    let out = root
        .hog()
        .args(&["--color", "never"])
        .stdin(b"{\"msg\":\"hi\"}")
        .fails();
    assert!(
        out.stderr
            .starts_with(&format!("error: {}:2: ", root.config().display())),
        "{}",
        out.stderr
    );
    assert!(
        out.stdout.is_empty(),
        "a line was rendered from a broken config"
    );
}

/// The same file, but reached through `hog config exclude add`: the edit has to fail
/// **and leave the file exactly as it was**. Appending to a document we could
/// not read would mean rewriting it from our own idea of what it said, and the
/// comments would be the first casualty.
#[test]
fn an_edit_on_a_broken_file_changes_nothing() {
    let root = Root::new("broken-edit");
    let before = concat!(
        "# precious\n",
        "exclude = [\"a\"]\n",
        "time_zone = \"utc\n", // the string never closes
    );
    root.write_config(before);

    let out = root
        .hog()
        .args(&["config", "exclude", "add", "new"])
        .fails();
    out.stderr_has(&format!("{}:3:", root.config().display()));
    assert_eq!(root.read_config(), before, "a broken file was rewritten");
    assert!(
        leftovers(root.config().parent().expect("the config has a parent")).is_empty(),
        "a temp file was left behind"
    );
}

/// A value hog understands the shape of but cannot honour fails the same way,
/// on the line it is written — unlike an unknown key, which is only a warning.
/// HLD §3: a `time_format` without a `%` has to be one of two reserved words.
#[test]
fn a_value_hog_cannot_honour_fails_on_its_own_line() {
    // The two reserved words `time_format` accepts besides a strftime
    // pattern are checked below, on this line.
    const TS: &[u8] = br#"{"ts":"2025-06-15T10:32:01Z","msg":"hi"}"#;

    let root = Root::new("bad-value");
    root.write_config(concat!(
        "[output]\n",                // 1
        "time_zone = \"local\"\n",   // 2
        "time_format = \"HH:MM\"\n", // 3 — no '%', and not `raw` or `none`
    ));

    let out = root.hog().stdin(b"").fails();
    out.stderr_has(&format!("{}:3:", root.config().display()))
        .stderr_has("raw");

    // The two reserved words are accepted, and they mean what §3 says they
    // mean — which is what makes the rule a rule rather than a typo check.
    root.write_config("[output]\ntime_format = \"raw\"\n");
    let out = root.hog().args(&["--color", "never"]).stdin(TS).ok();
    assert_eq!(
        out.stdout, "2025-06-15T10:32:01Z hi\n",
        "`raw` has to print the timestamp byte for byte"
    );

    root.write_config("[output]\ntime_format = \"none\"\n");
    let out = root.hog().args(&["--color", "never"]).stdin(TS).ok();
    assert_eq!(out.stdout, "hi\n", "`none` has to drop the column");
}

/// `exclude` written as something other than an array of paths is an error
/// rather than a silent replacement: the user meant to keep that value, and an
/// edit that quietly turned it into an array would throw it away.
#[test]
fn a_non_array_exclude_is_refused_instead_of_overwritten() {
    let root = Root::new("non-array");
    let before = "# mine\nexclude = \"grpc\"\n";
    root.write_config(before);

    // The message is the deserializer's, but the position is hog's: line 2 is
    // where `exclude` is written.
    let out = root
        .hog()
        .args(&["config", "exclude", "add", "new"])
        .fails();
    out.stderr_has(&format!("{}:2:", root.config().display()));
    assert_eq!(root.read_config(), before, "the value was overwritten");

    // A run reports it the same way rather than rendering with a silently
    // empty exclude list.
    let out = root.hog().stdin(b"{\"msg\":\"hi\"}").fails();
    out.stderr_has(&format!("{}:2:", root.config().display()));
    assert!(out.stdout.is_empty());
}

// ============================================================== golden --help

/// The `.trycmd` files under `tests/cmd/` (HLD §2): golden output for the CLI
/// surface itself.
///
/// `assert_cmd` above checks what a run *does*; this checks what `--help`
/// *says*, which is the half that drifts silently when a flag is renamed. An
/// intentional change is recorded with `TRYCMD=overwrite cargo test`.
///
/// Three variables are pinned, because each of them would otherwise print the
/// developer's own machine into the golden file:
///
/// * `COLUMNS` — clap's `wrap_help` wraps to the terminal width, and `100` is
///   the width it falls back to when there is no terminal;
/// * `HOG_CONFIG` — clap echoes the variable's current value into the
///   `--config` row as `[env: HOG_CONFIG=…]`, and an empty value renders
///   exactly like an unset one;
/// * `NO_COLOR` — a `CLICOLOR_FORCE` in the environment would otherwise put
///   ANSI escapes in the middle of the expected text.
#[test]
fn the_documented_cli_surface_matches_the_golden_files() {
    trycmd::TestCases::new()
        .env("COLUMNS", "100")
        .env("HOG_CONFIG", "")
        .env("NO_COLOR", "1")
        .case("tests/cmd/*.trycmd");
}
