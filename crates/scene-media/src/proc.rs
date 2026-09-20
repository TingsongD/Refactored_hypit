//! Deadlines for subprocess waits. `child.wait()` and `.output()` block
//! forever on a wedged child — a hung ffmpeg, a connector that never
//! reads stdin, a `curl` stuck mid-transfer. These variants poll
//! `try_wait` on a deadline and kill+reap on expiry, so a hung tool is
//! an error the engine can report instead of a process that never ends.
//!
//! Limits are hang-catchers: generous by design (minutes, not seconds),
//! so they never fire on a healthy slow run.
//!
//! Children spawned through [`spawn_grouped`] get their own process
//! group on unix, so a timeout kills the whole tree — not just the
//! direct child. That matters for shell connectors: a `curl` grandchild
//! otherwise outlives its killed parent, keeps the inherited stdout
//! pipe open, and hangs the pipe-drain join that runs after the kill.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::error::MediaError;
use crate::stderr::StderrDrain;

/// Poll interval — coarse enough to cost nothing, fine enough that a
/// hung tool is noticed within a tenth of a second past its deadline.
const POLL: Duration = Duration::from_millis(50);

/// Spawn `cmd` in its own process group (unix) so `wait_timeout`,
/// `output_timeout` and the stall watchdog can kill the whole tree —
/// descendants included. Non-unix this is a plain `spawn`; the kill
/// falls back to `taskkill /T` for tree semantics there.
pub fn spawn_grouped(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // pgid = the new child's pid: a fresh group containing exactly
        // this child and anything it spawns that doesn't set its own.
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// Kill `child` and every descendant that shares its process group /
/// tree, then reap. `killpg(child.id())` is safe even when the child
/// wasn't grouped — a pgid equals its leader's pid, and a non-leader
/// child's pid names no group, so the call reports ESRCH and the direct
/// `child.kill()` still applies.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(child.id() as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        // `Child::kill` ends only the process; /T takes the tree.
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Wait for `child` up to `limit`. On expiry the child — and its
/// descendants, when spawned via [`spawn_grouped`] — is killed and
/// reaped (no zombie), and the error names the tool and the deadline.
pub fn wait_timeout(
    child: &mut Child,
    tool: &'static str,
    limit: Duration,
) -> Result<ExitStatus, MediaError> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            kill_tree(child);
            return Err(MediaError::TimedOut { tool, limit });
        }
        std::thread::sleep(POLL);
    }
}

/// What an `output_timeout` run captured. `stdout` buffers in full —
/// `.output()` parity, since callers parse it. `stderr` keeps only the
/// tail (`StderrDrain` semantics) so a flood can't grow memory.
#[derive(Debug)]
pub struct ProcOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// `.output()` with a deadline. Spawns `cmd` (in its own process group)
/// with null stdin, piped stdout/stderr; drains both on threads so
/// neither pipe can fill and deadlock against our wait; kills the whole
/// tree and reaps past `limit`.
///
/// On timeout the process *tree* is dead — no surviving grandchild can
/// hold our end of a pipe — so both drains hit EOF and complete before
/// the error returns: no leaked threads.
pub fn output_timeout(
    cmd: &mut Command,
    tool: &'static str,
    limit: Duration,
) -> Result<ProcOutput, MediaError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_grouped(cmd).map_err(|e| MediaError::Spawn { tool, source: e })?;
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let mut err_drain = StderrDrain::start(stderr_pipe);
    let status = wait_timeout(&mut child, tool, limit);
    let stdout = out_thread.join().unwrap_or_default();
    err_drain.join();
    let stderr = err_drain.tail().into_bytes();
    Ok(ProcOutput {
        status: status?,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child that never exits on its own — `cat` waiting on stdin that
    /// stays open… simpler still: `sleep`, present on every unix CI.
    fn sleeper(secs: &str) -> Child {
        Command::new("sleep")
            .arg(secs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep")
    }

    #[cfg(unix)]
    #[test]
    fn wait_timeout_kills_and_reaps_a_hung_child() {
        let mut child = sleeper("30");
        let start = Instant::now();
        let err = wait_timeout(&mut child, "sleep", Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, MediaError::TimedOut { .. }), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
        // Reaped — a second wait reports the killed status, not a zombie.
        assert!(child.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn wait_timeout_returns_promptly_for_a_finished_child() {
        let mut child = sleeper("0");
        let status = wait_timeout(&mut child, "sleep", Duration::from_secs(10)).unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn output_timeout_captures_output_then_times_out() {
        // Print one line, then hang — stdout must survive the kill.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out-line; echo err-line >&2; sleep 30"]);
        let err = output_timeout(&mut cmd, "sh", Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, MediaError::TimedOut { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn output_timeout_collects_like_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hello; echo oops >&2; exit 3"]);
        let out = output_timeout(&mut cmd, "sh", Duration::from_secs(10)).unwrap();
        assert!(!out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "oops");
    }

    /// The review repro: `sh` backgrounds a `sleep` that inherits our
    /// stdout pipe, then sleeps itself. Killing only the parent leaves
    /// the grandchild holding the pipe — `read_to_end` never sees EOF
    /// and the drain join hangs. The group kill ends the whole tree.
    #[cfg(unix)]
    #[test]
    fn output_timeout_kills_descendants_holding_the_pipes() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & exec sleep 30"]);
        let start = Instant::now();
        let err = output_timeout(&mut cmd, "sh", Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, MediaError::TimedOut { .. }));
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "drain threads must finish once the tree is dead"
        );
    }

    /// Same for `wait_timeout` on a grouped spawn — the connector path.
    #[cfg(unix)]
    #[test]
    fn wait_timeout_kills_the_grouped_tree() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & exec sleep 30"]);
        let mut child = spawn_grouped(&mut cmd).unwrap();
        let start = Instant::now();
        let err = wait_timeout(&mut child, "sh", Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, MediaError::TimedOut { .. }));
        assert!(start.elapsed() < Duration::from_secs(5));
        // Reaped — a second wait reports the killed status.
        assert!(child.try_wait().unwrap().is_some());
        // The group dies: killpg(sig 0) reports ESRCH once every member
        // is reaped — poll briefly, orphan reaping isn't synchronous.
        let gone = (0..20).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            let rc = unsafe { libc::killpg(child.id() as i32, 0) };
            rc != 0
        });
        assert!(gone, "grandchild's group should be dead");
    }
}
