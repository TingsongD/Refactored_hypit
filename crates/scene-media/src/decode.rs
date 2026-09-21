//! Sequential video decode: `ffmpeg -i in -f rawvideo -pix_fmt rgba -`
//! piped into an iterator of RGBA frames. Sequential, not seeking — a
//! decode is a stream, and random access is the frame cache's job (M4).

use std::io::Read;
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};
use std::time::Duration;

use crate::error::{MediaError, Tool};
use crate::probe::{MediaInfo, probe};
use crate::proc::{StreamingChild, wait_stream};
use crate::stderr::StderrDrain;
use crate::watchdog::{Heartbeat, StallWatchdog, beat, heartbeat, watch_io};

/// Decode reads are local-file I/O — five minutes without a byte is a
/// wedged decoder, not slow media. The watchdog kills the child so the
/// blocked `read` returns instead of holding a frame forever.
const STALL_LIMIT: Duration = Duration::from_secs(300);
/// Reaping a finished child is instant; thirty seconds covers a wedged
/// post-exit teardown.
const REAP_TIMEOUT: Duration = Duration::from_secs(30);

/// One decoded frame, RGBA8, row-major, tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Zero-based decode order.
    pub index: u64,
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

impl Frame {
    pub fn byte_len(width: u32, height: u32) -> usize {
        width as usize * height as usize * 4
    }
}

/// Source-time interval `[start_s, end_s)` for scaled decoding.
#[derive(Debug, Clone, Copy)]
pub struct DecodeWindow {
    pub start_s: f64,
    pub end_s: f64,
}

/// Streams decoded frames from an ffmpeg subprocess. Dropping kills the
/// child (closing stdout makes ffmpeg exit on its own; we reap it anyway).
pub struct FrameStream {
    child: StreamingChild,
    stdout: ChildStdout,
    /// stderr drains on a thread so a chatty decoder can't fill the pipe
    /// and deadlock against our stdout reads.
    stderr: StderrDrain,
    /// Updated on every successful pipe read; the watchdog kills the
    /// child if it goes stale — a blocked `read` can't rescue itself.
    heartbeat: Heartbeat,
    watchdog: StallWatchdog,
    info: MediaInfo,
    width: u32,
    height: u32,
    frame_len: usize,
    index: u64,
}

impl FrameStream {
    /// Probe `path`, then spawn the decoder at native resolution.
    pub fn open(path: &Path) -> Result<Self, MediaError> {
        let info = probe(path)?;
        Self::open_with(path, &info, &[])
    }

    /// Decode downscaled + fps-thinned frames for analysis. Output dims
    /// are the requested `width`×`height`, not the source's.
    pub fn open_scaled(
        path: &Path,
        info: &MediaInfo,
        width: u32,
        height: u32,
        fps: f64,
    ) -> Result<Self, MediaError> {
        Self::open_scaled_window(path, info, width, height, fps, None)
    }

    /// Decode a half-open source-time window, with window-local frame indices.
    /// Keep one second of preroll so the fps filter retains the source grid.
    pub fn open_scaled_window(
        path: &Path,
        info: &MediaInfo,
        width: u32,
        height: u32,
        fps: f64,
        window: Option<DecodeWindow>,
    ) -> Result<Self, MediaError> {
        let invalid = |message| {
            MediaError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                message,
            ))
        };
        if width == 0 || height == 0 || !fps.is_finite() || fps <= 0.0 {
            return Err(invalid(
                "scaled dimensions and fps must be positive and finite",
            ));
        }
        let mut filter = format!("scale={width}:{height},fps={fps}");
        let mut args = Vec::new();
        if let Some(window) = window {
            if !window.start_s.is_finite()
                || !window.end_s.is_finite()
                || !(window.end_s * fps).is_finite()
                || window.start_s < 0.0
                || window.end_s <= window.start_s
            {
                return Err(invalid(
                    "decode window must be finite, nonnegative, and nonempty",
                ));
            }
            args = vec![
                "-copyts".to_string(),
                "-start_at_zero".to_string(),
                "-ss".to_string(),
                (window.start_s.floor() - 1.0).max(0.0).to_string(),
                "-to".to_string(),
                (window.end_s + 1.0 / fps).to_string(),
            ];
            filter.push_str(&format!(
                ",trim=start_pts={}:end_pts={},setpts=PTS-STARTPTS",
                (window.start_s * fps - 1e-9).ceil().max(0.0),
                (window.end_s * fps - 1e-9).ceil()
            ));
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Self::spawn(path, info, &refs, Some(filter), (width, height))
    }

    /// Open with a caller-supplied probe result plus extra ffmpeg input
    /// args (e.g. `["-ss", "1.5"]` placed before `-i`).
    pub fn open_with(
        path: &Path,
        info: &MediaInfo,
        pre_input_args: &[&str],
    ) -> Result<Self, MediaError> {
        let video = info.video.as_ref().ok_or(MediaError::NoVideoStream)?;
        Self::spawn(
            path,
            info,
            pre_input_args,
            None,
            (video.width, video.height),
        )
    }

    /// The one spawn path: `ffmpeg [pre] -i path [-vf f] -f rawvideo
    /// -pix_fmt rgba -`, piped. `dims` are the output dims — the
    /// source's own unless a `-vf scale` shrinks them.
    fn spawn(
        path: &Path,
        info: &MediaInfo,
        pre_input_args: &[&str],
        vf: Option<String>,
        (width, height): (u32, u32),
    ) -> Result<Self, MediaError> {
        let bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4));
        if width == 0 || height == 0 || bytes.is_none_or(|n| n > 1024 * 1024 * 1024) {
            return Err(MediaError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "decoded dimensions must fit within a 1 GiB frame",
            )));
        }
        if info.video.is_none() {
            return Err(MediaError::NoVideoStream);
        }
        let tool = std::env::var("FFMPEG").unwrap_or_else(|_| Tool::Ffmpeg.name().to_string());
        let mut cmd = Command::new(&tool);
        cmd.args(["-v", "error"])
            .args(pre_input_args)
            .arg("-i")
            .arg(path);
        if let Some(vf) = &vf {
            cmd.args(["-vf", vf]);
        }
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
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
        Ok(FrameStream {
            child,
            stdout,
            stderr,
            heartbeat,
            watchdog,
            info: info.clone(),
            width,
            height,
            frame_len: Frame::byte_len(width, height),
            index: 0,
        })
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The captured stderr tail — join the drain after the child exits
    /// so the final bytes land (diagnostics on failure).
    fn stderr_tail(&mut self) -> String {
        self.stderr.join();
        self.stderr.tail()
    }

    /// Fill `buf` completely; returns bytes actually read. A clean EOF at
    /// 0 bytes means end of stream; a short count is a truncated frame.
    fn fill(&mut self, buf: &mut [u8]) -> Result<usize, MediaError> {
        let _io = watch_io(&self.heartbeat);
        let mut read = 0;
        while read < buf.len() {
            match self.stdout.read(&mut buf[read..]) {
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

impl Iterator for FrameStream {
    type Item = Result<Frame, MediaError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut pixels = vec![0u8; self.frame_len];
        match self.fill(&mut pixels) {
            Ok(n) if n == self.frame_len => {
                let frame = Frame {
                    index: self.index,
                    width: self.width,
                    height: self.height,
                    pixels,
                };
                self.index += 1;
                Some(Ok(frame))
            }
            Ok(0) => {
                // Clean EOF at a frame boundary. Disarm first — once the
                // child is reaped its pid can be recycled, and a still-
                // armed watchdog ticking could kill a stranger. The reap
                // deadline is wait_timeout's own kill path.
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
            Ok(got) => Some(Err(MediaError::ShortFrame {
                expected: self.frame_len,
                got,
            })),
            Err(e) => Some(Err(e)),
        }
    }
}

impl Drop for FrameStream {
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
