//! Drains a child's stderr on a thread into a bounded keep-tail buffer.
//! Without it, a chatty child fills the pipe (64 KiB) and blocks on its
//! stderr write while we're blocked reading stdout / writing stdin — a
//! classic pipe deadlock. The drain keeps the pipe empty forever; the
//! keep-tail bound keeps memory flat when the child is verbose.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Bytes kept — diagnostics need the tail, not the flood.
const KEEP: usize = 256 * 1024;

pub struct StderrDrain {
    buf: Arc<Mutex<Vec<u8>>>,
    thread: Option<JoinHandle<()>>,
}

impl StderrDrain {
    /// Spawn the drain for `pipe` — anything `Read + Send` (`ChildStderr`
    /// from `Stdio::piped()`, a `PipeReader` in tests).
    pub fn start<R: Read + Send + 'static>(mut pipe: R) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let thread_buf = Arc::clone(&buf);
        let thread = std::thread::spawn(move || {
            let mut tmp = [0u8; 8192];
            loop {
                match pipe.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = thread_buf.lock().unwrap();
                        b.extend_from_slice(&tmp[..n]);
                        if b.len() > KEEP {
                            let drop = b.len() - KEEP;
                            b.drain(..drop);
                        }
                    }
                }
            }
        });
        StderrDrain {
            buf,
            thread: Some(thread),
        }
    }

    /// The captured tail, lossy. Safe while the child still runs.
    pub fn tail(&self) -> String {
        let b = self.buf.lock().unwrap();
        String::from_utf8_lossy(&b).trim().to_string()
    }

    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }

    /// Join the reader thread — call after the child exits (stderr EOF
    /// ends it) so `tail()` sees the final bytes.
    pub fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for StderrDrain {
    fn drop(&mut self) {
        self.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn drain_empties_the_pipe_and_keeps_the_tail() {
        // 700 KB through the pipe — far past the 64 KiB buffer. Without
        // the drain, `write_all` would block forever; with it, this
        // completes and the tail holds the last KEEP bytes.
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut drain = StderrDrain::start(reader);
        writer.write_all(&vec![b'x'; 700_000]).unwrap();
        drop(writer); // EOF ends the thread
        drain.join();
        let tail = drain.tail();
        assert_eq!(tail.len(), KEEP);
        assert!(tail.bytes().all(|b| b == b'x'));
    }

    #[test]
    fn tail_is_live_before_join() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut drain = StderrDrain::start(reader);
        writer.write_all(b"early").unwrap();
        // The drain may not have consumed it yet — poll briefly rather
        // than sleep a fixed amount.
        let mut seen = false;
        for _ in 0..10_000 {
            if drain.tail().contains("early") {
                seen = true;
                break;
            }
            std::thread::yield_now();
        }
        // Drop the writer before asserting: EOF ends the drain thread so
        // `join` (and Drop::join on unwind) can never block.
        drop(writer);
        drain.join();
        assert!(seen, "drain did not capture stderr");
    }
}
