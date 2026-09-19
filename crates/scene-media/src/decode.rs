//! Sequential video decode: `ffmpeg -i in -f rawvideo -pix_fmt rgba -`
//! piped into an iterator of RGBA frames. Sequential, not seeking — a
//! decode is a stream, and random access is the frame cache's job (M4).

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

use crate::error::{MediaError, Tool};
use crate::probe::{MediaInfo, probe};

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

/// Streams decoded frames from an ffmpeg subprocess. Dropping kills the
/// child (closing stdout makes ffmpeg exit on its own; we reap it anyway).
pub struct FrameStream {
    child: Child,
    stdout: ChildStdout,
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
        Self::spawn(
            path,
            info,
            &[],
            Some(format!("scale={width}:{height},fps={fps}")),
            (width, height),
        )
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
        let mut child = cmd.spawn().map_err(|e| MediaError::Spawn {
            tool: "ffmpeg",
            source: e,
        })?;
        let stdout = child.stdout.take().expect("stdout was piped");
        Ok(FrameStream {
            child,
            stdout,
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

    /// Drain stderr after the child exits (diagnostics on failure).
    fn stderr_tail(&mut self) -> String {
        let mut buf = String::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_string(&mut buf);
        }
        buf.trim().to_string()
    }

    /// Fill `buf` completely; returns bytes actually read. A clean EOF at
    /// 0 bytes means end of stream; a short count is a truncated frame.
    fn fill(&mut self, buf: &mut [u8]) -> Result<usize, MediaError> {
        let mut read = 0;
        while read < buf.len() {
            match self.stdout.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
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
                // Clean EOF at a frame boundary. Block until the child
                // exits to inspect the real status — try_wait races with
                // process reaping and a crashed decoder that emitted
                // nothing is an error, not EOF.
                match self.child.wait() {
                    Ok(status) if status.success() => None,
                    Ok(status) => Some(Err(MediaError::Failed {
                        tool: "ffmpeg",
                        status: status.to_string(),
                        stderr: self.stderr_tail(),
                    })),
                    Err(e) => Some(Err(e.into())),
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
        // Closing stdout already signals EOF; kill only if still alive,
        // then reap so no zombie remains.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}
