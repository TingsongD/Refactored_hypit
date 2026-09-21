//! Time out outstanding pipe operations, never intentionally idle workers.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Default)]
struct WatchState {
    active_since: Option<Instant>,
    stopped: bool,
}

#[derive(Clone, Default)]
pub struct Heartbeat(Arc<(Mutex<WatchState>, Condvar)>);

/// Starts inactive. A decoder awaiting demand and an encoder awaiting rendered
/// frames have no outstanding I/O and must never age into a timeout.
pub fn heartbeat() -> Heartbeat {
    Heartbeat::default()
}

pub fn beat(heart: &Heartbeat) {
    let (lock, changed) = &*heart.0;
    let mut state = lock.lock().unwrap();
    if state.active_since.is_some() {
        state.active_since = Some(Instant::now());
        changed.notify_all();
    }
}

/// Scopes the watchdog to a read/write, including early returns and unwinding.
pub struct IoGuard(Heartbeat);

pub fn watch_io(heart: &Heartbeat) -> IoGuard {
    let (lock, changed) = &*heart.0;
    lock.lock().unwrap().active_since = Some(Instant::now());
    changed.notify_all();
    IoGuard(heart.clone())
}

impl Drop for IoGuard {
    fn drop(&mut self) {
        let (lock, changed) = &*self.0.0;
        lock.lock().unwrap().active_since = None;
        changed.notify_all();
    }
}

pub struct StallWatchdog {
    heart: Heartbeat,
    thread: Option<JoinHandle<()>>,
}

impl StallWatchdog {
    pub fn arm(pid: u32, heart: Heartbeat, stall: Duration) -> Self {
        Self::arm_action(heart, stall, move || kill_pid(pid))
    }

    pub(crate) fn arm_action(
        heart: Heartbeat,
        stall: Duration,
        kill: impl FnOnce() + Send + 'static,
    ) -> Self {
        let watched = heart.clone();
        let thread = std::thread::spawn(move || {
            let (lock, changed) = &*watched.0;
            let mut state = lock.lock().unwrap();
            while !state.stopped {
                if let Some(started) = state.active_since {
                    let elapsed = started.elapsed();
                    if elapsed >= stall {
                        // Serialize expiry with IoGuard::drop, so a completed
                        // operation cannot race a stale snapshot into a kill.
                        kill();
                        return;
                    }
                    state = changed.wait_timeout(state, stall - elapsed).unwrap().0;
                } else {
                    state = changed.wait(state).unwrap();
                }
            }
        });
        Self {
            heart,
            thread: Some(thread),
        }
    }

    /// Call before reaping the child, closing the PID reuse window.
    pub fn disarm(&mut self) {
        let (lock, changed) = &*self.heart.0;
        lock.lock().unwrap().stopped = true;
        changed.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for StallWatchdog {
    fn drop(&mut self) {
        self.disarm();
    }
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    // Grouped children lead their own process group — kill the group so
    // descendants can't outlive the stall kill holding our pipes. ESRCH
    // on an ungrouped pid is harmless (a pgid is its leader's pid; a
    // non-leader names no group); the direct kill covers that case and
    // the zombie case (kill on an unreaped child is a no-op).
    unsafe {
        libc::killpg(pid as i32, libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    // taskkill /T /F — the whole tree, forced. Best-effort: if the tool
    // or the pid is already gone the owner's blocked pipe I/O still
    // fails as soon as the process holding it dies.
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn kill_pid(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::{Command, Stdio};

    fn sleeper() -> std::process::Child {
        crate::proc::spawn_grouped(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proc::tests::pipe_holder_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .unwrap()
    }

    #[test]
    fn idle_and_completed_operations_survive_past_stall_limit() {
        let mut child = sleeper();
        let hb = heartbeat();
        let mut wd = StallWatchdog::arm(child.id(), hb.clone(), Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(150));
        assert!(child.try_wait().unwrap().is_none(), "idle worker killed");
        {
            let _io = watch_io(&hb);
            beat(&hb);
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            child.try_wait().unwrap().is_none(),
            "completed I/O still armed"
        );
        wd.disarm();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn outstanding_read_still_terminates_hung_child() {
        let mut child = sleeper();
        let mut pipe = child.stdout.take().unwrap();
        let hb = heartbeat();
        let mut wd = StallWatchdog::arm(child.id(), hb.clone(), Duration::from_millis(100));
        let start = Instant::now();
        {
            let _io = watch_io(&hb);
            pipe.read_to_end(&mut Vec::new()).unwrap();
        }
        wd.disarm();
        assert!(!child.wait().unwrap().success());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn disarm_wakes_inactive_watchdog_promptly() {
        let mut child = sleeper();
        let mut wd = StallWatchdog::arm(child.id(), heartbeat(), Duration::from_secs(600));
        let start = Instant::now();
        wd.disarm();
        assert!(start.elapsed() < Duration::from_secs(1));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
