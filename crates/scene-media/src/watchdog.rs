//! A stall watchdog for children whose pipes we block on. A thread that
//! is *inside* a `read`/`write` on a pipe can't rescue itself — the kill
//! has to come from outside. The I/O side stores a monotonic-millis
//! heartbeat after every successful read/write; when the stamp goes
//! stale past `stall`, the watchdog SIGKILLs the child's pid so the
//! blocked call returns EPIPE and the error propagates normally.
//!
//! Killing by pid needs a syscall — unix only (`libc`, a tiny
//! unix-target dep). Other platforms compile the watchdog to a no-op;
//! owner-thread deadlines (`wait_timeout`) still apply everywhere.
//!
//! Safety argument for kill-by-pid: on unix a spawned child keeps its
//! pid until reaped — even as a zombie — so while a `Child` handle is
//! alive the pid cannot be recycled into an unrelated process. The
//! watchdog is disarmed as soon as a wait completes, closing the window.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Millis since this process started — monotonic, so wall-clock jumps
/// can't age a heartbeat backwards or forwards.
fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Shared progress stamp — the I/O path stores `monotonic_ms()` after
/// each successful read or write.
pub type Heartbeat = Arc<AtomicU64>;

/// A heartbeat primed to "now" — arming at spawn gives the child the
/// full stall window before any I/O is expected.
pub fn heartbeat() -> Heartbeat {
    Arc::new(AtomicU64::new(monotonic_ms()))
}

/// Store progress. Called from the I/O path after each successful
/// `read`/`write` — one atomic store per pipe chunk, effectively free.
pub fn beat(heart: &Heartbeat) {
    heart.store(monotonic_ms(), Ordering::Relaxed);
}

/// Watches `beat`; if it stays unchanged longer than `stall`, kills
/// `pid` once and exits. `Drop`/`disarm` stops the thread — call
/// `disarm` right after reaping the child so a recycled pid can never
/// become a target.
pub struct StallWatchdog {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StallWatchdog {
    pub fn arm(pid: u32, beat: Heartbeat, stall: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let age = monotonic_ms().saturating_sub(beat.load(Ordering::Relaxed));
                // Re-check stop after the age check: a `disarm` racing
                // our sleep means the owner already reaped the child and
                // its pid may be recycled — killing now could hit an
                // unrelated process.
                if age > stall.as_millis() as u64 {
                    if stop2.load(Ordering::Relaxed) {
                        return;
                    }
                    kill_pid(pid);
                    return;
                }
            }
        });
        StallWatchdog {
            stop,
            thread: Some(thread),
        }
    }

    /// Stop watching. The owner kills or reaps the child itself from
    /// here — the watchdog's job ends when the owner is back in control.
    pub fn disarm(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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
    // SIGKILL the pid we spawned. If the child already exited but wasn't
    // reaped it's a zombie — kill reports ESRCH, harmless.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

#[cfg(not(unix))]
fn kill_pid(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn fresh_heartbeat_is_now() {
        let hb = heartbeat();
        let age = monotonic_ms() - hb.load(Ordering::Relaxed);
        assert!(age < 1000, "fresh heartbeat should be ~now, got {age}ms");
    }

    /// The watchdog kills a stalled child: `sleep 30` with a heartbeat
    /// that never updates → SIGKILL inside ~2s. Unix-only (kill-by-pid).
    #[cfg(unix)]
    #[test]
    fn watchdog_kills_a_stalled_child() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let hb = heartbeat();
        // Stale the heartbeat into the past so the watchdog fires fast.
        hb.store(0, Ordering::Relaxed);
        let _wd = StallWatchdog::arm(child.id(), hb, Duration::from_millis(500));
        // Poll until the kill lands — the watchdog ticks once a second.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                assert!(!status.success(), "killed child is not a clean exit");
                return;
            }
            assert!(Instant::now() < deadline, "watchdog never fired");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A heartbeat that keeps beating must not be killed.
    #[cfg(unix)]
    #[test]
    fn beating_heartbeat_survives() {
        let mut child = Command::new("sleep")
            .arg("0.2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let hb = heartbeat();
        let mut wd = StallWatchdog::arm(child.id(), hb.clone(), Duration::from_secs(60));
        // Clean exit before any stall — watchdog disarms, no kill.
        let status = child.wait().unwrap();
        wd.disarm();
        assert!(status.success());
    }
}
