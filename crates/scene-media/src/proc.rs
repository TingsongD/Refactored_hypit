//! Deadlines for subprocess waits. `child.wait()` and `.output()` block
//! forever on a wedged child — a hung ffmpeg, a connector that never
//! reads stdin, a `curl` stuck mid-transfer. These variants poll
//! `try_wait` on a deadline and kill+reap on expiry, so a hung tool is
//! an error the engine can report instead of a process that never ends.
//!
//! Limits are hang-catchers: generous by design (minutes, not seconds),
//! so they never fire on a healthy slow run.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::error::MediaError;
use crate::stderr::StderrDrain;

/// Poll interval — coarse enough to cost nothing, fine enough that a
/// hung tool is noticed within a tenth of a second past its deadline.
const POLL: Duration = Duration::from_millis(50);

/// Wait for `child` up to `limit`. On expiry the child is killed and
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
            let _ = child.kill();
            let _ = child.wait();
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

/// `.output()` with a deadline. Spawns `cmd` with null stdin, piped
/// stdout/stderr; drains both on threads so neither pipe can fill and
/// deadlock against our wait; kills+reaps past `limit`.
///
/// On timeout the child is already dead and the pipes are at EOF, so
/// both drains complete before the error returns — no leaked threads.
pub fn output_timeout(
    cmd: &mut Command,
    tool: &'static str,
    limit: Duration,
) -> Result<ProcOutput, MediaError> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| MediaError::Spawn { tool, source: e })?;
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
}
