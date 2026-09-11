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
//! flags: `path`, `init`, `edit`, `exclude [add|rm]`, `command [set]`.
//!
//! Every child gets `XDG_CONFIG_HOME` pointed at a directory this test owns.
//! `std::env::set_var` is `unsafe` and the package denies unsafe code, so the
//! environment can only be controlled from the *parent* side — which is exactly
//! why `config::discover` takes an `Env` instead of reading one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use assert_cmd::cargo::CommandCargoExt as _;

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

    /// The file `hog config path` is expected to name.
    fn config(&self) -> PathBuf {
        self.path.join("hog").join("config.toml")
    }

    /// Writes a config file, creating `<home>/hog` on the way.
    fn write_config(&self, text: &str) -> PathBuf {
        let path = self.config();
        fs::create_dir_all(path.parent().expect("the config has a parent"))
            .expect("the config directory is creatable");
        fs::write(&path, text).expect("the config is writable");
        path
    }

    fn read_config(&self) -> String {
        fs::read_to_string(self.config()).expect("the config is readable")
    }

    /// Runs `hog <args>` with this directory as the config home.
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
            .env("XDG_CONFIG_HOME", &self.path)
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
    let out = home.run(&["config", "path"]).exited(0);

    assert_eq!(out.stdout, format!("{}\n", home.config().display()));
    assert_eq!(out.stderr, "");
    assert!(
        !home.config().exists(),
        "`config path` answered a question, it must not create a file"
    );
}

/// HLD §3: no `$XDG_CONFIG_HOME` and no `$HOME` is not a path hog can guess, so
/// `hog config` says so instead of printing an empty line.
#[test]
fn path_without_a_config_home_is_an_error_naming_both_variables() {
    let home = Home::new("nohome");
    let mut command = Command::cargo_bin("hog").expect("the binary is built");
    let out = command
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env_remove("HOG_CONFIG")
        .args(["config", "path"])
        .output()
        .expect("hog must finish");
    drop(home);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("XDG_CONFIG_HOME"), "{stderr}");
    assert!(stderr.contains("HOME"), "{stderr}");
}

// ======================================================================= init

#[test]
fn init_writes_a_commented_starter_and_refuses_to_clobber_it() {
    let home = Home::new("init");

    let out = home.run(&["config", "init"]).exited(0);
    assert!(out.stdout.starts_with("created "), "{}", out.stdout);

    let written = home.read_config();
    assert!(written.contains("exclude = []"), "{written}");
    assert!(
        written.contains("appends here (persistent)"),
        "the starter is the documentation of the format: {written}"
    );
    assert!(written.contains("command = \"ssh -tt"), "{written}");

    // A second `init` must not touch the file, whatever is in it by then.
    fs::write(home.config(), "exclude = [\"mine\"]\n").expect("the setup write succeeds");
    let out = home.run(&["config", "init"]).exited(0);
    assert!(out.stdout.contains("already exists"), "{}", out.stdout);
    assert_eq!(home.read_config(), "exclude = [\"mine\"]\n");
}

/// The file a fresh `config init` writes has to load without a single warning
/// — otherwise every new install starts with a diagnostic.
#[test]
fn the_file_init_writes_loads_silently() {
    let home = Home::new("init-clean");
    home.run(&["config", "init"]).exited(0);

    let out = home
        .run_with(&["--color", "never"], &[], b"{\"msg\":\"hi\"}\n")
        .exited(0);
    assert_eq!(out.stderr, "", "a fresh config warned about itself");
}

// =============================================================== exclude verb

/// The wave's headline guarantee: editing the list is not paid for in comments.
#[test]
fn appending_and_removing_keeps_every_comment_in_the_file() {
    let home = Home::new("edit");
    home.run(&["config", "init"]).exited(0);
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

    let out = home.run(&["config", "exclude"]).exited(0);
    assert_eq!(out.stdout, "");
    assert!(out.stderr.contains("nothing is excluded"), "{}", out.stderr);
    assert!(!home.config().exists(), "a question created a file");

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
        "source:    $XDG_CONFIG_HOME",
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

#[test]
fn the_summary_without_a_file_shows_the_defaults_and_how_to_create_one() {
    let home = Home::new("summary-empty");
    let out = home.run(&["config"]).exited(0);

    assert!(out.stdout.contains("not there yet"), "{}", out.stdout);
    assert!(out.stdout.contains("hog config init"), "{}", out.stdout);
    assert!(out.stdout.contains("exclude:   (none)"), "{}", out.stdout);
    assert!(
        out.stdout.contains("time:      %H:%M:%S, local"),
        "{}",
        out.stdout
    );
    assert!(!home.config().exists(), "the summary created a file");
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
    home.write_config("exclude = [\"from_xdg\"]\n");
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
            br#"{"from_xdg":1,"from_flag":2}"#,
        )
        .exited(0);
    assert_eq!(
        out.stdout, "from_xdg=1\n",
        "the flag's list applied and the XDG one did not"
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

    // No `command` key yet: the built-in, which prints the arguments back.
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

    let out = home.run(&["config", "command"]).exited(0);
    assert_eq!(out.stdout, "echo {@}\n", "the built-in default (HLD §5)");
    assert!(out.stderr.contains("built-in default"), "{}", out.stderr);
    assert!(!home.config().exists(), "a question created a file");

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
    home.run(&["config", "init"]).exited(0);
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

    // 1. no editor at all: a refusal that names both variables and the file, and
    //    creates nothing — there is nothing to open it with.
    let out = home.run(&["config", "edit"]).exited(1);
    assert!(out.stderr.contains("$VISUAL"), "{}", out.stderr);
    assert!(out.stderr.contains("$EDITOR"), "{}", out.stderr);
    assert!(
        out.stderr.contains(&home.config().display().to_string()),
        "{}",
        out.stderr
    );
    assert!(!home.config().exists(), "the refusal created a file");

    // 2. an editor that appends a line. The file did not exist, so hog seeds the
    //    commented starter first — an empty buffer would make the user write a
    //    config from memory.
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
    assert!(out.stderr.contains("created"), "{}", out.stderr);
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
    home.run(&["config", "init"]).exited(0);

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
    home.run(&["config", "init"]).exited(0);

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

/// There is deliberately no implicit `./hog.toml` and no walk up the tree: the
/// config names a command hog executes, so picking one up from the working
/// directory would make `git clone && cd && hog prod api` remote code execution
/// (HLD §3).
#[test]
fn a_config_in_the_working_directory_is_never_picked_up() {
    let home = Home::new("cwd");
    // Every name a "helpful" implementation might reach for, all of them
    // carrying a `command` the test would notice being honoured.
    let planted = home.path.join("hog.toml");
    for name in ["hog.toml", ".hog.toml", "config.toml", "hog.config.toml"] {
        fs::write(
            home.path.join(name),
            "exclude = [\"planted\"]\ncommand = \"echo planted\"\n",
        )
        .expect("the setup write succeeds");
    }

    let run = |args: &[&str], input: &[u8]| {
        use std::io::Write as _;

        let mut command = Command::cargo_bin("hog").expect("the binary is built");
        let mut child = command
            .current_dir(&home.path)
            .env_remove("HOG_CONFIG")
            .env_remove("NO_COLOR")
            .env("XDG_CONFIG_HOME", &home.path)
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

    // 1. discovery names the config home, not anything beside the process.
    let (stdout, _, code) = run(&["config", "path"], b"");
    assert_eq!(code, 0);
    assert_eq!(
        stdout.trim_end(),
        home.config().display().to_string(),
        "hog looked somewhere other than the config home"
    );
    assert_ne!(Path::new(stdout.trim_end()), planted);

    // 2. and a real render run does not act on the planted file either —
    //    `--path` and the render path could have disagreed.
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
}
