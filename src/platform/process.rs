//! Process groups: spawning a child that owns one, and killing all of it.
//!
//! Every subprocess Wizard starts is really a *tree*. `sh -c` may fork the
//! command rather than exec it (dash does), `cargo test` forks rustc, build
//! scripts and the test binaries, and a background task can spawn whatever it
//! likes. Killing the child Wizard spawned therefore kills the least
//! interesting process in the tree, and leaves the rest running with the
//! output pipes still open, which is what used to hang a timed-out tool call
//! until the orphan finished.
//!
//! Unix answers with a process group: the child is made its own group leader
//! at spawn, and `kill(-pgid)` then reaches everything it forked. Windows has
//! no process groups in this sense; the equivalent is a Job Object that the
//! child is assigned to at creation and that can be terminated wholesale.
//! Both are "put the child somewhere killable, then kill the whole place",
//! which is why the surface here is a spawn-side trait and a kill-side pair of
//! functions rather than anything with a pid group in its signature.
//!
//! [`exec_replace`] is the other end of the same subject: not a child at all,
//! but this process becoming a different program.

use anyhow::Result;

/// Spawn configuration: put the child in a killable group of its own.
///
/// A trait rather than a free function so it composes into the builder chains
/// the call sites already have (`.stdin(..).own_process_group().spawn()`), and
/// so the blocking and async `Command` types get one spelling between them.
pub trait ProcessGroupExt {
    /// Make the child its own process-group leader (Unix), so a later
    /// [`kill_group`] or [`terminate_group`] on its pid reaches every process
    /// it forks. No-op where the platform has no equivalent yet.
    fn own_process_group(&mut self) -> &mut Self;
}

impl ProcessGroupExt for std::process::Command {
    fn own_process_group(&mut self) -> &mut Self {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // 0 means "a new group whose id is the child's pid", so the pid
            // the caller already holds is also the group to signal.
            self.process_group(0);
        }
        self
    }
}

impl ProcessGroupExt for tokio::process::Command {
    fn own_process_group(&mut self) -> &mut Self {
        #[cfg(unix)]
        self.process_group(0);
        self
    }
}

/// SIGKILL every process in the group led by `leader`.
///
/// `leader` is the pid of a child spawned with [`ProcessGroupExt::own_process_group`];
/// without that, the signal goes to whatever group the child inherited, which
/// on Unix is Wizard's own. Callers still reap the child themselves (tokio's
/// `Child::kill`, or a `wait`): this only delivers the signal.
///
/// A pid that is already gone fails with ESRCH, which is not an error worth
/// reporting; the caller's goal was for it to be gone.
pub fn kill_group(leader: u32) {
    #[cfg(unix)]
    {
        // SAFETY: `kill` is async-signal-safe and borrows nothing from us; the
        // only failure modes are ESRCH (already dead) and EPERM (not ours),
        // neither of which the caller can act on.
        unsafe { libc::kill(-(leader as i32), libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    {
        // Windows: terminate the Job Object the child was assigned to.
        let _ = leader;
    }
}

/// SIGTERM the group led by `leader`, falling back to the single process when
/// there is no such group.
///
/// The polite half of the pair, for things a user asked to stop (a dispatched
/// background session) rather than things that ran out of time. The fallback
/// matters because the pid may belong to a process that was never made a group
/// leader, in which case `kill(-pid)` fails with ESRCH and nothing is
/// signalled at all.
pub fn terminate_group(leader: u32) {
    #[cfg(unix)]
    {
        // SAFETY: as in `kill_group`.
        unsafe {
            if libc::kill(-(leader as i32), libc::SIGTERM) != 0 {
                libc::kill(leader as i32, libc::SIGTERM);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = leader;
    }
}

/// Whether this process is in the foreground process group of its controlling
/// terminal, i.e. whether it is the thing the keyboard is currently talking to.
///
/// The question behind "may I park a thread on stdin and ask the user
/// something?". `isatty` is not enough on its own: a backgrounded job still
/// has a terminal on fd 0, and reading from it earns a `SIGTTIN` that stops
/// the process, so a trust prompt would hang a job the user had deliberately
/// pushed into the background rather than asking them anything.
///
/// `true` where the platform has no such notion, which is the answer that
/// leaves the caller's other checks (a declared console, a terminal on both
/// ends) in charge. Windows has no process groups in this sense; the nearest
/// question there is whether this process owns the console, and until that is
/// written this must not be the thing that refuses.
pub fn in_foreground() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: both are plain getters on the calling process; neither takes
        // a pointer nor mutates any state.
        let (foreground, ours) = unsafe { (libc::tcgetpgrp(libc::STDIN_FILENO), libc::getpgrp()) };
        // `tcgetpgrp` returns -1 with ENOTTY when fd 0 is not a terminal at
        // all, which is not this function's question: the caller's `isatty`
        // check has already answered it.
        foreground >= 0 && foreground == ours
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Become `binary`: replace this process's image, keeping its pid, its open
/// descriptors and its controlling terminal.
///
/// Deep evolve ends here. It has just installed a rebuilt wizard over the one
/// that is running, and the user is sitting in front of a TUI that has to come
/// back as the new build. Spawning a child and exiting would work on paper and
/// not in practice: the shell that started Wizard would see the parent exit
/// and print a prompt over the child's screen, `Ctrl-C` would go to the wrong
/// process, and a wrapper that waits on Wizard's pid (a supervisor, `time`, a
/// CI step) would be told the run finished. `exec` has none of those problems
/// because there is no second process.
///
/// Returns only on failure, which is why the success type is uninhabited: a
/// caller cannot accidentally write code after this that it expects to run.
///
/// Windows has no `exec`. The port is not another call, it is a different
/// shape (spawn, hand over the console, exit), and doing it wrong is worse
/// than not doing it, so that arm reports where the new binary is waiting and
/// leaves the running process alone.
pub fn exec_replace(binary: &std::path::Path) -> Result<std::convert::Infallible> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` replaces the image on success, so anything it returns is the
        // failure: a binary that is not executable, the wrong architecture, a
        // missing interpreter.
        let err = std::process::Command::new(binary).exec();
        restore_sigpipe_ignore();
        Err(anyhow::Error::new(err)
            .context(format!("failed to exec-replace with {}", binary.display())))
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!(
            "exec-replace is only supported on Unix; the new binary is staged at {}",
            binary.display()
        )
    }
}

/// Put `SIGPIPE` back to `SIG_IGN` after an exec that did not happen.
///
/// [`exec_replace`] does not fork. The standard library ignores `SIGPIPE` at
/// startup, and then restores the default disposition immediately before
/// `execvp` so the new program does not inherit that oddity. For `spawn` that
/// restore happens in the forked child and nothing here notices; for `exec`
/// there is no child, so it happens in *this* process. On success the image is
/// gone and it is exactly right. On failure the call returns, and without this
/// the process keeps running with `SIGPIPE` defaulted.
///
/// The consequence is not theoretical and not confined to the failure being
/// reported. From that point on, any write to a pipe whose reader has gone
/// kills the process outright instead of returning `EPIPE` — the behaviour
/// [`crate::output`] relies on to survive `wizard ... | head`. It reached CI
/// as a suite that died mid-run with signal 13, in a different place each
/// time, because which test wrote to a closed pipe first depends on how the
/// harness scheduled them.
///
/// Deep evolve is the caller that matters: it exec-replaces into a newly built
/// binary, and the whole point of the error path is that a failed swap leaves
/// a *working* agent behind.
#[cfg(unix)]
fn restore_sigpipe_ignore() {
    // SAFETY: `signal` with `SIG_IGN` installs no handler, so there is no
    // async-signal-safety obligation on a callback, and it borrows nothing.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_child_configured_this_way_leads_a_group_of_its_own() {
        // The property the kill side depends on: the child's pid *is* a group
        // id. Without it, `kill(-pid)` has nothing to signal (and the caller's
        // pid belongs to Wizard's own group, which is the accident this
        // prevents).
        let mut child = crate::platform::shell::command("sleep 1")
            .own_process_group()
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let group = unsafe { libc::getpgid(pid as i32) };
        assert_eq!(group, pid as i32, "the child must lead its own group");

        // A child spawned without it inherits ours, which is why the call is
        // not optional at the sites that later kill by group.
        let mut plain = crate::platform::shell::command("sleep 1")
            .spawn()
            .expect("spawn");
        let inherited = unsafe { libc::getpgid(plain.id() as i32) };
        assert_eq!(inherited, unsafe { libc::getpgid(0) });

        kill_group(pid);
        let _ = child.wait();
        let _ = plain.kill();
        let _ = plain.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_group_kill_reaches_a_grandchild_the_shell_forked() {
        use std::process::Stdio;
        use std::time::Duration;

        // The failure this guards: `sh -c` forks the real command, so killing
        // the shell alone leaves the grandchild running and holding the output
        // pipes open. The grandchild here writes a marker file one second in;
        // the assertion happens well after that, so a survivor is visible.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("survived");
        let script = format!("( sleep 1; : > '{}' ) & wait", marker.display());

        let mut command = crate::platform::shell::tokio_command(&script);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .own_process_group();
        let mut child = command.spawn().expect("spawn");
        let pid = child.id().expect("the child has a pid");

        // Let the shell get as far as forking the subshell.
        tokio::time::sleep(Duration::from_millis(300)).await;
        kill_group(pid);
        let _ = child.kill().await;

        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            !marker.exists(),
            "the grandchild outlived the group kill and wrote {}",
            marker.display()
        );
    }

    /// Environment variable naming the script the child half of
    /// [`exec_replace_becomes_the_named_binary_in_the_same_process`] should
    /// become. Inert unless the parent sets it, because this "test" ends by
    /// replacing the process running it, which would take the whole suite with
    /// it.
    #[cfg(unix)]
    const EXEC_PROBE_ENV: &str = "WIZARD_PLATFORM_EXEC_PROBE";

    #[cfg(unix)]
    #[test]
    fn exec_probe() {
        let Some(script) = std::env::var_os(EXEC_PROBE_ENV) else {
            return;
        };
        // Printed *before* the exec and read by the parent afterwards: same
        // stream, same process, which is the whole claim.
        println!("exec-probe pid={}", std::process::id());
        let err = exec_replace(std::path::Path::new(&script))
            .expect_err("exec_replace returns only on failure");
        panic!("the exec did not happen: {err:#}");
    }

    #[cfg(unix)]
    #[test]
    fn exec_replace_becomes_the_named_binary_in_the_same_process() {
        // A spawn-and-exit would also "run the new binary", and would break
        // every wrapper watching Wizard's pid. What distinguishes the two is
        // that after an exec the pid, the exit status and the open stdout all
        // still belong to the process the parent started, so the assertions
        // are on exactly those three.
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("replacement");
        std::fs::write(
            &script,
            format!(
                "{}\necho exec-probe replaced\nexit 7\n",
                crate::platform::shell::shebang()
            ),
        )
        .expect("write the replacement");
        crate::platform::exe_swap::set_executable(&script).expect("chmod");

        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "platform::process::tests::exec_probe",
                "--nocapture",
            ])
            .env(EXEC_PROBE_ENV, &script)
            .output()
            .expect("run the exec probe");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

        assert_eq!(
            output.status.code(),
            Some(7),
            "the replacement's exit status must be the process's:\n{stdout}"
        );
        assert!(
            stdout.contains("exec-probe pid="),
            "the pre-exec line is missing, so nothing below is evidence:\n{stdout}"
        );
        assert!(
            stdout.contains("exec-probe replaced"),
            "the replacement never ran on the inherited stdout:\n{stdout}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exec_replace_reports_a_binary_it_could_not_become() {
        // The only way this returns at all. The message has to name the path,
        // because the caller (deep evolve) has just installed that file and
        // the user's next question is which one failed.
        let dir = tempfile::tempdir().expect("tempdir");
        let absent = dir.path().join("never-built");
        let err = exec_replace(&absent).expect_err("a missing binary cannot be exec'd");
        assert!(
            format!("{err:#}").contains(&absent.display().to_string()),
            "got: {err:#}"
        );
    }

    /// The failure path must not leave the process a different one than it
    /// found. `exec` resets `SIGPIPE` to the default in this very process
    /// before `execvp`, and when the exec does not happen that reset stands:
    /// the next write to a closed pipe kills a process that was supposed to
    /// get `EPIPE`. This ran as a suite dying with signal 13 somewhere else
    /// entirely, so the assertion belongs next to the call that caused it.
    #[cfg(unix)]
    #[test]
    fn a_failed_exec_leaves_sigpipe_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absent = dir.path().join("never-built");
        exec_replace(&absent).expect_err("a missing binary cannot be exec'd");

        // Read the disposition without disturbing it: a null `act` queries.
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: a null action pointer only reads, and `current` is a valid
        // writable `sigaction` for the duration of the call.
        let rc = unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut current) };
        assert_eq!(rc, 0, "querying the SIGPIPE disposition failed");
        assert_eq!(
            current.sa_sigaction,
            libc::SIG_IGN,
            "a failed exec left SIGPIPE defaulted, so a closed pipe now kills the process"
        );
    }

    #[cfg(unix)]
    #[test]
    fn signalling_a_pid_that_is_not_a_group_leader_still_reaches_it() {
        use std::time::{Duration, Instant};

        // `terminate_group`'s fallback: a child that was never made a leader
        // has no group of its own, so `kill(-pid)` fails and the bare pid is
        // the only thing that works. Without the fallback this child would run
        // for its full sleep.
        let mut child = crate::platform::shell::command("sleep 30")
            .spawn()
            .expect("spawn");
        let started = Instant::now();
        terminate_group(child.id());
        let status = child.wait().expect("wait");
        assert!(!status.success(), "the child should have been signalled");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the signal never reached the child"
        );
    }
}
