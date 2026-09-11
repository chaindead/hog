//! `hog config` and the config layer, through the real binary.
//!
//! The unit tests under `src/config/` own the interesting matrices — every
//! array style an edit has to survive, every unknown-key shape, the atomic
//! write. What can only be checked out here is the wiring: that discovery reads
//! the environment the process was actually given, that the two streams are the
//! two streams a shell sees, that `hog config edit` really does spawn `$EDITOR`,
//! and that the exit codes match HLD §6.
//!
//! The surface under test is the one HLD §6 and §11.8 settle on — verbs, not
//! flags: `path`, `edit`, `exclude [add|rm]`, `command [set]`. There is no
//! `init`: hog writes `$HOME/.hog.toml` itself on the first run that finds it
//! missing, which is its own section below.
//!
//! Every child gets `$HOME` pointed at a directory this test owns, so the file
//! under test is `<that directory>/.hog.toml` and never the developer's own —
//! and since a run now *creates* that file, an assertion about stderr in this
//! file is an assertion about the second run unless it says otherwise.
//! `std::env::set_var` is `unsafe` and the package denies unsafe code, so the
//! environment can only be controlled from the *parent* side — which is exactly
//! why `config::discover` takes an `Env` instead of reading one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use assert_cmd::cargo::CommandCargoExt as _;
use hog::config::edit::STARTER;

/// A directory that removes itself when the test ends (`test-fixture-raii`).
struct Home {
    path: PathBuf,
}

impl Home {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hog-config-cli-{tag}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the temp directory is creatable");
        Self { path }
    }

    /// The file `hog config path` is expected to name: `$HOME/.hog.toml`.
    fn config(&self) -> PathBuf {
        self.path.join(hog::config::discover::CONFIG_FILE)
    }

    /// The one line a run prints when it creates this home's config file.
    fn created_line(&self) -> String {
        format!("hog: created {}\n", self.config().display())
    }

    /// Puts this `$HOME` where a real one is after its owner has run hog once:
    /// the config file exists and no later run has anything to announce.
    ///
    /// `config path` is the invocation used because it is the one verb that
    /// writes nothing of its own — whatever ends up in the file was put there
    /// by the run, not by the verb.
    fn after_first_run(&self) -> &Self {
        let out = self.run(&["config", "path"]).exited(0);
        assert_eq!(out.stderr, self.created_line());
        assert!(self.config().exists(), "the first run created nothing");
        self
    }

    /// Writes the config file this `$HOME` resolves to.
    fn write_config(&self, text: &str) -> PathBuf {
        let path = self.config();
        fs::write(&path, text).expect("the config is writable");
        path
    }

    fn read_config(&self) -> String {
        fs::read_to_string(self.config()).expect("the config is readable")
    }

    /// Runs `hog <args>` with this directory as `$HOME`.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[], b"")
    }

    /// The same, with extra environment variables and something on stdin.
    fn run_with(&self, args: &[&str], env: &[(&str, &str)], input: &[u8]) -> Output {
        use std::io::Write as _;

        let mut command = Command::cargo_bin("hog").expect("the binary is built by `cargo test`");
        command
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("COLORTERM")
            .env_remove("HOG_CONFIG")
            .env("HOME", &self.path)
            .args(args);
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

        Output {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().expect("hog must not die from a signal"),
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Output {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Output {
    /// Asserts the exit code and hands the output back for further checks.
    fn exited(self, code: i32) -> Self {
        assert_eq!(
            self.code, code,
            "stdout: {}\nstderr: {}",
            self.stdout, self.stderr
        );
        self
    }
}

/// The four excludes a render would hide, read off a rendered line.
fn hidden_keys(line: &str) -> Vec<&str> {
    line.split_whitespace()
        .filter_map(|word| word.split_once('='))
        .map(|(key, _)| key)
        .collect()
}

// ======================================================================= path

#[test]
fn path_prints_the_file_that_would_be_read() {
    let home = Home::new("path");
    home.after_first_run();

    let out = home.run(&["config", "path"]).exited(0);
    assert_eq!(out.stdout, format!("{}\n", home.config().display()));
    assert_eq!(out.stderr, "", "stderr is empty once the file is there");

    // The answer is the same on the very first run, when the file is being
    // created underneath it: `$(hog config path)` is a path, and the note about
    // the new file is on the other stream where a substitution cannot see it.
    let fresh = Home::new("path-fresh");
    let out = fresh.run(&["config", "path"]).exited(0);
    assert_eq!(out.stdout, format!("{}\n", fresh.config().display()));
    assert_eq!(out.stderr, fresh.created_line());
}

/// HLD §3: no `$HOME` is not a path hog can guess, so `hog config` says so
/// instead of printing an empty line.
#[test]
fn path_without_a_home_is_an_error_naming_the_variable() {
    let home = Home::new("nohome");
    let mut command = Command::cargo_bin("hog").expect("the binary is built");
    let out = command
        .env_remove("HOME")
        .env_remove("HOG_CONFIG")
        .args(["config", "path"])
        .output()
        .expect("hog must finish");
    drop(home);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("$HOME"), "{stderr}");
    assert!(stderr.contains("--config"), "{stderr}");
}

// ============================================================== auto-creation

/// The first run on a fresh machine writes the commented starter, whatever that
/// run happened to be — here the plainest one there is, `cat file | hog`.
///
/// One line about it, on stderr; nothing about it on stdout, where it would
/// land inside `hog … | grep`; and nothing at all from the runs after it.
#[test]
fn the_first_run_writes_the_starter_and_says_so_exactly_once() {
    let home = Home::new("autocreate");
    assert!(!home.config().exists());

    let out = home
        .run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n")
        .exited(0);

    assert_eq!(out.stdout, "hi\n", "the log is what stdout is for");
    assert_eq!(out.stderr, home.created_line());

    let written = home.read_config();
    assert!(written.contains("exclude = []"), "{written}");
    assert!(
        written.contains("appends here (persistent)"),
        "the starter is the documentation of the format: {written}"
    );
    assert!(
        written.contains("# command = \"ssh -tt"),
        "the template has to arrive commented out: {written}"
    );

    // The second run says nothing: the line is news, and news is only news
    // once.
    let out = home
        .run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n")
        .exited(0);
    assert_eq!(out.stdout, "hi\n");
    assert_eq!(out.stderr, "");
}

/// A config the user has edited is never written over, however many runs go
/// past it.
#[test]
fn an_existing_config_is_never_clobbered_by_a_later_run() {
    let home = Home::new("autocreate-keep");
    home.write_config("exclude = [\"mine\"]\n");

    let out = home.run_with(&[], &[], b"").exited(0);

    assert_eq!(out.stderr, "");
    assert_eq!(home.read_config(), "exclude = [\"mine\"]\n");
}

/// The whole justification for writing a file nobody asked for: it changes
/// nothing. The same input renders byte for byte the same before the file
/// exists, on the run that creates it, and on every run after.
#[test]
fn the_created_config_changes_nothing_about_the_output() {
    const LINE: &[u8] =
        br#"{"ts":"2025-06-15T10:32:01Z","level":"info","msg":"hi","a":1,"grpc":{"code":"OK"}}"#;
    let args = ["--color", "never", "--timezone", "utc"];

    // A `$HOME` that is not there is the portable way to see a run with no
    // config file at all: hog creates a config file, never a home directory.
    let home = Home::new("same-output-none");
    let nowhere = home.path.join("not-here");
    let nowhere = nowhere.display().to_string();
    let before = home
        .run_with(&args, &[("HOME", nowhere.as_str())], LINE)
        .exited(0);
    assert_eq!(before.stderr, "", "the baseline run said something");
    assert!(
        !Path::new(&nowhere).exists(),
        "a home directory was invented"
    );

    // The run that creates the file, and the run after it.
    let creating = home.run_with(&args, &[], LINE).exited(0);
    let after = home.run_with(&args, &[], LINE).exited(0);

    assert_eq!(home.read_config(), STARTER, "the file is the starter");
    assert_eq!(
        creating.stdout, before.stdout,
        "the run that created the config rendered differently"
    );
    assert_eq!(
        after.stdout, before.stdout,
        "the created config changed the output"
    );
    assert_eq!(after.stderr, "");
}

/// The file hog writes has to load without a single warning — otherwise every
/// new install starts with a diagnostic about a file it never wrote itself.
#[test]
fn the_file_hog_creates_loads_without_a_warning() {
    let home = Home::new("autocreate-clean");
    home.after_first_run();

    let out = home
        .run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n")
        .exited(0);
    assert_eq!(out.stderr, "", "a fresh config warned about itself");
}

/// The `hog config …` verbs are runs like any other, so the first one of those
/// creates the file too — including the ones that write nothing themselves.
#[test]
fn a_config_verb_is_a_first_run_like_any_other() {
    for verb in [
        ["config"].as_slice(),
        ["config", "path"].as_slice(),
        ["config", "exclude"].as_slice(),
        ["config", "command"].as_slice(),
    ] {
        let home = Home::new("autocreate-verb");
        let out = home.run(verb).exited(0);

        assert!(
            out.stderr.starts_with(&home.created_line()),
            "`hog {}` did not create the file: {}",
            verb.join(" "),
            out.stderr
        );
        assert!(
            !out.stdout.contains("created"),
            "the note reached stdout: {}",
            out.stdout
        );
        assert_eq!(home.read_config(), STARTER);
    }
}

/// A file the **user** named is not hog's to invent: a misspelled `--config` or
/// `$HOG_CONFIG` is the same exit-1 refusal it always was, and no file appears
/// anywhere — neither at the path they named nor at the default one.
#[test]
fn a_named_file_that_is_missing_is_still_an_error_and_creates_nothing() {
    let home = Home::new("autocreate-named");
    let missing = home.path.join("nope.toml");
    let named = missing.display().to_string();

    for (args, env) in [
        (vec!["--config", named.as_str()], vec![]),
        (vec![], vec![("HOG_CONFIG", named.as_str())]),
    ] {
        let out = home.run_with(&args, &env, b"{\"msg\":\"hi\"}\n").exited(1);

        assert!(out.stderr.contains(&named), "{}", out.stderr);
        assert!(out.stdout.is_empty(), "the stream was rendered anyway");
        assert!(!missing.exists(), "the named file was invented");
        assert!(
            !home.config().exists(),
            "hog fell back to creating the default file instead"
        );
    }
}

/// Rule two: creating the config is a convenience, and a convenience that
/// cannot be had is worth one line, not a tool that refuses to show logs.
#[cfg(unix)]
#[test]
fn a_home_hog_cannot_write_to_still_renders() {
    let home = Home::new("autocreate-readonly");
    make_read_only(&home.path);

    let out = home.run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n");
    make_writable(&home.path);

    let out = out.exited(0);
    assert_eq!(out.stdout, "hi\n", "the logs are the job");
    assert_eq!(out.stderr.lines().count(), 1, "{}", out.stderr);
    assert!(
        out.stderr.starts_with("hog: could not create "),
        "{}",
        out.stderr
    );
    assert!(!home.config().exists());
}

/// The same rule, for the failures the kernel words differently: a `$HOME`
/// that is a regular file, and a `~/.hog.toml` that is a directory.
///
/// Both are states auto-creation can be left standing in front of — it will not
/// `mkdir` a `$HOME`, and `create_new` on a directory reports "already there" —
/// and in both there is no config file and never was one. Reading them used to
/// be `ENOTDIR` / `EISDIR` straight out of `read_to_string` and exit 1, which
/// turned "hog could not create its own config" into "hog will not show you
/// your logs": the exact failure rule two exists to forbid, one step further
/// down. `config` is asked as well as the stream, because the summary is what a
/// user runs *next* when a machine behaves oddly.
#[test]
fn a_default_path_that_cannot_hold_a_file_still_renders() {
    let home = Home::new("autocreate-not-a-file");

    // `$HOME/.hog.toml` is a directory.
    fs::create_dir_all(home.config()).expect("the directory is creatable");

    let out = home
        .run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n")
        .exited(0);
    assert_eq!(out.stdout, "hi\n", "the logs are the job");
    assert_eq!(out.stderr, "", "{}", out.stderr);

    let summary = home.run(&["config"]).exited(0);
    assert!(
        summary.stdout.contains("file:      not there"),
        "{}",
        summary.stdout
    );
    assert!(
        summary.stdout.contains("command:   echo {@}"),
        "{}",
        summary.stdout
    );

    // `$HOME` is a regular file, so `$HOME/.hog.toml` cannot exist at all.
    let home = Home::new("autocreate-home-is-a-file");
    let as_file = home.path.join("home");
    fs::write(&as_file, "not a directory\n").expect("the file is writable");

    let out = home
        .run_with(
            &["--color", "never"],
            &[("HOME", &as_file.display().to_string())],
            b"{\"msg\":\"hi\"}\n",
        )
        .exited(0);
    assert_eq!(out.stdout, "hi\n", "the logs are the job");
    assert_eq!(out.stderr, "", "{}", out.stderr);
}

/// The other half of that rule: a config file that is really there and cannot
/// be read is **not** "no config". Silently rendering the whole stream with
/// settings the user did not ask for is the wrong answer, and the only clue
/// would be the output looking odd.
#[cfg(unix)]
#[test]
fn an_unreadable_config_that_does_exist_is_still_an_error() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = Home::new("autocreate-unreadable");
    let path = home.write_config("exclude = [\"mine\"]\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
        .expect("the file mode is settable");

    let out = home.run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .expect("the file mode is settable");

    let out = out.exited(1);
    assert!(
        out.stderr.contains("failed to read the config file"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "the stream was rendered anyway");
}

/// `$HOME` unset at all: nowhere to write, nothing to say, and the run works.
#[test]
fn no_home_at_all_creates_nothing_and_still_renders() {
    use std::io::Write as _;

    let mut command = Command::cargo_bin("hog").expect("the binary is built");
    let mut child = command
        .env_remove("HOME")
        .env_remove("HOG_CONFIG")
        .env_remove("NO_COLOR")
        .args(["--color", "never"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hog must start");
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(b"{\"msg\":\"hi\"}\n")
        .expect("hog must accept the input");
    let out = child.wait_with_output().expect("hog must finish");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert_eq!(stderr, "");
}

// =============================================================== exclude verb

/// The wave's headline guarantee: editing the list is not paid for in comments.
#[test]
fn appending_and_removing_keeps_every_comment_in_the_file() {
    let home = Home::new("edit");
    home.after_first_run();
    let starter = home.read_config();

    let out = home
        .run(&["config", "exclude", "add", "foo", "bar"])
        .exited(0);
    assert_eq!(out.stdout, "added `foo`\nadded `bar`\n");

    let edited = home.read_config();
    assert_eq!(edited, starter.replace("[]", "[\"foo\", \"bar\"]"));
    assert!(edited.contains("# A real-world list — uncomment what you need."));

    let out = home.run(&["config", "exclude", "rm", "foo"]).exited(0);
    assert_eq!(out.stdout, "removed `foo`\n");
    assert_eq!(home.read_config(), starter.replace("[]", "[\"bar\"]"));

    // And back to where it started, byte for byte.
    home.run(&["config", "exclude", "rm", "bar"]).exited(0);
    assert_eq!(home.read_config(), starter);
}

#[test]
fn an_edit_on_a_fresh_machine_seeds_the_starter() {
    let home = Home::new("seed");
    assert!(!home.config().exists());

    home.run(&["config", "exclude", "add", "trace_id"])
        .exited(0);

    let written = home.read_config();
    assert!(written.contains("exclude = [\"trace_id\"]"), "{written}");
    assert!(
        written.contains(
            "# ------------------------------------------------------------------ command"
        ),
        "the seeded file is the commented starter, not a one-liner: {written}"
    );
}

#[test]
fn a_repeated_edit_says_so_instead_of_duplicating_the_field() {
    let home = Home::new("repeat");
    home.run(&["config", "exclude", "add", "a"]).exited(0);

    let out = home.run(&["config", "exclude", "add", "a"]).exited(0);
    assert_eq!(out.stdout, "`a` was already excluded\n");

    let out = home.run(&["config", "exclude", "rm", "zz"]).exited(0);
    assert_eq!(out.stdout, "`zz` was not excluded\n");
    assert_eq!(home.read_config().matches("\"a\"").count(), 1);
}

/// `exclude add` is comma-separated as well as variadic, exactly like `hog -e`.
#[test]
fn the_persistent_list_takes_commas_too() {
    let home = Home::new("commas");
    home.run(&["config", "exclude", "add", "a,b", "c"])
        .exited(0);
    assert!(
        home.read_config()
            .contains("exclude = [\"a\", \"b\", \"c\"]")
    );
}

/// `hog config exclude` with no operand is a question, not a change: one field
/// per line on stdout, nothing written, and a note on stderr when the list is
/// empty so that stdout stays empty for `wc -l`.
#[test]
fn showing_the_exclude_list_answers_on_stdout_alone() {
    let home = Home::new("show-exclude");
    home.after_first_run();
    let before = home.read_config();

    let out = home.run(&["config", "exclude"]).exited(0);
    assert_eq!(out.stdout, "");
    assert!(out.stderr.contains("nothing is excluded"), "{}", out.stderr);
    assert_eq!(home.read_config(), before, "a question rewrote the file");

    home.write_config("exclude = [\"trace_id\", \"grpc.code\"]\n");
    let out = home.run(&["config", "exclude"]).exited(0);
    assert_eq!(out.stdout, "grpc.code\ntrace_id\n");
    assert_eq!(out.stderr, "");
}

// ==================================================================== summary

#[test]
fn the_summary_reports_the_resolved_configuration_and_where_it_came_from() {
    let home = Home::new("summary");
    home.write_config(
        "exclude = [\"trace_id\"]\n\
         command = \"ssh {0}\"\n\
         [output]\n\
         time_format = \"raw\"\n\
         time_zone = \"utc\"\n\
         sort_keys = false\n",
    );

    let out = home.run(&["config"]).exited(0);
    for needle in [
        &format!("path:      {}", home.config().display()),
        "source:    $HOME",
        "file:      loaded",
        "command:   ssh {0}",
        "exclude:   trace_id",
        "time:      raw (printed byte for byte), utc",
        "sort_keys: false",
    ] {
        assert!(
            out.stdout.contains(needle),
            "missing {needle:?}:\n{}",
            out.stdout
        );
    }
}

/// `hog config` on a fresh machine reports the file the run has just created,
/// and every row in it is a built-in default — because that is all the starter
/// says.
#[test]
fn the_summary_on_a_fresh_machine_reports_the_file_it_just_created() {
    let home = Home::new("summary-empty");
    let out = home.run(&["config"]).exited(0);

    assert_eq!(out.stderr, home.created_line());
    assert!(out.stdout.contains("file:      loaded"), "{}", out.stdout);
    assert!(out.stdout.contains("exclude:   (none)"), "{}", out.stdout);
    assert!(
        out.stdout.contains("command:   echo {@}  (built-in)"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("time:      %H:%M:%S, local"),
        "{}",
        out.stdout
    );
    assert_eq!(home.read_config(), STARTER);
}

/// The one way left to have no file at all: hog could not write it. The summary
/// says so rather than pretending the file is simply not there yet, and still
/// prints every row from the built-in defaults.
#[cfg(unix)]
#[test]
fn the_summary_without_a_file_says_why_there_is_none() {
    let home = Home::new("summary-unwritable");
    make_read_only(&home.path);

    let out = home.run(&["config"]);
    make_writable(&home.path);

    let out = out.exited(0);
    assert!(
        out.stdout
            .contains("file:      not there (hog could not create it)"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("exclude:   (none)"), "{}", out.stdout);
    assert!(
        out.stdout.contains("time:      %H:%M:%S, local"),
        "{}",
        out.stdout
    );
    assert!(!home.config().exists());
}

// ============================================================== exclude layers

/// The HLD §11.6 table, end to end: the config gives the base list, `-e` adds
/// to it, `-E` drops it, and the two together replace it.
#[test]
fn the_config_list_is_the_base_the_cli_layers_onto() {
    let home = Home::new("layers");
    home.write_config("exclude = [\"a\"]\n");

    let line = br#"{"a":1,"b":2,"c":3}"#;
    let render = |args: &[&str]| -> Vec<String> {
        let out = home.run_with(args, &[], line).exited(0);
        hidden_keys(&out.stdout)
            .into_iter()
            .map(str::to_owned)
            .collect()
    };

    // config only
    assert_eq!(render(&["--color", "never"]), ["b", "c"]);
    // config + -e: both are hidden
    assert_eq!(render(&["--color", "never", "-e", "b"]), ["c"]);
    // -E: the config list is dropped, so everything shows
    assert_eq!(render(&["--color", "never", "-E"]), ["a", "b", "c"]);
    // -E -e c: exactly ["c"]
    assert_eq!(render(&["--color", "never", "-E", "-e", "c"]), ["a", "b"]);
}

/// The config decides the timestamp format and zone, and a flag still wins.
#[test]
fn the_file_sets_the_columns_until_a_flag_overrides_it() {
    let home = Home::new("columns");
    home.write_config(
        "[fields]\nts = \"at\"\n[output]\ntime_format = \"%H:%M\"\ntime_zone = \"utc\"\n",
    );

    let line = br#"{"at":"2025-06-15T10:32:01Z","msg":"hi"}"#;
    let out = home.run_with(&["--color", "never"], &[], line).exited(0);
    assert_eq!(out.stdout, "10:32 hi\n");

    let out = home
        .run_with(&["--color", "never", "--ts-format", "%H:%M:%S"], &[], line)
        .exited(0);
    assert_eq!(out.stdout, "10:32:01 hi\n");
}

// ================================================================ diagnostics

/// HLD §7.2: an unknown key is a warning **with a line number**, on stderr, and
/// the run carries on. Denying it would break a config written for a newer hog.
#[test]
fn an_unknown_key_warns_with_its_line_and_the_run_continues() {
    let home = Home::new("unknown");
    home.write_config("exclude = []\n\n[output]\ntime_fmt = \"%H\"\n");

    let out = home
        .run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n")
        .exited(0);

    assert_eq!(out.stdout, "hi\n", "the line was still rendered");
    assert_eq!(
        out.stderr,
        format!(
            "warning: {}:4: unknown key `output.time_fmt`\n",
            home.config().display()
        )
    );
}

/// A syntax error is an error, and it reads like a compiler diagnostic: the
/// file, the line, and what is wrong. Exit code 1 (HLD §6).
#[test]
fn a_broken_config_fails_with_a_line_number_and_exit_one() {
    let home = Home::new("broken");
    home.write_config("exclude = []\n[output\ncolor = \"auto\"\n");

    let out = home
        .run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n")
        .exited(1);

    assert!(out.stdout.is_empty(), "nothing should have been rendered");
    assert!(
        out.stderr
            .starts_with(&format!("error: {}:2: ", home.config().display())),
        "{}",
        out.stderr
    );
}

/// A value hog understands the shape of and cannot honour is an error too, on
/// the line it is written — unlike an unknown key, which is only a warning.
#[test]
fn a_value_hog_cannot_honour_fails_on_its_line() {
    let home = Home::new("badvalue");
    home.write_config("[output]\ntime_format = \"%H\"\ncolor = \"pink\"\n");

    let out = home.run_with(&[], &[], b"").exited(1);
    assert!(out.stderr.contains(":3:"), "{}", out.stderr);
    assert!(out.stderr.contains("pink"), "{}", out.stderr);
}

/// `$HOG_CONFIG` pointing at nothing is a failure, not a silent fall-through:
/// rendering the whole stream with settings the user did not ask for, and no
/// sign of it, is the outcome HLD §3 rules out.
#[test]
fn hog_config_pointing_at_nothing_is_an_error() {
    let home = Home::new("missing-env");
    let missing = home.path.join("nope.toml");
    let missing = missing.display().to_string();

    let out = home
        .run_with(&[], &[("HOG_CONFIG", missing.as_str())], b"")
        .exited(1);
    assert!(out.stderr.contains("$HOG_CONFIG"), "{}", out.stderr);
    assert!(out.stderr.contains(&missing), "{}", out.stderr);
}

/// `--config` named by hand behaves the same way, and says `--config` rather
/// than `$HOG_CONFIG` — the two arrive in one clap field and are told apart by
/// comparing the value.
#[test]
fn an_explicit_config_flag_pointing_at_nothing_names_the_flag() {
    let home = Home::new("missing-flag");
    let missing = home.path.join("nope.toml");

    let out = home
        .run_with(&["--config", &missing.display().to_string()], &[], b"")
        .exited(1);
    assert!(out.stderr.contains("--config"), "{}", out.stderr);
}

/// A `--config` file that *is* there wins over the default location, with no
/// merging between the two (HLD §3, "first hit wins").
#[test]
fn an_explicit_config_replaces_the_default_one_outright() {
    let home = Home::new("explicit");
    home.write_config("exclude = [\"from_home\"]\n");
    let explicit = home.path.join("ci.toml");
    fs::write(&explicit, "exclude = [\"from_flag\"]\n").expect("the setup write succeeds");

    let out = home
        .run_with(
            &[
                "--color",
                "never",
                "--config",
                &explicit.display().to_string(),
            ],
            &[],
            br#"{"from_home":1,"from_flag":2}"#,
        )
        .exited(0);
    assert_eq!(
        out.stdout, "from_home=1\n",
        "the flag's list applied and the default one did not"
    );
}

// ================================================================ command key

/// The `command` key is the config's only executable value, so this is the test
/// that proves the whole chain: the file is read, the template reaches the
/// settings, the positional argument is substituted, and the child's stdout is
/// what hog renders.
///
/// It also pins the boundary between the built-in and a configured template:
/// with no `command` key the arguments go to `echo {@}` (HLD §5), and the
/// moment the key appears they go to it instead.
#[test]
fn a_configured_command_runs_with_the_positional_arguments_substituted() {
    let home = Home::new("command");
    home.after_first_run();

    // The created file leaves `command` commented out, so this is still the
    // built-in — which prints the arguments back.
    let out = home.run_with(&["prod", "api"], &[], b"").exited(0);
    assert_eq!(out.stdout, "prod api\n");
    assert_eq!(out.stderr, "");

    // A template, and the argument landing in it. `echo` is not a shell
    // builtin here — there is no shell — it is `/bin/echo` found on PATH.
    home.write_config("command = \"echo {0}\"\n");
    let out = home.run_with(&["prod"], &[], b"").exited(0);
    assert_eq!(out.stdout, "prod\n");
    assert_eq!(out.stderr, "");

    // stdin is not read in command mode, even when there is something on it.
    home.write_config("command = \"echo {0}\"\n");
    let out = home
        .run_with(&["from-the-command"], &[], b"{\"msg\":\"from stdin\"}\n")
        .exited(0);
    assert_eq!(out.stdout, "from-the-command\n");

    // And `--dry-run` shows the assembled argv without running anything.
    let out = home.run_with(&["--dry-run", "prod"], &[], b"").exited(0);
    assert_eq!(out.stdout, "echo\nprod\n");
}

// =========================================================== config command

/// `hog config command` answers on stdout alone, so `$(hog config command)` is
/// a template. Where the answer came from goes to stderr, because a built-in
/// default and a template the user wrote are indistinguishable on stdout and
/// only one of them survives editing the file.
#[test]
fn showing_the_command_template_keeps_stdout_to_the_template() {
    let home = Home::new("show-command");
    home.after_first_run();
    let before = home.read_config();

    let out = home.run(&["config", "command"]).exited(0);
    assert_eq!(out.stdout, "echo {@}\n", "the built-in default (HLD §5)");
    assert!(out.stderr.contains("built-in default"), "{}", out.stderr);
    assert_eq!(home.read_config(), before, "a question rewrote the file");

    home.write_config("command = \"ssh {0} 'docker logs -f {@}'\"\n");
    let out = home.run(&["config", "command"]).exited(0);
    assert_eq!(out.stdout, "ssh {0} 'docker logs -f {@}'\n");
    assert_eq!(out.stderr, "");
}

/// The whole point of `command set` being a verb rather than an editor session:
/// the template is checked **before** it is written, so a broken one cannot sit
/// in the file waiting for the next run to trip over it (HLD §3, §11.8).
#[test]
fn setting_a_command_validates_it_before_writing_and_then_runs_it() {
    let home = Home::new("set-command");
    home.after_first_run();
    let starter = home.read_config();

    // Every rule the template parser enforces, refused with exit 1 and with the
    // file left exactly as it was.
    for (template, needle) in [
        ("ssh {0} 'docker logs", "unclosed quote"),
        ("ssh {0} {2}", "uses {2} but never {1}"),
        ("ssh myapp-{@}-1", "{@}"),
        ("", "empty"),
    ] {
        let out = home.run(&["config", "command", "set", template]).exited(1);
        assert!(
            out.stderr.contains(needle),
            "{template:?} was not refused for {needle:?}: {}",
            out.stderr
        );
        assert_eq!(home.read_config(), starter, "{template:?} reached the file");
    }

    // A good one is written, with the forty lines explaining `-tt` intact.
    let out = home
        .run(&["config", "command", "set", "echo {0} {@}"])
        .exited(0);
    assert_eq!(out.stdout, "command set to \"echo {0} {@}\"\n");
    let written = home.read_config();
    assert!(
        written.contains("command = \"echo {0} {@}\"\n"),
        "{written}"
    );
    assert!(
        written.contains("# -o ServerAliveInterval=15"),
        "the comments above the key were lost: {written}"
    );

    // And it is the template the next run actually uses.
    let out = home.run_with(&["prod", "api", "web"], &[], b"").exited(0);
    assert_eq!(out.stdout, "prod api web\n");

    // Setting the same template again changes nothing and says so.
    let out = home
        .run(&["config", "command", "set", "echo {0} {@}"])
        .exited(0);
    assert_eq!(out.stdout, "command was already \"echo {0} {@}\"\n");
}

/// A `command` key that is not in the file yet has to land **above** the first
/// `[table]` header: written after `[output]` it would parse back as
/// `output.command`, which is the exact mistake HLD §3 records against itself.
#[test]
fn a_command_set_on_a_file_full_of_tables_stays_top_level() {
    let home = Home::new("set-top-level");
    home.write_config("exclude = [\"a\"]\n\n[output]\ncolor = \"never\"\n");

    home.run(&["config", "command", "set", "echo {@}"])
        .exited(0);

    let written = home.read_config();
    assert!(
        written.find("command =") < written.find("[output]"),
        "the key landed inside [output]:\n{written}"
    );
    // Proof rather than inference: hog reads it back as the command it runs,
    // and an `output.command` would be an unknown-key warning instead.
    let out = home.run(&["config", "command"]).exited(0);
    assert_eq!(out.stdout, "echo {@}\n");
    assert_eq!(out.stderr, "", "the key was read as output.command");
}

// ============================================================== config edit

/// `hog config edit` hands the file to `$EDITOR` and checks what comes back.
///
/// The editor is a script this test writes, which is the only way to exercise
/// the branch without opening the developer's own vi: `$EDITOR` can only be set
/// from the parent side, since `std::env::set_var` is `unsafe` and the package
/// denies unsafe code.
#[test]
fn edit_opens_the_file_in_the_editor_and_checks_the_result() {
    let home = Home::new("edit");

    // 1. no editor at all: a refusal that names both variables and the file. The
    //    run created the config on its way in — that is not this verb's doing —
    //    but the refusal itself must not touch a `--config` file it will never
    //    open, which is checked on its own below.
    let out = home.run(&["config", "edit"]).exited(1);
    assert!(out.stderr.contains("$VISUAL"), "{}", out.stderr);
    assert!(out.stderr.contains("$EDITOR"), "{}", out.stderr);
    assert!(
        out.stderr.contains(&home.config().display().to_string()),
        "{}",
        out.stderr
    );

    // 2. an editor that appends a line, on the file that is already there.
    let editor = home.path.join("editor.sh");
    fs::write(
        &editor,
        "#!/bin/sh\nprintf 'exclude = [\"from_the_editor\"]\\n' >> \"$1\"\n",
    )
    .expect("the editor script is writable");
    make_executable(&editor);

    let out = home
        .run_with(
            &["config", "edit"],
            &[("EDITOR", &editor.display().to_string())],
            b"",
        )
        .exited(0);
    assert_eq!(out.stdout, "", "`config edit` answers no question");
    assert!(
        !out.stderr.contains("created"),
        "the file was already there to open: {}",
        out.stderr
    );
    let written = home.read_config();
    assert!(written.contains("# -o ServerAliveInterval=15"), "{written}");
    assert!(
        written.ends_with("exclude = [\"from_the_editor\"]\n"),
        "{written}"
    );

    // 3. $VISUAL wins over $EDITOR.
    let visual = home.path.join("visual.sh");
    fs::write(&visual, "#!/bin/sh\necho visual-ran >&2\n").expect("the script is writable");
    make_executable(&visual);
    let out = home
        .run_with(
            &["config", "edit"],
            &[
                ("EDITOR", &editor.display().to_string()),
                ("VISUAL", &visual.display().to_string()),
            ],
            b"",
        )
        .exited(0);
    assert!(out.stderr.contains("visual-ran"), "{}", out.stderr);
}

/// The file a run never creates by itself — one the user named with `--config`
/// — is still created by `edit`, because an editor opened on nothing gives an
/// empty buffer and a config written from memory.
///
/// And the refusal on the way there creates nothing: with no editor to open it
/// with, there is no reason for the file to appear.
#[test]
fn edit_creates_the_file_it_was_told_to_open() {
    let home = Home::new("edit-explicit");
    let named = home.path.join("ci.toml");
    let named = named.display().to_string();

    // No editor: the refusal names the file it would have opened and leaves the
    // filesystem alone.
    home.run(&["--config", &named, "config", "edit"]).exited(1);
    assert!(
        !Path::new(&named).exists(),
        "the refusal created a file it never opened"
    );

    let editor = home.path.join("editor.sh");
    fs::write(&editor, "#!/bin/sh\nprintf '# seen\\n' >> \"$1\"\n")
        .expect("the script is writable");
    make_executable(&editor);

    let out = home
        .run_with(
            &["--config", &named, "config", "edit"],
            &[("EDITOR", &editor.display().to_string())],
            b"",
        )
        .exited(0);

    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, format!("hog: created {named}\n"));
    let written = fs::read_to_string(&named).expect("the file is there");
    assert!(
        written.starts_with("# ~/.hog.toml"),
        "the editor got the commented starter: {written}"
    );
    assert!(written.ends_with("# seen\n"), "{written}");
    assert!(
        !home.config().exists(),
        "a `--config` run touched the default file"
    );
}

/// What the editor left behind is parsed straight away, so a typo is reported
/// with its line number while the user is still at the keyboard — rather than
/// at the next `hog`, when they are looking at something else.
#[test]
fn a_config_the_editor_broke_is_reported_at_once() {
    let home = Home::new("edit-broken");

    for (script, needle) in [
        ("printf 'exclude = [\\n[output\\n' > \"$1\"\n", ":2:"),
        ("printf '[output]\\ncolor = \"pink\"\\n' > \"$1\"\n", "pink"),
        (
            "printf 'command = \"ssh {0} {2}\"\\n' > \"$1\"\n",
            "uses {2} but never {1}",
        ),
    ] {
        let editor = home.path.join("editor.sh");
        fs::write(&editor, format!("#!/bin/sh\n{script}")).expect("the script is writable");
        make_executable(&editor);

        let out = home
            .run_with(
                &["config", "edit"],
                &[("EDITOR", &editor.display().to_string())],
                b"",
            )
            .exited(1);
        assert!(out.stderr.contains(needle), "{script:?}: {}", out.stderr);
    }
}

/// An editor that exits non-zero stops the check, and hog says so rather than
/// claiming either that the edit worked or that nothing was written — it cannot
/// know which.
#[test]
fn an_editor_that_fails_is_reported_rather_than_ignored() {
    let home = Home::new("edit-fails");
    home.after_first_run();

    let out = home
        .run_with(&["config", "edit"], &[("EDITOR", "false")], b"")
        .exited(1);
    assert!(out.stderr.contains("status 1"), "{}", out.stderr);

    // And an editor that is not there at all names what could not be run.
    let out = home
        .run_with(
            &["config", "edit"],
            &[("EDITOR", "/nonexistent/editor")],
            b"",
        )
        .exited(1);
    assert!(out.stderr.contains("/nonexistent/editor"), "{}", out.stderr);
}

/// `$EDITOR` is a command line, not a path: `code -w` and `emacs -nw` are both
/// ordinary values, so it is split by shell rules before it is spawned.
#[test]
fn an_editor_with_arguments_is_split_by_shell_rules() {
    let home = Home::new("edit-args");
    home.after_first_run();

    let editor = home.path.join("editor.sh");
    fs::write(&editor, "#!/bin/sh\necho \"got: $1 $2\" >&2\n").expect("the script is writable");
    make_executable(&editor);

    let out = home
        .run_with(
            &["config", "edit"],
            &[("EDITOR", &format!("{} --wait", editor.display()))],
            b"",
        )
        .exited(0);
    assert!(
        out.stderr
            .contains(&format!("got: --wait {}", home.config().display())),
        "the flag was lost or glued to the program: {}",
        out.stderr
    );
}

/// `chmod 500` on a directory: readable and searchable, and nothing may be
/// created in it. Used to stage the one failure mode auto-creation has to
/// survive rather than report as an error.
#[cfg(unix)]
fn make_read_only(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o500))
        .expect("the directory mode is settable");
}

/// Puts the mode back, so the fixture can delete itself again. Always called
/// before the assertions that might fail.
#[cfg(unix)]
fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .expect("the directory mode is settable");
}

/// `chmod +x`, which `fs::set_permissions` spells the long way.
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("the script is made executable");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

// =========================================================== global --config

/// The regression HLD §6's grammar block would have caused: `--config` is
/// `global = true`, and with `args_conflicts_with_subcommands` set clap counted
/// it as "the parent's args were given" and stopped looking for a subcommand.
/// `hog --config x config` then ran the **command mode** with `ARGS=["config"]`
/// — a wrong answer with no diagnostic at all.
///
/// Checked through the process because that is where the damage showed: the
/// parse test in `cli_surface.rs` covers the same rows one layer down.
#[test]
fn the_config_flag_works_on_either_side_of_the_subcommand() {
    let home = Home::new("global-flag");
    let explicit = home.path.join("ci.toml");
    fs::write(&explicit, "exclude = [\"from_flag\"]\n").expect("the setup write succeeds");
    let explicit = explicit.display().to_string();

    for args in [
        vec!["--config", explicit.as_str(), "config", "path"],
        vec!["config", "--config", explicit.as_str(), "path"],
        vec!["config", "path", "--config", explicit.as_str()],
    ] {
        let out = home.run(&args).exited(0);
        assert_eq!(out.stdout, format!("{explicit}\n"), "args: {args:?}");
    }

    // And the verbs below `config` see it too.
    let out = home
        .run(&["--config", explicit.as_str(), "config", "exclude"])
        .exited(0);
    assert_eq!(out.stdout, "from_flag\n");
}

// ===================================================================== safety

/// There is deliberately no implicit `./hog.toml` — nor `./.hog.toml`, which
/// is now the *name* of the default config — and no walk up the tree: the
/// config names a command hog executes, so picking one up from the working
/// directory would make `git clone && cd && hog prod api` remote code execution
/// (HLD §3).
///
/// The dotfile name makes this sharper than it was, not softer: `~/.hog.toml`
/// and `./.hog.toml` are spelled identically, so the working directory here is
/// deliberately **not** `$HOME` — a `$HOME`-relative join and a CWD-relative one
/// would otherwise be indistinguishable.
#[test]
fn a_config_in_the_working_directory_is_never_picked_up() {
    let home = Home::new("cwd");
    let work = home.path.join("work");
    fs::create_dir_all(&work).expect("the working directory is creatable");

    // Every name a "helpful" implementation might reach for, all of them
    // carrying a `command` the test would notice being honoured.
    let planted = work.join(".hog.toml");
    for name in ["hog.toml", ".hog.toml", "config.toml", "hog.config.toml"] {
        fs::write(
            work.join(name),
            "exclude = [\"planted\"]\ncommand = \"echo planted\"\n",
        )
        .expect("the setup write succeeds");
    }

    let run = |args: &[&str], input: &[u8]| {
        use std::io::Write as _;

        let mut command = Command::cargo_bin("hog").expect("the binary is built");
        let mut child = command
            .current_dir(&work)
            .env_remove("HOG_CONFIG")
            .env_remove("NO_COLOR")
            .env("HOME", &home.path)
            .args(args)
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
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().expect("hog must not die from a signal"),
        )
    };

    // 1. discovery names the file in $HOME, not anything beside the process.
    let (stdout, _, code) = run(&["config", "path"], b"");
    assert_eq!(code, 0);
    assert_eq!(
        stdout.trim_end(),
        home.config().display().to_string(),
        "hog looked somewhere other than $HOME"
    );
    assert_ne!(Path::new(stdout.trim_end()), planted);

    // 2. and a real render run does not act on the planted file either —
    //    `config path` and the render path could have disagreed.
    let (stdout, _, code) = run(&[], b"{\"msg\":\"hi\",\"planted\":1}\n");
    assert_eq!(code, 0);
    assert!(
        stdout.contains("planted=1"),
        "the planted exclude list was applied: {stdout}"
    );

    // 3. the dangerous half: a planted `command` must not become something hog
    //    is willing to run. With no config of its own, hog falls back to the
    //    built-in `echo {@}` — so the argument is echoed rather than handed to
    //    whatever the planted file asked for.
    let (stdout, stderr, code) = run(&["prod"], b"");
    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout, "prod\n",
        "a planted command template was picked up: {stdout}"
    );

    // 4. and the file in $HOME is the one that *is* read, so the test above
    //    cannot pass merely because discovery is broken everywhere.
    home.write_config("exclude = [\"planted\"]\n");
    let (stdout, _, code) = run(&["--color", "never"], b"{\"msg\":\"hi\",\"planted\":1}\n");
    assert_eq!(code, 0);
    assert_eq!(stdout, "hi\n", "the config in $HOME was not read: {stdout}");
}
