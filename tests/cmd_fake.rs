//! Command mode end to end, against a stub script standing in for `ssh`.
//!
//! HLD §2 names this file, and the reason it is separate from
//! `tests/cmd_template.rs` is the reason command mode was split into a pure half
//! and an impure one: everything *about the argv* — substitution, arity, the
//! whitelist — is settled by pure functions and pinned there, so what is left
//! here is only what a real process adds, and every one of those is a thing that
//! cannot be faked:
//!
//! * the child's stdout really is the stream hog renders, and its stderr really
//!   is the user's;
//! * a non-zero status is propagated, with the loud line of HLD §5 — and so is
//!   a death by signal, as 128 + the signal;
//! * `hog … | head -2` exits 141 **and the child is dead afterwards** — the one
//!   failure that leaves a `docker logs -f` running on a production host,
//!   checked both for a child that sleeps between writes and for one blocked
//!   inside `write()` on a pipe nobody drains;
//! * a program that is not in `PATH` exits 127, not 1;
//! * `--dry-run` runs nothing at all, which is only provable by giving the stub
//!   a side effect and finding it absent;
//! * there is **no local shell**: `$(id)`, `2>&1` and `$HOME` written outside
//!   the template's quotes arrive at the program as literal argv entries.
//!
//! All six rows of the input-mode table of HLD §6 are here too, in
//! [`input_modes`]. The three that need stdin to be a terminal go through
//! `script(1)`; if there is no pty to be had they say so and skip, rather than
//! failing for a reason that has nothing to do with hog.
//!
//! # Isolation
//!
//! Every test gets its own throwaway directory, used as both `$XDG_CONFIG_HOME`
//! and `$HOME`, and `$HOG_CONFIG` is removed from the environment — so the
//! machine's real config cannot change an outcome and a test cannot write
//! anywhere but its own directory. The stubs are shell scripts rather than
//! `ssh`, so the suite needs no network, no host and no credentials. hog spawns
//! them **directly**: the `#!/bin/sh` line inside a stub is the kernel's
//! business, not hog's.

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use assert_cmd::cargo::CommandCargoExt as _;

/// Makes every fixture directory unique even if two tests pick the same name.
/// The pid alone is not enough: `cargo test` runs this file's tests as threads
/// of **one** process, so the pid is shared and only the counter separates them.
static NEXT: AtomicU32 = AtomicU32::new(0);

/// A throwaway `$XDG_CONFIG_HOME` plus a place to put stub scripts.
struct Fake {
    path: PathBuf,
}

impl Fake {
    fn new(name: &str) -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hog-cmd-fake-{name}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("hog")).expect("the fake home must be creatable");
        Self { path }
    }

    /// Writes an executable `/bin/sh` script and returns its path.
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).expect("the stub must be writable");
        let mut permissions = fs::metadata(&path).expect("the stub exists").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("the stub must be executable");
        path
    }

    /// Points the config's `command` at `template`.
    fn command(&self, template: &str) {
        // The stub paths contain only `/`, letters, digits and `-`, so a basic
        // TOML string needs no escaping. An assert rather than an escape: a
        // fixture that silently wrote broken TOML would fail somewhere else.
        assert!(
            !template.contains(['"', '\\']),
            "the fixture writes a basic TOML string: {template}"
        );
        fs::write(
            self.path.join("hog/config.toml"),
            format!("command = \"{template}\"\n"),
        )
        .expect("the config must be writable");
    }

    /// Writes a config file somewhere other than the discovered location and
    /// returns its path, for the `--config` route into command mode.
    fn config_at(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, body).expect("the config must be writable");
        path
    }

    fn hog(&self, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("hog").expect("the binary is built by `cargo test`");
        command
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("COLORTERM")
            .env_remove("HOG_CONFIG")
            .env("XDG_CONFIG_HOME", &self.path)
            .env("HOME", &self.path)
            .args(["--color", "never", "--timezone", "utc"])
            .args(args);
        command
    }

    /// Runs hog to completion over `input`, returning (stdout, stderr, code).
    fn run(&self, args: &[&str], input: &[u8]) -> (String, String, i32) {
        let mut child = self
            .hog(args)
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
    }

    /// Runs hog with a **pseudo-terminal** on stdin, the only way to reach rows
    /// four to six of the mode table. `None` when `script(1)` is unavailable.
    fn run_on_a_pty(&self, args: &[&str]) -> Option<(String, i32)> {
        let binary = Command::cargo_bin("hog").expect("the binary is built by `cargo test`");
        let binary = binary.get_program().to_string_lossy().into_owned();

        let mut command = Command::new("script");
        command
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("COLORTERM")
            .env_remove("HOG_CONFIG")
            .env("XDG_CONFIG_HOME", &self.path)
            .env("HOME", &self.path);

        if cfg!(target_os = "macos") {
            // BSD: script [-q] [file [command ...]]; exits with the child's status.
            command.arg("-q").arg("/dev/null").arg(&binary).args(args);
        } else {
            // util-linux: one shell word, and -e propagates the child's status.
            let mut line = shell_quote(&binary);
            for arg in args {
                line.push(' ');
                line.push_str(&shell_quote(arg));
            }
            command.arg("-qe").arg("-c").arg(line).arg("/dev/null");
        }

        let out = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .ok()?;

        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Some((text, out.status.code()?))
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Single-quotes one argument for the util-linux `script -c` form.
fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

/// `kill -0` from a separate process: is this pid still alive?
fn is_alive(pid: &str) -> bool {
    Command::new("/bin/sh")
        .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
        .status()
        .is_ok_and(|status| status.success())
}

fn path_of(path: &Path) -> String {
    path.to_str().expect("the temp path is UTF-8").to_owned()
}

// ============================================================ the happy path

/// The whole chain: template → argv → child → renderer → stdout, exit 0.
#[test]
fn a_command_that_ends_cleanly_renders_its_lines_and_exits_zero() {
    let fake = Fake::new("ok");
    let stub = fake.script(
        "logs-ok",
        r#"echo "{\"ts\":\"2025-06-15T10:32:01Z\",\"level\":\"info\",\"msg\":\"server started\",\"port\":8080}"
echo "{\"ts\":\"2025-06-15T10:32:02Z\",\"level\":\"warn\",\"msg\":\"slow request\",\"trace_id\":\"abc123\"}"
echo "plain text line"
"#,
    );
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        "10:32:01 [INF] server started port=8080\n\
         10:32:02 [WRN] slow request trace_id=abc123\n\
         plain text line\n"
    );
}

/// Command mode ignores stdin entirely (row one of the mode table): whatever is
/// piped in must not reach the renderer, and must not block hog either.
#[test]
fn stdin_is_not_read_when_a_command_runs() {
    let fake = Fake::new("ignores-stdin");
    let stub = fake.script("logs", "echo from-the-command-$1\n");
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["prod"], b"{\"msg\":\"from stdin\"}\n");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(stdout, "from-the-command-prod\n");
}

/// **There is no local shell** (HLD §5): the template is split into argv words
/// and handed to the OS, so every shell metacharacter outside the quotes is an
/// ordinary argument. Proved by letting the stub print what it actually got —
/// if a shell were interposed, `$(id)` would have run and `$HOME` would have
/// been a path.
#[test]
fn no_local_shell_stands_between_the_template_and_the_program() {
    let fake = Fake::new("no-shell");
    let stub = fake.script("show-args", "for a in \"$@\"; do echo \"arg=$a\"; done\n");
    // `'b ; c'` is one word because the *template* quotes it — that quoting is
    // resolved by `shlex::split` at parse time, not by any shell at run time.
    fake.command(&format!(
        "{} a$(id) 'b ; c' $HOME 2>&1 {{0}}",
        path_of(&stub)
    ));

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        "arg=a$(id)\n\
         arg=b ; c\n\
         arg=$HOME\n\
         arg=2>&1\n\
         arg=prod\n",
        "something expanded, redirected or re-split the argv"
    );
}

/// The other half of the same property: a value that passes the whitelist is
/// substituted byte for byte, with no quoting added (HLD §5 rejects quoting
/// outright, because hog cannot know whether the remote shell will split the
/// word again).
#[test]
fn a_substituted_value_reaches_the_program_verbatim() {
    let fake = Fake::new("verbatim");
    let stub = fake.script("show-args", "for a in \"$@\"; do echo \"arg=$a\"; done\n");
    fake.command(&format!("{} {{0}} myapp-{{1}}-1", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["deploy@prod-1.example.com:22", "api.v2"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        "arg=deploy@prod-1.example.com:22\narg=myapp-api.v2-1\n"
    );
}

// ============================================================== exit statuses

/// HLD §5's loud line, and the reason it exists: `ssh` exits 255 when the VPN
/// drops, and without this the run would look exactly like "the log ended".
#[test]
fn a_non_zero_exit_is_propagated_with_a_loud_line_naming_the_line_count() {
    let fake = Fake::new("status-255");
    let stub = fake.script(
        "logs-255",
        r#"echo "{\"level\":\"info\",\"msg\":\"first\"}"
echo "{\"level\":\"error\",\"msg\":\"second\"}"
exit 255
"#,
    );
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!(code, 255, "the command's own code is hog's: {stderr}");
    // The lines that did arrive are rendered; the failure does not eat them.
    assert_eq!(stdout, "[INF] first\n[ERR] second\n");
    assert!(
        stderr.contains("command exited with status 255 after 2 lines"),
        "the loud line of HLD §5 is missing from: {stderr}"
    );
}

/// A command that fails before printing anything still says so, and the count
/// reads as a sentence rather than "after 0 lines" being the only clue.
#[test]
fn a_command_that_fails_immediately_still_reports_its_status() {
    let fake = Fake::new("status-7");
    let stub = fake.script("fails", "exit 7\n");
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!(code, 7, "stderr: {stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("command exited with status 7 after 0 lines"),
        "{stderr}"
    );
}

/// 127 is the code every shell uses for "command not found", so a script
/// wrapping hog can tell a missing `ssh` from an `ssh` that ran and failed.
#[test]
fn a_program_that_is_not_in_path_exits_127() {
    let fake = Fake::new("not-found");
    fake.command("hog-no-such-program-anywhere -tt {0}");

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!(code, 127, "stderr: {stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.contains("command not found"), "{stderr}");
    assert!(
        stderr.contains("hog-no-such-program-anywhere"),
        "the message must name the program: {stderr}"
    );
    // And the hint shows the argv, because the usual cause is a template whose
    // first word is a shell alias — and there is no shell here.
    assert!(
        stderr.contains("hog-no-such-program-anywhere -tt prod"),
        "the hint must show what it tried: {stderr}"
    );
}

/// A file that exists but is not executable is a plain runtime error, not 127:
/// `PATH` found it, so "command not found" would send the reader the wrong way.
#[test]
fn a_file_that_is_not_executable_is_exit_one() {
    let fake = Fake::new("not-executable");
    let path = fake.path.join("not-executable");
    fs::write(&path, "data\n").expect("writable");
    fake.command(&format!("{} {{0}}", path_of(&path)));

    let (_, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("not-executable"), "{stderr}");
    assert!(!stderr.contains("not found"), "{stderr}");
}

/// Exit 1 from the command is still *the command's* failure, and the loud line
/// is what says so. Without it, hog's own exit 1 (a bad template, an unreadable
/// config) and the remote command's exit 1 would be indistinguishable.
#[test]
fn exit_one_from_the_command_is_still_announced_as_the_commands_own() {
    let fake = Fake::new("status-1");
    let stub = fake.script(
        "fails-late",
        "echo \"{\\\"level\\\":\\\"info\\\",\\\"msg\\\":\\\"a\\\"}\"\nexit 1\n",
    );
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!(code, 1, "stderr: {stderr}");
    assert_eq!(stdout, "[INF] a\n");
    assert!(
        stderr.contains("command exited with status 1 after 1 line"),
        "{stderr}"
    );
}

/// "after 1 lines" would be the giveaway that nobody read the output. The
/// singular is part of the one message HLD §5 asks to be trustworthy.
#[test]
fn the_loud_line_says_line_not_lines_for_exactly_one() {
    let fake = Fake::new("status-singular");
    let stub = fake.script("one-then-fail", "echo only-line\nexit 3\n");
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!(code, 3, "stderr: {stderr}");
    assert_eq!(stdout, "only-line\n");
    assert!(
        stderr.contains("command exited with status 3 after 1 line\n"),
        "{stderr}"
    );
}

/// A command killed by a signal is not an exit status, and saying "exited with
/// status 137" would be a small lie in the one message meant to be trusted. The
/// code hog returns is 128 + the signal — what a shell would have reported.
#[test]
fn a_command_killed_by_a_signal_reports_the_signal_and_exits_128_plus_it() {
    let fake = Fake::new("signal");
    let stub = fake.script(
        "dies",
        "echo \"{\\\"level\\\":\\\"warn\\\",\\\"msg\\\":\\\"about to die\\\"}\"\nkill -9 $$\n",
    );
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!(code, 137, "stderr: {stderr}");
    assert_eq!(stdout, "[WRN] about to die\n");
    assert!(
        stderr.contains("command was killed by signal 9 after 1 line"),
        "{stderr}"
    );
}

/// The child's stderr is **inherited**, not captured: an ssh password prompt or
/// a host-key warning has to reach the user while it is happening. It must also
/// never be mistaken for a log line, so nothing of it lands on stdout.
#[test]
fn the_childs_stderr_reaches_the_user_and_never_the_rendered_stream() {
    let fake = Fake::new("child-stderr");
    let stub = fake.script(
        "noisy",
        "echo \"Permission denied (publickey).\" 1>&2\n\
         echo \"{\\\"level\\\":\\\"info\\\",\\\"msg\\\":\\\"connected anyway\\\"}\"\n",
    );
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["prod"], b"");

    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "[INF] connected anyway\n");
    assert!(
        stderr.contains("Permission denied (publickey)."),
        "the child's diagnostics must reach the user: {stderr:?}"
    );
}

/// The whitelist is checked while the plan is assembled, which is before
/// anything is spawned. Proved the only way it can be: the stub has a side
/// effect, and the side effect must be absent.
#[test]
fn an_argument_the_whitelist_refuses_never_spawns_the_command() {
    let fake = Fake::new("hostile-arg");
    let marker = fake.path.join("it-ran");
    let stub = fake.script("logs", &format!("touch {}\n", path_of(&marker)));
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["--", "prod; rm -rf /"], b"");

    assert_eq!(code, 1, "stderr: {stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("argument 1 contains characters that are not allowed"),
        "{stderr}"
    );
    assert!(
        !marker.exists(),
        "the command ran despite the argument being refused"
    );
}

/// Command mode goes through the same renderer and the same settings as stdin
/// mode: `-e` prunes a field out of a stream that came from a process.
#[test]
fn the_run_flags_apply_to_a_stream_that_came_from_a_command() {
    let fake = Fake::new("exclude");
    let stub = fake.script(
        "logs",
        "echo \"{\\\"level\\\":\\\"info\\\",\\\"msg\\\":\\\"up\\\",\\\"port\\\":8080,\\\"trace_id\\\":\\\"t1\\\"}\"\n",
    );
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["-e", "port", "prod"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(stdout, "[INF] up trace_id=t1\n");
}

/// `--config PATH` is a route into command mode too, and it wins over the
/// discovered file — the template comes from the file hog was told to read.
#[test]
fn an_explicit_config_supplies_the_template() {
    let fake = Fake::new("explicit-config");
    let discovered = fake.script("discovered", "echo wrong-one\n");
    fake.command(&path_of(&discovered));

    let chosen = fake.script("chosen", "echo right-one-$1\n");
    let config = fake.config_at(
        "elsewhere.toml",
        &format!("command = \"{} {{0}}\"\n", path_of(&chosen)),
    );

    let (stdout, stderr, code) = fake.run(
        &[
            "--config",
            config.to_str().expect("the temp path is UTF-8"),
            "prod",
        ],
        b"",
    );

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(stdout, "right-one-prod\n");
}

// ================================================== the broken-pipe path, 141

/// The flagship risk of this milestone. `hog … | head -2` has to exit 141 —
/// and the child it spawned has to be **dead**, not left streaming into a pipe
/// nobody reads. Nothing kills it for us: it is in hog's process group, but
/// `head` exiting sends no signal to anyone.
///
/// A termination signal aimed at **hog's pid alone** must not orphan a silent
/// child — for every catchable signal, not only SIGTERM.
///
/// HLD §5 handles SIGTERM and argues SIGINT needs no handler because Ctrl-C
/// reaches the whole process group. That argument is about group delivery and
/// stays true; it says nothing about `kill -INT <pid>`, which a person types all
/// the time. Measured before the fix: SIGINT, SIGHUP and SIGQUIT each ended hog
/// without unwinding and left `sleep` running with `ppid 1`, exactly the leak
/// the SIGTERM handler exists to prevent.
///
/// The child is silent on purpose. A child that writes dies on its own when
/// hog's read end disappears, so a noisy fixture would pass with no handler at
/// all and prove nothing.
#[test]
fn a_signal_aimed_at_hog_alone_never_orphans_a_silent_child() {
    for signal in ["TERM", "INT", "HUP", "QUIT"] {
        let fake = Fake::new(&format!("signal-{signal}"));
        let pidfile = fake.path.join("child.pid");
        let stub = fake.script(
            "quiet",
            &format!("echo $$ > {pid}\nexec sleep 30\n", pid = path_of(&pidfile)),
        );
        fake.command(&format!("{} {{0}}", path_of(&stub)));

        // `null`, not `piped`, and `wait` rather than `wait_with_output` — both
        // for the same reason, and it is the reason this test was worthless in
        // its first draft. An orphaned grandchild inherits hog's stdout pipe,
        // so reading that pipe to EOF waits for *the orphan*, not for hog:
        // with the leak present the test sat for `sleep`'s full duration, found
        // the orphan finally gone, and passed. Nothing may wait on that pipe.
        let mut child = fake
            .hog(&["prod"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("hog must start");

        // The stub writes its pid before `exec`, so the file appearing is also
        // the signal that hog got far enough to have something to leak.
        let grandchild = wait_for_pid_file(&pidfile).expect("the stub must record its pid");
        assert!(
            is_alive(&grandchild),
            "{signal}: the fixture child must run"
        );

        // `kill(1)` rather than `libc::kill`: this package denies `unsafe_code`
        // and does not depend on `libc`.
        let killed = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(child.id().to_string())
            .status()
            .expect("kill must run");
        assert!(killed.success(), "{signal}: kill failed");

        let status = child.wait().expect("hog must finish");
        // Died *of* the signal, so the shell still reports 128 + n.
        assert_eq!(
            status.code(),
            None,
            "{signal}: hog must die of the signal, not exit"
        );

        // The kill and the reap race the parent's exit, so give them a moment.
        for _ in 0..100 {
            if !is_alive(&grandchild) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !is_alive(&grandchild),
            "{signal}: pid {grandchild} outlived hog"
        );
    }
}

/// Polls for the stub's pid file, returning the pid it wrote.
fn wait_for_pid_file(path: &Path) -> Option<String> {
    for _ in 0..200 {
        if let Ok(text) = fs::read_to_string(path) {
            let pid = text.trim();
            if !pid.is_empty() {
                return Some(pid.to_owned());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}

/// This is also the test that justifies `panic = "abort"` being absent from the
/// release profile (HLD §7): `abort` would skip the `Drop`-guard entirely.
#[test]
fn a_closed_stdout_exits_141_and_the_child_does_not_survive_it() {
    let fake = Fake::new("broken-pipe");
    let pidfile = fake.path.join("child.pid");
    let stub = fake.script(
        "logs-forever",
        &format!(
            r#"echo $$ > {pid}
n=0
while :; do
  n=$((n + 1))
  echo "{{\"level\":\"info\",\"msg\":\"tick\",\"n\":$n}}"
  sleep 0.2
done
"#,
            pid = path_of(&pidfile)
        ),
    );
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let mut child = fake
        .hog(&["prod"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hog must start");

    // Read a little, then hang up — `head -2` going away.
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut first = [0u8; 32];
    let read = stdout.read(&mut first).expect("hog must produce a line");
    assert!(read > 0, "hog must have rendered something first");
    drop(stdout);

    let out = child.wait_with_output().expect("hog must finish");
    assert_eq!(
        out.status.code(),
        Some(141),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "a broken pipe must be quiet: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And now the part that only a real process can answer.
    let pid = fs::read_to_string(&pidfile).expect("the stub wrote its pid");
    let pid = pid.trim();
    assert!(!pid.is_empty(), "the stub wrote an empty pid file");
    assert!(
        !is_alive(pid),
        "the stub (pid {pid}) outlived hog: the Drop-guard did not kill it"
    );
}

/// The same guarantee for the child that is *hardest* to stop: one blocked
/// inside `write()` on a pipe nobody is draining any more. Closing hog's end of
/// its stdin is not enough to free it, so this is the case that proves the kill
/// actually happens rather than the child politely noticing and leaving.
///
/// It is also the shape of the real failure: `docker logs -f` on a busy service
/// never pauses, so by the time `head` hangs up the child is already blocked.
#[test]
fn a_child_blocked_writing_into_an_undrained_pipe_is_still_killed() {
    let fake = Fake::new("broken-pipe-blocked");
    let pidfile = fake.path.join("child.pid");
    let stub = fake.script(
        "logs-flat-out",
        &format!(
            r#"echo $$ > {pid}
n=0
while :; do
  n=$((n + 1))
  echo "{{\"level\":\"info\",\"msg\":\"flood\",\"n\":$n}}"
done
"#,
            pid = path_of(&pidfile)
        ),
    );
    fake.command(&format!("{} {{0}}", path_of(&stub)));

    let mut child = fake
        .hog(&["prod"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hog must start");

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut first = [0u8; 64];
    let read = stdout.read(&mut first).expect("hog must produce a line");
    assert!(read > 0, "hog must have rendered something first");
    drop(stdout);

    let out = child.wait_with_output().expect("hog must finish");
    assert_eq!(
        out.status.code(),
        Some(141),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `Session::drop` kills *and reaps*, both before hog returns, so the pid is
    // gone the moment hog is — no polling, and no window for a flaky answer.
    let pid = fs::read_to_string(&pidfile).expect("the stub wrote its pid");
    let pid = pid.trim();
    assert!(!pid.is_empty(), "the stub wrote an empty pid file");
    assert!(
        !is_alive(pid),
        "the flooding stub (pid {pid}) outlived hog: it was never killed"
    );
}

// ===================================================================== dry run

/// `--dry-run` prints the argv and runs **nothing** — proved by giving the stub
/// a side effect and finding it absent, which is the only proof that does not
/// rely on hog telling the truth about itself.
#[test]
fn dry_run_prints_the_argv_one_word_per_line_and_spawns_nothing() {
    let fake = Fake::new("dry-run");
    let marker = fake.path.join("it-ran");
    let stub = fake.script("logs", &format!("touch {}\n", path_of(&marker)));
    fake.command(&format!("{} -tt {{0}} {{1}}", path_of(&stub)));

    let (stdout, stderr, code) = fake.run(&["--dry-run", "prod", "api"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        format!("{}\n-tt\nprod\napi\n", path_of(&stub)),
        "one word per line, so the word boundaries are visible"
    );
    assert!(
        !marker.exists(),
        "--dry-run spawned the command it was asked only to print"
    );
}

/// The word boundaries are the whole question `--dry-run` answers: a quoted
/// remote command is one argv entry, not four, and `2>&1` inside the quotes
/// stays inside them.
#[test]
fn dry_run_shows_where_the_word_boundaries_fell() {
    let fake = Fake::new("dry-run-quoting");
    fake.command("ssh -tt {0} 'docker logs -f myapp-{1}-1 2>&1'");

    let (stdout, stderr, code) = fake.run(&["--dry-run", "prod", "api"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        "ssh\n-tt\nprod\n\"docker logs -f myapp-api-1 2>&1\"\n"
    );
}

/// `--dry-run` answers even when stdin is a pipe, which is where the naive
/// reading of the mode table goes wrong: row three says "read stdin", so hog
/// would sit there waiting for JSON under a flag whose `--help` promises it
/// prints and exits. Found by running it; see the module docs of `input`.
#[test]
fn dry_run_answers_instead_of_reading_a_pipe_on_stdin() {
    let fake = Fake::new("dry-run-piped");
    fake.command("kubectl logs -f -l app=api");

    // A pipe on stdin with bytes in it that hog must not wait for, and must
    // not render either.
    let (stdout, stderr, code) = fake.run(&["--dry-run"], b"{\"msg\":\"never read\"}\n");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(stdout, "kubectl\nlogs\n-f\n-l\napp=api\n");
}

/// The same invocation without `--dry-run` is row three again: stdin wins.
/// The pair is what pins the exception as an exception rather than a hole.
#[test]
fn without_dry_run_the_same_pipe_is_read() {
    let fake = Fake::new("dry-run-piped-control");
    fake.command("kubectl logs -f -l app=api");

    let (stdout, stderr, code) = fake.run(&[], b"{\"msg\":\"read after all\"}\n");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(stdout, "read after all\n");
}

/// And with nothing configured, `--dry-run` answers with the built-in rather
/// than refusing: the question is "what would run?", and since HLD §5 there is
/// always an answer to it.
#[test]
fn dry_run_without_a_template_shows_the_built_in() {
    let fake = Fake::new("dry-run-no-template");

    let (stdout, stderr, code) = fake.run(&["--dry-run", "prod", "api"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(stdout, "echo\nprod\napi\n");
}

/// The same with nothing to substitute either: `echo {@}` with no arguments is
/// a one-word argv, and `--dry-run` prints exactly that instead of the usage
/// error row six would give a real run.
#[test]
fn dry_run_with_neither_a_template_nor_arguments_still_answers() {
    let fake = Fake::new("dry-run-nothing-at-all");

    let (stdout, stderr, code) = fake.run(&["--dry-run"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(stdout, "echo\n");
    assert!(!stderr.contains("stdin is a terminal"), "{stderr}");
}

/// `--dry-run` refuses the same arguments a real run would, and before
/// spawning — the whitelist runs on the plan, not on the process.
#[test]
fn dry_run_still_refuses_an_argument_the_whitelist_rejects() {
    let fake = Fake::new("dry-run-hostile");
    fake.command("ssh {0}");

    let (stdout, stderr, code) = fake.run(&["--dry-run", "api; rm -rf /"], b"");

    assert_eq!(code, 1, "stderr: {stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("contains characters that are not allowed"),
        "{stderr}"
    );
}

/// `--dry-run` answers the question without needing the answer to be runnable:
/// a program that is not in `PATH` still prints, and still exits 0. Nothing is
/// spawned, so there is nothing to be "not found".
#[test]
fn dry_run_prints_an_argv_it_could_not_have_run() {
    let fake = Fake::new("dry-run-not-in-path");
    fake.command("hog-no-such-program-anywhere -tt {0}");

    let (stdout, stderr, code) = fake.run(&["--dry-run", "prod"], b"");

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(stdout, "hog-no-such-program-anywhere\n-tt\nprod\n");
}

/// The arity check runs before the spawn as well, so `--dry-run` with the
/// wrong number of arguments is the arity refusal rather than a printed argv
/// with a hole in it.
#[test]
fn dry_run_refuses_the_wrong_number_of_arguments() {
    let fake = Fake::new("dry-run-arity");
    fake.command("ssh {0} 'docker logs -f myapp-{1}-1'");

    let (stdout, stderr, code) = fake.run(&["--dry-run", "prod"], b"");

    assert_eq!(code, 1, "stderr: {stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("template needs 2 arguments, got 1"),
        "{stderr}"
    );
}

// ======================================================== the mode table, §6

/// All six rows of HLD §6, in order. The three that need a terminal go through
/// `script(1)`; the suite says so out loud and skips if it cannot get a pty,
/// rather than failing for a reason that has nothing to do with hog.
mod input_modes {
    use super::*;

    /// Row 1 — arguments, any stdin, a template: run it.
    #[test]
    fn arguments_with_a_template_run_the_command() {
        let fake = Fake::new("row1");
        let stub = fake.script("logs", "echo ran-with-$1\n");
        fake.command(&format!("{} {{0}}", path_of(&stub)));

        let (stdout, stderr, code) = fake.run(&["prod"], b"ignored\n");
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert_eq!(stdout, "ran-with-prod\n");
    }

    /// Row 1, the other half of "any stdin": the same invocation at an
    /// interactive prompt must run the command too, not fall through to the
    /// terminal gate. The mode is chosen by the arguments; the terminal only
    /// ever breaks the tie when there are none.
    #[test]
    fn arguments_with_a_template_run_the_command_on_a_terminal_too() {
        let fake = Fake::new("row1-tty");
        let stub = fake.script("logs", "echo ran-on-a-tty-$1\n");
        fake.command(&format!("{} {{0}}", path_of(&stub)));

        let Some((text, code)) = fake.run_on_a_pty(&["prod"]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };
        assert_eq!(code, 0, "got: {text}");
        assert!(text.contains("ran-on-a-tty-prod"), "got: {text}");
    }

    /// Row 2 — arguments, any stdin, no template: the built-in `echo {@}`
    /// runs, and stdin is still not read.
    ///
    /// This is the row HLD §5 rewrote: the old refusal is gone, and a
    /// brand-new install answers `hog prod api` with `prod api`. Note the
    /// stdin bytes: they must not appear, because row 2 is command mode and
    /// command mode never reads stdin.
    #[test]
    fn arguments_without_a_template_run_the_built_in_echo() {
        let fake = Fake::new("row2");

        let (stdout, stderr, code) = fake.run(&["prod", "api"], b"{\"msg\":\"unread\"}\n");
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert_eq!(stdout, "prod api\n");
    }

    /// Row 2 on a terminal, which is the row most easily confused with row 6:
    /// arguments were typed, so this is **not** "nothing to read" — the
    /// built-in runs and exits 0, and the usage error stays on row 6.
    #[test]
    fn arguments_without_a_template_on_a_terminal_are_not_the_usage_error() {
        let fake = Fake::new("row2-tty");

        let Some((text, code)) = fake.run_on_a_pty(&["prod", "api"]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };
        assert_eq!(code, 0, "got: {text}");
        assert!(text.contains("prod api"), "got: {text}");
        assert!(
            !text.contains("stdin is a terminal"),
            "row 6's message, on row 2: {text}"
        );
    }

    /// Row 3 — no arguments and a pipe on stdin: **read stdin**, even though a
    /// template is configured. The template must not hijack `cat x.log | hog`.
    #[test]
    fn a_pipe_on_stdin_wins_over_a_configured_template() {
        let fake = Fake::new("row3");
        let stub = fake.script("logs", "echo from-the-command\n");
        fake.command(&path_of(&stub));

        let (stdout, stderr, code) = fake.run(&[], b"{\"msg\":\"from stdin\"}\n");
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert_eq!(stdout, "from stdin\n");
    }

    /// Row 4 — no arguments, a terminal, a template with no `{N}`: run it.
    /// `command = "kubectl logs -f -l app=api"` plus a bare `hog` is the case
    /// HLD §6 calls out by name.
    #[test]
    fn a_terminal_and_a_template_without_placeholders_runs_it() {
        let fake = Fake::new("row4");
        let stub = fake.script("logs", "echo no-placeholders-needed\n");
        fake.command(&path_of(&stub));

        let Some((text, code)) = fake.run_on_a_pty(&[]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };
        assert_eq!(code, 0, "got: {text}");
        assert!(text.contains("no-placeholders-needed"), "got: {text}");
    }

    /// Row 5 — no arguments, a terminal, a template that needs some: the arity
    /// error, in exactly the words HLD §6 spells out.
    #[test]
    fn a_terminal_and_a_template_with_placeholders_is_an_arity_error() {
        let fake = Fake::new("row5");
        fake.command("ssh {0} 'docker logs -f myapp-{1}-1'");

        let Some((text, code)) = fake.run_on_a_pty(&[]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };
        assert_eq!(code, 1, "got: {text}");
        assert!(
            text.contains("template needs 2 arguments, got 0"),
            "got: {text}"
        );
    }

    /// Row 6 — no arguments, a terminal, no template: the short help and exit
    /// 2. The row that stops a bare `hog` waiting forever for typed JSON.
    #[test]
    fn a_bare_terminal_with_no_template_prints_usage_and_exits_two() {
        let fake = Fake::new("row6");

        let Some((text, code)) = fake.run_on_a_pty(&[]) else {
            eprintln!("skipped: `script` is not available to make a pty");
            return;
        };
        assert_eq!(code, 2, "got: {text}");
        assert!(text.contains("stdin is a terminal"), "got: {text}");
        assert!(text.contains("Usage: hog"), "got: {text}");
    }
}
