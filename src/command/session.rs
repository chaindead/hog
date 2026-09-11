//! The impure half of command mode: one child process, from spawn to reaping.
//!
//! Everything security-critical about *what* runs was settled by [`super::plan`]
//! before a byte of this file executes; a [`CommandPlan`] is already an argv, so
//! this module never parses, quotes or concatenates anything. What it owns is
//! the process lifecycle, and HLD §5 pins every part of it:
//!
//! * **stdout is piped.** It is the log stream, and it is the only thing hog
//!   reads.
//! * **stdin is piped and the handle is held.** `Child::wait` documents that it
//!   drops the child's stdin before waiting; for `ssh -tt` that EOF is a hang-up
//!   and the remote `docker logs -f` stops. Taking the handle out of the `Child`
//!   (`let stdin = child.stdin.take();`) is what prevents `wait` from closing
//!   it. hog never writes to it — it exists to stay open.
//! * **stderr is inherited.** An ssh password prompt, a host-key warning or
//!   `Permission denied` has to reach the user *while it is happening*, not
//!   after the run. It also means the child's diagnostics interleave with hog's
//!   own, which is correct: they are about the same failure.
//! * **a `Drop`-guard kills and reaps.** Dropping a `std::process::Child` does
//!   **not** kill the child and does **not** reap it — the default is an orphan
//!   plus a zombie. Every path out of hog therefore has to go through this
//!   `Drop`, which is precisely why `panic = "abort"` is banned in the release
//!   profile (`Cargo.toml`, HLD §7): `abort` skips unwinding, skips this
//!   `Drop`, and leaves `docker logs -f` running on the production host.
//!
//! # Signals
//!
//! **A signal delivered to the process group needs no handler.** The child is in
//! hog's group — hog never calls `setpgid` — so Ctrl-C, a dying terminal's
//! SIGHUP and `kill -TERM -<pgid>` from systemd or a container runtime all reach
//! both processes at the same instant. Verified on a pty: `^C` leaves neither
//! hog nor the child behind, for a child that prints and for one that never
//! prints at all.
//!
//! **A signal aimed at hog's pid alone is handled**, because nothing else covers
//! it. `kill -TERM <hog>` with no handler ends the process without unwinding, so
//! the `Drop` above never runs and the child is inherited by pid 1. A child that
//! *writes* dies on its own moments later — the read end of its stdout pipe is
//! gone, so the next write is `EPIPE`/`SIGPIPE` — but a **silent** child (the
//! quiet container HLD §5 wants `-tt` for) has no next write and survives
//! indefinitely. Reproduced on a release build: a `sleep 600` child outlived
//! `kill -TERM` on hog with `ppid=1`.
//!
//! HLD §5 names only SIGTERM there, but the leak is a property of *ending
//! without unwinding*, not of that one number: `kill -INT`, `kill -HUP` and
//! `kill -QUIT` on the same pid orphaned the same silent child in the same way,
//! each measured. [`FATAL_SIGNALS`] is therefore every catchable signal whose
//! default action ends the process, and the table of measurements is recorded
//! there. This does not contradict the paragraph above — a group Ctrl-C still
//! kills the child by itself, and the handler simply finds nothing left to do.
//!
//! The handler is a thread blocked on `signal_hook`'s self-pipe, armed for the
//! life of the session ([`arm_sigterm`]). It does one thing: kill and reap the
//! child, then die of **the signal that arrived**, so the shell still reports
//! 143 for a SIGTERM and 130 for a Ctrl-C. It has to be a thread rather than a
//! flag the main loop checks, because the main loop is blocked reading a pipe
//! that a silent child will never write to.
//!
//! SIGKILL remains what it always is: uncatchable, and therefore still a way to
//! orphan the child. That is a property of the signal, not a gap in this file.
//!
//! # What the guard does not cover
//!
//! **A grandchild.** The kill goes to the **direct child**, not to a process
//! group: putting the child in one of its own needs `setpgid`, and doing so
//! would take the child out of hog's group, where Ctrl-C reaches it.
//! For the case command mode exists for that is exactly right — `ssh` *is* the
//! direct child, and killing it tears the remote channel down, which is what
//! stops the remote `docker logs -f` — but a template that wraps a shell around
//! a long-running program (`sh -c 'setup; tail -f x'`) can leave the inner
//! program orphaned, because the shell forks it rather than `exec`ing it. The
//! fix is in the template, not here: `exec` the streaming program
//! (`sh -c 'setup; exec tail -f x'`), or name it directly. README material.

use std::io;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::Context as _;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;

use crate::command::CommandPlan;
use crate::error::Error;
use crate::input::{Input, Source};

/// The child process, reachable from both the main thread and the SIGTERM
/// thread.
///
/// `None` means "already reaped, or never spawned", which is what makes every
/// operation below idempotent: `finish` and `Drop` and the signal handler all
/// empty the slot, and whichever of them runs second finds nothing to do.
type ChildSlot = Arc<Mutex<Option<Child>>>;

/// How long to wait before asking a child that has closed its stdout whether it
/// has exited yet, and the ceiling that interval backs off to.
///
/// See [`wait_for_exit`] for why this polls rather than blocking in `wait`.
const POLL_FIRST: Duration = Duration::from_micros(200);
const POLL_LIMIT: Duration = Duration::from_millis(20);

/// A running command and the stream it produces.
///
/// Owns the child, its held-open stdin and the reader over its stdout, so that
/// "the process is alive" and "there are bytes to read" cannot get separated:
/// a caller holding the stream necessarily holds the guard that will kill the
/// process it came from.
#[derive(Debug)]
pub(crate) struct Session {
    child: ChildSlot,
    /// The child's stdin. Never written to; see the module docs.
    ///
    /// An `Option` only so that [`Drop`] can close it before killing, which
    /// unsticks a child that is blocked writing into a pipe nobody drains.
    stdin: Option<ChildStdin>,
    /// The child's stdout, wrapped in the same reader stdin mode uses.
    input: Input,
}

/// Spawns the command in `plan`.
///
/// # Errors
///
/// [`Error::CommandNotFound`] (exit 127) when the OS cannot find the program in
/// `PATH`, which is `io::ErrorKind::NotFound` from `spawn`. Every other spawn
/// failure — most often `PermissionDenied` for a file that exists but is not
/// executable — is an ordinary runtime error with context, exit 1.
pub(crate) fn spawn(plan: &CommandPlan) -> anyhow::Result<Session> {
    let child: ChildSlot = Arc::new(Mutex::new(None));

    // Armed *before* anything is spawned, and the slot's lock is then held
    // across the spawn, so there is no instant in which a child exists that the
    // handler cannot reach: a SIGTERM arriving mid-`spawn` waits on the lock and
    // finds the child the moment it is recorded.
    arm_sigterm(&child);

    let (stdin, stdout) = {
        let mut slot = held(&child);

        let mut spawned = Command::new(plan.program())
            .args(plan.args())
            // Held open on purpose. `Stdio::null()` would be an immediate EOF,
            // and `Stdio::inherit()` would let the child eat the terminal's
            // input.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| spawn_failed(err, plan))?;

        let stdin = spawned.stdin.take();
        let stdout = spawned.stdout.take();
        *slot = Some(spawned);
        (stdin, stdout)
    };

    let Some(stdout) = stdout else {
        // Unreachable: stdout was just configured as a pipe. Reached anyway,
        // the child is already running, so it is killed rather than leaked.
        kill_and_reap(&child);
        anyhow::bail!("the command was spawned with a piped stdout but has none");
    };

    Ok(Session {
        child,
        stdin,
        input: Input::new(Source::Command(stdout)),
    })
}

impl Session {
    /// The log stream, for [`crate::pipeline::run`].
    pub(crate) fn input(&mut self) -> &mut Input {
        &mut self.input
    }

    /// Waits for the child and turns its exit status into hog's.
    ///
    /// `lines` is how many lines hog rendered, and it is not decoration: HLD §5
    /// requires `command exited with status 255 after 1423 lines` so that a
    /// dropped VPN cannot masquerade as "the log ended". The count lives in the
    /// pipeline, so it has to come in from outside.
    ///
    /// Called only after the stream has ended, so the child has already closed
    /// its stdout and is on its way out. Calling it twice is harmless, and so is
    /// calling it after the SIGTERM handler has reaped the child: an empty slot
    /// is simply nothing to report.
    ///
    /// # Errors
    ///
    /// [`Error::CommandExit`] or [`Error::CommandSignal`] when the command did
    /// not succeed; both carry the exit code hog will propagate.
    pub(crate) fn finish(&mut self, lines: u64) -> anyhow::Result<()> {
        let waited = wait_for_exit(&self.child).context("waiting for the command to exit")?;
        let Some(status) = waited else {
            return Ok(());
        };

        Ok(triage(status, lines)?)
    }
}

impl Drop for Session {
    /// Kills and reaps, unless [`Session::finish`] already did.
    ///
    /// This is the whole reason hog returns `Error::BrokenPipe` as a value
    /// instead of calling `process::exit(141)`: an `exit` from inside the write
    /// path would never unwind past here, and the command would keep streaming
    /// into a pipe with no reader.
    fn drop(&mut self) {
        // Close our end of stdin first: a child blocked writing to a terminal
        // it no longer owns, or waiting on input, gets unstuck without needing
        // the signal to arrive first.
        drop(self.stdin.take());
        kill_and_reap(&self.child);
    }
}

/// Locks the slot, taking the child back out of a poisoned mutex rather than
/// panicking.
///
/// A panic while the lock was held would poison it, and the one thing that must
/// still happen after a panic is the kill. `unwrap` is a lint error outside
/// tests for exactly this class of reason: it would turn a recoverable state
/// into a second panic, this time inside a `Drop`.
fn held(child: &ChildSlot) -> MutexGuard<'_, Option<Child>> {
    child.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Kills the child and reaps it, leaving the slot empty.
///
/// Every result is discarded. A child that has already exited makes `kill`
/// fail, and there is nothing useful to say about it on the way out.
///
/// `wait` runs with the lock held, which is safe *because* the kill came first:
/// SIGKILL cannot be caught or ignored, so the wait returns rather than
/// blocking for as long as the child feels like living.
fn kill_and_reap(child: &ChildSlot) {
    let mut slot = held(child);
    if let Some(running) = slot.as_mut() {
        let _ = running.kill();
        // Without this the child becomes a zombie for as long as hog lives —
        // which, on the `| head` path, is long enough to matter.
        let _ = running.wait();
    }
    *slot = None;
}

/// Waits for the child to exit on its own, emptying the slot, and returns its
/// status — or `None` if something else had already reaped it.
///
/// Polls with `try_wait` instead of blocking in `wait`, and the reason is the
/// SIGTERM thread: `wait` would hold the slot's lock for as long as the child
/// took to exit, and a child that closes its stdout without exiting would make
/// hog *ignore* SIGTERM rather than merely leak on it. That trade is the wrong
/// way round — leaking a child is the bug this handler exists to fix, but a
/// process that cannot be terminated is a worse one.
///
/// The poll costs nothing in the normal case: the stream ends because the child
/// exited, so the first `try_wait` already has the status.
fn wait_for_exit(child: &ChildSlot) -> io::Result<Option<ExitStatus>> {
    let mut pause = POLL_FIRST;
    loop {
        {
            let mut slot = held(child);
            let Some(running) = slot.as_mut() else {
                return Ok(None);
            };
            if let Some(status) = running.try_wait()? {
                *slot = None;
                return Ok(Some(status));
            }
        }

        std::thread::sleep(pause);
        pause = (pause * 2).min(POLL_LIMIT);
    }
}

/// Starts the SIGTERM handler for `child`.
///
/// Failing to install it is **not** fatal. The handler turns "hog leaks the
/// command on `kill -TERM`" into "hog cleans up first"; refusing to run the
/// command at all because a sandbox would not let us open a self-pipe or call
/// `sigaction` would be a bigger regression than the leak it prevents. So the
/// failure is reported once, on stderr, and the run continues with v0.3's
/// behaviour.
#[cfg(unix)]
fn arm_sigterm(child: &ChildSlot) {
    if let Err(err) = install_sigterm(child) {
        // Not `eprintln!`: the `println!` family panics on a closed stream, and
        // a diagnostic must never be the thing that kills the run.
        use std::io::Write as _;
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "warning: could not handle termination signals ({err}); `kill` on hog will leave the command running"
        );
    }
}

/// No-op off unix: there is no `kill -TERM`, no pid 1 to inherit an orphan, and
/// `signal-hook` is not a dependency there (`Cargo.toml`).
#[cfg(not(unix))]
fn arm_sigterm(_child: &ChildSlot) {}

/// The signals that would otherwise end hog without unwinding.
///
/// HLD §5 names only SIGTERM, and its reasoning for leaving SIGINT alone is
/// sound *for the case it describes*: Ctrl-C is delivered to the whole
/// foreground process group, so the child gets it at the same instant hog does
/// and there is nothing to clean up. That is still true and still verified on a
/// pty.
///
/// What it does not cover is the same signal aimed at **hog's pid alone**, and
/// there the SIGTERM paragraph applies word for word — no unwinding, no `Drop`,
/// and a silent child inherited by pid 1. Measured on the release binary, with
/// the child in its own session so that only the pid named could be reached:
///
/// ```text
/// kill -TERM <hog>   hog exit -15   orphan probe CLEAN
/// kill -INT  <hog>   hog exit  -2   sleep 600 survives with ppid 1
/// kill -HUP  <hog>   hog exit  -1   sleep 600 survives with ppid 1
/// kill -QUIT <hog>   hog exit  -3   sleep 600 survives with ppid 1
/// ```
///
/// `kill -INT` on a pid is an ordinary thing for a person to type, and SIGHUP
/// is what a dying terminal sends. So the set is every catchable signal whose
/// default action is to end the process: the mechanism is the same one, and
/// handling three more costs one array literal.
///
/// SIGKILL is absent because it cannot be caught. That is a property of the
/// signal, not a gap here.
#[cfg(unix)]
const FATAL_SIGNALS: [std::ffi::c_int; 4] = [
    signal_hook::consts::SIGTERM,
    signal_hook::consts::SIGINT,
    signal_hook::consts::SIGHUP,
    signal_hook::consts::SIGQUIT,
];

/// Installs the handler: one registration, one thread, no unsafe on this side
/// of the wall.
///
/// The thread is deliberately **detached**. Stopping it when the session ends
/// would mean unregistering the hook, and `signal_hook_registry::unregister`
/// documents that it does *not* put the default action back — the process would
/// silently ignore these signals from then on, which is worse than the thread it
/// saves. hog spawns one command per run, so this is one thread that lives as
/// long as the process it is there to terminate.
#[cfg(unix)]
fn install_sigterm(child: &ChildSlot) -> anyhow::Result<()> {
    use signal_hook::iterator::Signals;

    let mut signals =
        Signals::new(FATAL_SIGNALS).context("registering the termination handlers")?;
    let child = Arc::clone(child);

    std::thread::Builder::new()
        .name("hog-signals".to_owned())
        .spawn(move || {
            // `forever` blocks and yields only what was registered above. The
            // loop outlives the first signal only if `leave` somehow returns.
            for signal in signals.forever() {
                kill_and_reap(&child);
                leave(signal);
            }
        })
        .context("starting the signal thread")
        .map(drop)
}

/// Ends hog the way the signal asked.
///
/// Restores that signal's default disposition and re-raises it, so hog really
/// does die *of* the signal and the shell reports 128 + n — HLD §5's 143 for
/// SIGTERM, and the 130 everyone expects from Ctrl-C — reached by being killed
/// rather than by claiming to have been. It is also the only way out that
/// `clippy.toml` leaves open, and for a good reason: `process::exit` is banned
/// there because it skips `Drop` and orphans the child, and the way to satisfy
/// that rule is to kill the child *first*, which the caller just did.
///
/// Re-raising **the signal that arrived** rather than a fixed SIGTERM is what
/// keeps that number honest: reporting 143 for a Ctrl-C would be a lie told by
/// the one mechanism whose whole job is an accurate exit status.
///
/// `emulate_default_handler` does not return for a terminating signal; it falls
/// back to `abort` if even the re-raise fails. If it ever did return, the child
/// is already dead, the stream is at EOF, and hog exits through its ordinary
/// path reporting the signal that killed the command.
#[cfg(unix)]
fn leave(signal: std::ffi::c_int) {
    let _ = signal_hook::low_level::emulate_default_handler(signal);
}

/// Classifies a failure to spawn.
///
/// `NotFound` is the one that earns its own exit code: 127 is what every shell
/// returns for "command not found", so a script around hog can tell a missing
/// `ssh` from an `ssh` that ran and failed.
fn spawn_failed(err: io::Error, plan: &CommandPlan) -> anyhow::Error {
    if err.kind() == io::ErrorKind::NotFound {
        return Error::CommandNotFound {
            program: plan.program().to_owned(),
            command: plan.to_string(),
        }
        .into();
    }

    anyhow::Error::new(err).context(format!("running {plan}"))
}

/// Turns an exit status into hog's own outcome.
///
/// Pure, and separate from [`Session::finish`] for exactly that reason: the
/// triage table is what the tests need to pin, and it needs no child process.
///
/// A command killed by a signal reports 128 + the signal, the number a shell
/// would have reported. HLD §5's "hog no longer tells 'could not connect' from
/// the remote command's own code" applies to the *value*, not to the shape: a
/// death by SIGKILL is still not an exit status, and saying "exited with status
/// 137" would be a small lie in the one message meant to be trustworthy.
fn triage(status: ExitStatus, lines: u64) -> Result<(), Error> {
    if status.success() {
        return Ok(());
    }

    if let Some(code) = status.code() {
        return Err(Error::CommandExit {
            status: code,
            lines,
        });
    }

    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        return Err(Error::CommandSignal { signal, lines });
    }

    // No code and no signal: not reachable on any platform hog builds for, but
    // "it failed" is still the honest answer.
    Err(Error::CommandExit { status: 1, lines })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::input::Line;
    use crate::settings::Settings;

    impl Session {
        /// The child's pid, or `None` once something has reaped it.
        ///
        /// Test-only: production code never needs to name the pid, because
        /// everything it does to the child goes through the slot.
        fn pid(&self) -> Option<u32> {
            held(&self.child).as_ref().map(Child::id)
        }
    }

    /// Builds a plan straight from a literal argv, by writing it as a template
    /// of single-quoted words.
    ///
    /// It goes through [`crate::command::plan`] rather than around it so the
    /// fixtures cannot drift from what the real path produces. `/bin/sh -c` is
    /// used as the *child* in several tests, which is the cheapest process that
    /// can be told to do something specific — it is a fixture, not the shape of
    /// command mode: hog spawns the argv directly and interposes no shell.
    fn argv(words: &[&str]) -> CommandPlan {
        let template = words
            .iter()
            .map(|word| format!("'{word}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let settings = Settings {
            command: Some(template),
            ..Settings::default()
        };
        crate::command::plan(&settings, &[]).expect("the fixture plan must assemble")
    }

    /// Reads the session's stream to the end, returning the lines as text.
    fn drain(session: &mut Session) -> Vec<String> {
        let mut buf = Vec::new();
        let mut lines = Vec::new();
        loop {
            match session.input().read_line(&mut buf).expect("a pipe reads") {
                Line::Eof => return lines,
                _ => lines.push(String::from_utf8_lossy(&buf).into_owned()),
            }
        }
    }

    /// `kill -0 <pid>` from a *separate* process: the only way to ask whether a
    /// pid we no longer own is alive.
    fn is_alive(pid: u32) -> bool {
        Command::new("/bin/sh")
            .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_successful_command_streams_its_stdout_and_exits_zero() {
        let mut session = spawn(&argv(&["/bin/sh", "-c", "printf a\\\\nb\\\\n"])).expect("spawns");
        assert_eq!(drain(&mut session), ["a", "b"]);
        session.finish(2).expect("exit 0 is not an error");
    }

    #[test]
    fn a_non_zero_exit_carries_its_code_and_the_line_count() {
        let mut session = spawn(&argv(&["/bin/sh", "-c", "echo one; exit 255"])).expect("spawns");
        assert_eq!(drain(&mut session), ["one"]);

        let err = session.finish(1).expect_err("255 is a failure");
        let domain = err.downcast_ref::<Error>();
        assert!(
            matches!(
                domain,
                Some(Error::CommandExit {
                    status: 255,
                    lines: 1
                })
            ),
            "got {err:?}"
        );
        // The loud line of HLD §5, in the singular.
        assert_eq!(
            err.to_string(),
            "command exited with status 255 after 1 line"
        );
        assert_eq!(domain.map(Error::exit_code), Some(255));
    }

    #[test]
    fn a_missing_program_is_exit_127_and_says_what_it_tried() {
        let err = spawn(&argv(&["hog-no-such-program-exists", "--flag"]))
            .expect_err("a missing program cannot spawn");

        let domain = err.downcast_ref::<Error>();
        assert!(
            matches!(domain, Some(Error::CommandNotFound { .. })),
            "got {err:?}"
        );
        assert_eq!(domain.map(Error::exit_code), Some(127));
        assert!(
            err.to_string().contains("hog-no-such-program-exists"),
            "{err}"
        );
    }

    /// A file that exists but is not executable is **not** 127: `PATH` found
    /// it. Conflating the two would send a script looking for a missing binary
    /// when the real answer is a missing `chmod +x`.
    #[test]
    fn an_unexecutable_file_is_a_plain_runtime_error() {
        let err = spawn(&argv(&["/etc/hosts"])).expect_err("not executable");
        assert!(
            err.downcast_ref::<Error>().is_none(),
            "must not claim a distinct exit code: {err:?}"
        );
        assert!(err.to_string().contains("running /etc/hosts"), "{err}");
    }

    /// The guard, which is the risk this whole file exists to manage: dropping
    /// an unfinished `Session` must leave neither an orphan nor a zombie.
    #[test]
    fn dropping_an_unfinished_session_kills_and_reaps_the_child() {
        // A child that would otherwise outlive hog by two minutes. `exec` so
        // that the pid hog holds *is* the sleep: without it the shell forks,
        // and the orphan it leaves behind is the module doc's caveat rather
        // than anything this test can assert about.
        let mut session =
            spawn(&argv(&["/bin/sh", "-c", "echo up; exec sleep 120"])).expect("spawns");

        let mut buf = Vec::new();
        assert_eq!(
            session.input().read_line(&mut buf).expect("a pipe reads"),
            Line::Full
        );
        assert_eq!(buf, b"up");

        let pid = session.pid().expect("the fixture child must be running");
        assert!(is_alive(pid), "the fixture child must be running");

        // Returning at all is half the assertion: a `Drop` that waited on a
        // child it had not killed would hang here for two minutes.
        drop(session);

        assert!(!is_alive(pid), "pid {pid} survived the Drop guard");
    }

    /// The same guard, reached by a **panic** instead of a return.
    ///
    /// This is the test that gives `panic = "abort"`'s absence from
    /// `[profile.release]` (`Cargo.toml`, HLD §7) something to fail against.
    /// With `abort` there is no unwind, so `Session::drop` never runs and the
    /// remote follower outlives the panic; with the default `unwind` the child
    /// is killed and reaped on the way out, exactly as on the `?` paths.
    ///
    /// libtest captures the panic message of a passing test, so the deliberate
    /// panic below is not printed unless this test actually fails.
    #[test]
    fn a_panic_unwinds_through_the_guard_and_still_kills_the_child() {
        let pid = std::cell::Cell::new(0_u32);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut session =
                spawn(&argv(&["/bin/sh", "-c", "echo up; exec sleep 120"])).expect("spawns");

            let mut buf = Vec::new();
            assert_eq!(
                session.input().read_line(&mut buf).expect("a pipe reads"),
                Line::Full
            );
            pid.set(session.pid().expect("spawned"));

            panic!("hog is going down with a live session");
        }));

        assert!(outcome.is_err(), "the closure must have panicked");
        let pid = pid.get();
        assert_ne!(pid, 0, "the child must have been spawned");
        assert!(!is_alive(pid), "pid {pid} survived a panic");
    }

    /// What the SIGTERM thread does when the signal arrives, minus the part
    /// that ends the process.
    ///
    /// The handler is two steps — [`kill_and_reap`] then [`leave`] — and only
    /// the first can be tested in-process: `leave` restores SIGTERM's default
    /// disposition and re-raises it, which would take the test binary with it.
    /// The whole handler is exercised end to end against the real binary in the
    /// live check recorded in the wave notes (`kill -TERM` on hog, then `pgrep`
    /// for a silent child).
    #[test]
    fn the_signal_handlers_half_kills_and_reaps_a_silent_child() {
        // Silent on purpose: a child that prints would die of EPIPE when hog's
        // read end went away, which is exactly the case that *never* needed a
        // handler. `exec`, so the pid in the slot is the sleep itself.
        let session = spawn(&argv(&["/bin/sh", "-c", "exec sleep 120"])).expect("spawns");
        let pid = session.pid().expect("the fixture child must be running");
        assert!(is_alive(pid));

        kill_and_reap(&session.child);

        assert!(!is_alive(pid), "pid {pid} survived the SIGTERM handler");
        assert!(
            session.pid().is_none(),
            "the slot must be empty, or Drop would wait on a reaped pid"
        );
        // And the `Drop` that follows must not hang or double-wait.
        drop(session);
    }

    /// `finish` is idempotent, so the `Drop` that follows it is a no-op rather
    /// than a `wait` on a pid that is no longer ours.
    #[test]
    fn finishing_twice_is_harmless() {
        let mut session = spawn(&argv(&["/bin/sh", "-c", "exit 0"])).expect("spawns");
        assert!(drain(&mut session).is_empty());
        session.finish(0).expect("exit 0");
        session.finish(0).expect("already reaped");
    }

    /// A child the SIGTERM handler got to first leaves `finish` nothing to
    /// report — and, above all, nothing to block on.
    #[test]
    fn finishing_after_the_handler_reaped_is_silent() {
        let mut session = spawn(&argv(&["/bin/sh", "-c", "exec sleep 120"])).expect("spawns");
        kill_and_reap(&session.child);
        session.finish(7).expect("an empty slot has nothing to say");
    }

    /// The `ssh -tt` requirement, from both sides: while the handle is held the
    /// child sees no EOF on stdin, and letting go of it is what delivers one.
    ///
    /// `Child::wait` drops the child's stdin before waiting, so without the
    /// `take` in `spawn` the "held" half of this could not hold.
    #[test]
    fn the_held_stdin_handle_is_what_keeps_the_child_from_seeing_eof() {
        let mut session = spawn(&argv(&[
            "/bin/sh",
            "-c",
            "echo up; if read -r line; then echo got; else echo eof; fi",
        ]))
        .expect("spawns");

        let mut buf = Vec::new();
        assert_eq!(
            session.input().read_line(&mut buf).expect("a pipe reads"),
            Line::Full
        );
        assert_eq!(buf, b"up");
        assert!(
            session.stdin.is_some(),
            "the handle must be out of the Child, or wait() would close it"
        );

        // Let go of it: now — and only now — the child's `read` hits EOF.
        drop(session.stdin.take());
        assert_eq!(
            session.input().read_line(&mut buf).expect("a pipe reads"),
            Line::Full
        );
        assert_eq!(buf, b"eof");

        session.finish(2).expect("exit 0");
    }

    /// stderr is inherited, not captured: nothing the child writes there can
    /// reach the log stream hog renders.
    #[test]
    fn the_childs_stderr_never_lands_in_the_stream() {
        let mut session = spawn(&argv(&[
            "/bin/sh",
            "-c",
            "echo out; echo noise 1>&2; echo out2",
        ]))
        .expect("spawns");
        assert_eq!(drain(&mut session), ["out", "out2"]);
        session.finish(2).expect("exit 0");
    }

    /// The stream is the child's raw stdout: no pty, so no ONLCR, and a final
    /// line without a newline is still a line.
    #[test]
    fn the_stream_is_the_raw_pipe() {
        let mut session =
            spawn(&argv(&["/bin/sh", "-c", "printf no-trailing-newline"])).expect("spawns");
        assert_eq!(drain(&mut session), ["no-trailing-newline"]);
        session.finish(1).expect("exit 0");
    }

    /// `wait_for_exit` polls, so a child that takes its time still reports the
    /// status it eventually exits with — and the slot is empty afterwards.
    #[test]
    fn waiting_survives_a_child_that_exits_after_its_stdout_closes() {
        // Closes stdout immediately (`exec 1>&-`) and exits a moment later, so
        // the first `try_wait` necessarily returns `None`.
        let mut session = spawn(&argv(&[
            "/bin/sh",
            "-c",
            "echo up; exec 1>&-; sleep 0.2; exit 3",
        ]))
        .expect("spawns");
        assert_eq!(drain(&mut session), ["up"]);

        let err = session.finish(1).expect_err("exit 3 is a failure");
        assert!(
            matches!(
                err.downcast_ref::<Error>(),
                Some(Error::CommandExit {
                    status: 3,
                    lines: 1
                })
            ),
            "got {err:?}"
        );
        assert!(
            session.pid().is_none(),
            "the slot must be empty after a wait"
        );
    }

    #[test]
    fn triage_maps_the_three_outcomes() {
        let status = |script: &str| {
            Command::new("/bin/sh")
                .args(["-c", script])
                .status()
                .expect("sh runs")
        };

        assert!(triage(status("exit 0"), 0).is_ok());

        assert!(matches!(
            triage(status("exit 3"), 7),
            Err(Error::CommandExit {
                status: 3,
                lines: 7
            })
        ));

        // A signal death is reported as such, and as 128 + the signal — the
        // number a shell would have reported for the same death.
        let err = triage(status("kill -9 $$"), 4).expect_err("a signal death is a failure");
        assert!(
            matches!(
                err,
                Error::CommandSignal {
                    signal: 9,
                    lines: 4
                }
            ),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 137);
        assert_eq!(
            err.to_string(),
            "command was killed by signal 9 after 4 lines"
        );
    }
}
