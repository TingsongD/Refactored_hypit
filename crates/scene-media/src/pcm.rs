//! Sequential audio decode: `ffmpeg -i in -vn -ac 1 -ar RATE -f f32le -`
//! piped into an iterator of fixed-size sample chunks. Mirrors
//! `FrameStream` — sequential, not seeking; one chunk per video-frame
//! hop keeps audio metrics aligned to the analysis grid.

use std::io::Read;
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};
use std::time::Duration;

use crate::error::{MediaError, Tool};
use crate::probe::MediaInfo;
use crate::proc::{StreamingChild, wait_stream};
use crate::stderr::StderrDrain;
use crate::watchdog::{Heartbeat, StallWatchdog, beat, heartbeat, watch_io};

/// Same reasoning as decode.rs — five silent minutes is a wedged
/// decoder, not slow media.
const STALL_LIMIT: Duration = Duration::from_secs(300);
const REAP_TIMEOUT: Duration = Duration::from_secs(30);

/// One chunk of mono f32 samples, `hop_samples` long.
#[derive(Debug, Clone, PartialEq)]
pub struct PcmChunk {
    /// Zero-based chunk index — aligned to the analysis frame index
    /// when `hop_samples == rate / fps`.
    pub index: u64,
    pub samples: Vec<f32>,
}

/// Streams decoded PCM from an ffmpeg subprocess. Dropping kills the
/// child (closing stdout makes ffmpeg exit on its own; we reap anyway).
pub struct PcmStream {
    child: StreamingChild,
    stdout: ChildStdout,
    /// stderr drains on a thread so a chatty decoder can't fill the
    /// pipe and deadlock against our stdout reads.
    stderr: StderrDrain,
    /// Updated on every successful pipe read; the watchdog kills the
    /// child if it goes stale — a blocked `read` can't rescue itself.
    heartbeat: Heartbeat,
    watchdog: StallWatchdog,
    /// `hop_samples * 4` — f32le.
    chunk_len: usize,
    hop_samples: usize,
    index: u64,
}

impl PcmStream {
    /// Open the source's first audio stream as mono f32 at `rate`,
    /// chunked into `hop_samples`-sample reads. `Ok(None)` when the
    /// media has no audio — silence is a fact, not a failure.
    pub fn open(
        path: &Path,
        info: &MediaInfo,
        rate: u32,
        hop_samples: usize,
    ) -> Result<Option<Self>, MediaError> {
        if info.audio.is_none() {
            return Ok(None);
        }
        if rate == 0 || hop_samples == 0 {
            return Err(MediaError::ProbeParse(
                "pcm: rate and hop_samples must be positive".into(),
            ));
        }
        let tool = std::env::var("FFMPEG").unwrap_or_else(|_| Tool::Ffmpeg.name().to_string());
        let rate_s = rate.to_string();
        let mut cmd = Command::new(&tool);
        cmd.args(["-v", "error"])
            .arg("-i")
            .arg(path)
            .args(["-vn", "-ac", "1", "-ar", &rate_s, "-f", "f32le", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = StreamingChild::spawn(&mut cmd).map_err(|e| MediaError::Spawn {
            tool: "ffmpeg",
            source: e,
        })?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = StderrDrain::start(child.stderr.take().expect("stderr was piped"));
        let heartbeat = heartbeat();
        let watchdog = child.watchdog(heartbeat.clone(), STALL_LIMIT);
        Ok(Some(PcmStream {
            child,
            stdout,
            stderr,
            heartbeat,
            watchdog,
            chunk_len: hop_samples * 4,
            hop_samples,
            index: 0,
        }))
    }

    /// The captured stderr tail — join the drain after the child exits
    /// so the final bytes land (diagnostics on failure).
    fn stderr_tail(&mut self) -> String {
        self.stderr.join();
        self.stderr.tail()
    }

    /// Fill `buf` completely; returns bytes actually read. A clean EOF
    /// at 0 bytes means end of stream; a short count is a truncated
    /// chunk — audio can end mid-hop and the tail is still signal, so
    /// short reads are zero-padded rather than dropped.
    fn fill(&mut self, buf: &mut [u8]) -> Result<usize, MediaError> {
        let mut read = 0;
        while read < buf.len() {
            let result = {
                let _io = watch_io(&self.heartbeat);
                self.stdout.read(&mut buf[read..])
            };
            match result {
                Ok(0) => break,
                Ok(n) => {
                    read += n;
                    beat(&self.heartbeat);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(read)
    }
}

impl Iterator for PcmStream {
    type Item = Result<PcmChunk, MediaError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut bytes = vec![0u8; self.chunk_len];
        match self.fill(&mut bytes) {
            Ok(n) if n == self.chunk_len => {
                let samples: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                let chunk = PcmChunk {
                    index: self.index,
                    samples,
                };
                self.index += 1;
                Some(Ok(chunk))
            }
            Ok(0) => {
                // Clean EOF at a chunk boundary. Disarm first — once the
                // child is reaped its pid can be recycled, and a still-
                // armed watchdog ticking could kill a stranger.
                self.watchdog.disarm();
                let status = wait_stream(&mut self.child, &mut self.stderr, "ffmpeg", REAP_TIMEOUT);
                match status {
                    Ok(status) if status.success() => None,
                    Ok(status) => Some(Err(MediaError::Failed {
                        tool: "ffmpeg",
                        status: status.to_string(),
                        stderr: self.stderr_tail(),
                    })),
                    Err(e) => Some(Err(e)),
                }
            }
            Ok(got) => {
                // Partial tail hop — zero-pad to the full chunk so the
                // hop grid stays aligned with the video frames.
                for b in &mut bytes[got..] {
                    *b = 0;
                }
                let samples: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                debug_assert_eq!(samples.len(), self.hop_samples);
                let chunk = PcmChunk {
                    index: self.index,
                    samples,
                };
                self.index += 1;
                Some(Ok(chunk))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

impl Drop for PcmStream {
    fn drop(&mut self) {
        // Owner is back in control — the watchdog must not race our own
        // kill-and-reap.
        self.watchdog.disarm();
        // Closing stdout already signals EOF; kill only if still alive,
        // then reap so no zombie remains.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    #[test]
    fn pcm_blocked_read_is_guarded_but_idle_stream_is_not() {
        let mut child = StreamingChild::spawn(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proc::tests::pipe_holder_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = StderrDrain::start(child.stderr.take().unwrap());
        let heartbeat = heartbeat();
        let watchdog = child.watchdog(heartbeat.clone(), Duration::from_millis(80));
        let mut stream = PcmStream {
            child,
            stdout,
            stderr,
            heartbeat,
            watchdog,
            chunk_len: 4096,
            hop_samples: 1024,
            index: 0,
        };
        std::thread::sleep(Duration::from_millis(160));
        assert!(
            stream.child.try_wait().unwrap().is_none(),
            "idle PCM child was killed"
        );
        let start = std::time::Instant::now();
        let _ = stream.next();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "PCM read did not meet deadline"
        );
        assert!(stream.child.try_wait().unwrap().is_some());
    }
}
