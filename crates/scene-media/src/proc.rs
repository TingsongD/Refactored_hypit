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

use command_group::CommandGroup;
use std::io::{Read, Seek, Write};
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

/// Capture a command with one deadline covering parent exit and pipe EOF.
/// The process-group / Windows Job handle stays alive after the leader exits.
pub fn output_timeout(
    cmd: &mut Command,
    tool: &'static str,
    limit: Duration,
) -> Result<ProcOutput, MediaError> {
    capture_timeout(cmd, tool, limit, None, true)
}

/// JSON connectors read the same stdin byte stream from an anonymous temporary
/// file. This removes the blocking writer thread entirely: a child that never
/// reads stdin cannot prevent timeout cleanup.
/// Stdout is discarded, preserving the connector contract; stderr is bounded.
pub fn run_with_input_timeout(
    cmd: &mut Command,
    tool: &'static str,
    limit: Duration,
    input: &[u8],
) -> Result<ProcOutput, MediaError> {
    capture_timeout(cmd, tool, limit, Some(input), false)
}

fn capture_timeout(
    cmd: &mut Command,
    tool: &'static str,
    limit: Duration,
    input: Option<&[u8]>,
    capture_stdout: bool,
) -> Result<ProcOutput, MediaError> {
    let deadline = Instant::now() + limit;
    if let Some(input) = input {
        let mut file = tempfile::tempfile()?;
        file.write_all(input)?;
        file.rewind()?;
        cmd.stdin(Stdio::from(file));
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(if capture_stdout {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stderr(Stdio::piped());
    // command-group assigns the Windows Job before resuming the child, so an
    // immediately exiting shell cannot escape job membership with its children.
    let mut group = cmd.group();
    #[cfg(windows)]
    group.kill_on_drop(true);
    let mut child = group
        .spawn()
        .map_err(|e| MediaError::Spawn { tool, source: e })?;
    let stdout_pipe = child.inner().stdout.take();
    let stderr_pipe = child.inner().stderr.take().expect("stderr piped");
    let out_thread = stdout_pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            pipe.read_to_end(&mut buf).map(|_| buf)
        })
    });
    let mut err_drain = StderrDrain::start(stderr_pipe);
    let mut status = None;
    let result = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(s) => status = s,
                Err(e) => break Err(MediaError::Io(e)),
            }
        }
        if let Some(status) = status
            && out_thread
                .as_ref()
                .is_none_or(|thread| thread.is_finished())
            && err_drain.is_finished()
        {
            break Ok(status);
        }
        if Instant::now() >= deadline {
            break Err(MediaError::TimedOut { tool, limit });
        }
        std::thread::sleep(POLL);
    };
    if result.is_err() {
        // Still owns the job/group even if the leader has already exited.
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = match out_thread {
        Some(thread) => thread
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("stdout reader panicked"))),
        None => Ok(Vec::new()),
    };
    err_drain.join();
    Ok(ProcOutput {
        status: result?,
        stdout: stdout?,
        stderr: err_drain.tail().into_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", name, "--ignored", "--nocapture"]);
        cmd
    }

    // These helpers run only as subprocesses, including on Windows where
    // shell scripts cannot stand in for executable connectors.
    #[test]
    #[ignore = "subprocess fixture"]
    fn pipe_holder_fixture() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn parent_exit_fixture() {
        let _child = fixture("proc::tests::pipe_holder_fixture")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        // Bypass the test harness so this parent really exits first.
        std::process::exit(0);
    }

    #[test]
    fn deadline_covers_pipes_after_parent_exit() {
        let mut cmd = fixture("proc::tests::parent_exit_fixture");
        let start = Instant::now();
        let err = output_timeout(&mut cmd, "fixture", Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, MediaError::TimedOut { .. }), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn unread_large_stdin_cannot_block_timeout() {
        let mut cmd = fixture("proc::tests::pipe_holder_fixture");
        let start = Instant::now();
        let err = run_with_input_timeout(
            &mut cmd,
            "fixture",
            Duration::from_secs(1),
            &vec![b'x'; 1024 * 1024],
        )
        .unwrap_err();
        assert!(matches!(err, MediaError::TimedOut { .. }), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// A child that never exits on its own — `cat` waiting on stdin that
    /// stays open… simpler still: `sleep`, present on every unix CI.
    #[cfg(unix)]
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
        cmd.args(["-c", "sleep 30 & exec sleep 30"])
            .stdout(Stdio::piped());
        let mut child = spawn_grouped(&mut cmd).unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let drain = std::thread::spawn(move || {
            let _ = tx.send(stdout.read_to_end(&mut Vec::new()));
        });
        let start = Instant::now();
        let err = wait_timeout(&mut child, "sh", Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, MediaError::TimedOut { .. }));
        assert!(start.elapsed() < Duration::from_secs(5));
        // Reaped — a second wait reports the killed status.
        assert!(child.try_wait().unwrap().is_some());
        // EOF proves descendants released the pipe, without depending on the
        // host init promptly reaping orphaned zombie processes.
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        drain.join().unwrap();
    }
}
