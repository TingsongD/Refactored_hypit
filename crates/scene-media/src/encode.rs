//! NV12 → ffmpeg → mp4. The encoder is a subprocess whose stdin is a
//! rawvideo stream — the same pipe shape decode uses, in reverse.
//!
//! The stream is tagged BT.709 end to end: the converter produces
//! BT.709-limited NV12, and these flags tell the encoder and the
//! container that is what it is holding.

use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

use scene_ir::Rational;

use crate::error::{MediaError, Tool};
use crate::stderr::StderrDrain;

/// One-way encode of an NV12 program stream to H.264/mp4.
///
/// `write_frame` takes one `w*h*3/2` NV12 buffer. `finish` closes the
/// pipe and waits for the muxer; dropping without finishing kills it.
pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    /// stderr drains on a thread so a chatty encoder can't fill the pipe
    /// and deadlock against our stdin writes.
    stderr: StderrDrain,
    finished: bool,
}

impl Encoder {
    /// Spawn `ffmpeg` reading rawvideo NV12 from stdin. `fps` is a
    /// rational so `30000/1001` reaches ffmpeg exactly.
    pub fn open(out: &Path, w: u32, h: u32, fps: &Rational) -> Result<Self, MediaError> {
        Self::open_impl(out, w, h, fps, None)
    }

    /// Same, but mux a rendered program audio file alongside the video.
    /// `-shortest` keeps the container honest if the wav outlasts the
    /// last frame by a codec block.
    pub fn open_muxed(
        out: &Path,
        w: u32,
        h: u32,
        fps: &Rational,
        audio: &Path,
    ) -> Result<Self, MediaError> {
        Self::open_impl(out, w, h, fps, Some(audio))
    }

    /// `Some` → muxed, `None` → silent. Lets callers hand through a
    /// maybe-mixed program wav without branching on the variant.
    pub fn open_muxed_opt(
        out: &Path,
        w: u32,
        h: u32,
        fps: &Rational,
        audio: Option<&Path>,
    ) -> Result<Self, MediaError> {
        Self::open_impl(out, w, h, fps, audio)
    }

    fn open_impl(
        out: &Path,
        w: u32,
        h: u32,
        fps: &Rational,
        audio: Option<&Path>,
    ) -> Result<Self, MediaError> {
        if !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            return Err(MediaError::ProbeParse(format!(
                "NV12 needs even dimensions, got {w}x{h}"
            )));
        }
        let tool = std::env::var("FFMPEG").unwrap_or_else(|_| Tool::Ffmpeg.name().to_string());
        let mut child = Command::new(&tool)
            .args(encode_args(out, w, h, fps, audio))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| MediaError::Spawn {
                tool: "ffmpeg",
                source: e,
            })?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stderr = StderrDrain::start(child.stderr.take().expect("stderr was piped"));
        Ok(Encoder {
            child,
            stdin: Some(stdin),
            stderr,
            finished: false,
        })
    }

    /// Push one NV12 frame into the encoder.
    pub fn write_frame(&mut self, nv12: &[u8]) -> Result<(), MediaError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(MediaError::ProbeParse(
                "encoder stdin already closed".to_string(),
            ));
        };
        stdin.write_all(nv12).map_err(|e| {
            // EPIPE means ffmpeg died — report its status, not just the pipe.
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                self.child_failed()
            } else {
                MediaError::Io(e)
            }
        })
    }

    /// Close stdin, wait for muxer teardown, report failures.
    pub fn finish(&mut self) -> Result<(), MediaError> {
        drop(self.stdin.take());
        let status = self.child.wait()?;
        self.finished = true;
        if !status.success() {
            return Err(self.failed_status(status));
        }
        Ok(())
    }

    fn child_failed(&mut self) -> MediaError {
        let status = self
            .child
            .try_wait()
            .ok()
            .flatten()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        MediaError::Failed {
            tool: "ffmpeg",
            status,
            stderr: self.stderr_tail(),
        }
    }

    fn failed_status(&mut self, status: std::process::ExitStatus) -> MediaError {
        MediaError::Failed {
            tool: "ffmpeg",
            status: status.to_string(),
            stderr: self.stderr_tail(),
        }
    }

    fn stderr_tail(&mut self) -> String {
        self.stderr.join();
        self.stderr.tail()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        drop(self.stdin.take());
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// The full ffmpeg argv (minus the binary name). Pure — checked by unit
/// test so a flag regression fails hermetically, before ffmpeg runs.
/// With `audio`, input 1 is the program wav and we map+AAC-encode it;
/// without, `-an` makes the silence explicit.
fn encode_args(out: &Path, w: u32, h: u32, fps: &Rational, audio: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-y",
        "-v",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "nv12",
        "-s",
        &format!("{w}x{h}"),
        "-r",
        &format!("{}/{}", fps.numerator, fps.denominator),
        "-i",
        "-",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(a) = audio {
        args.push("-i".into());
        args.push(a.display().to_string());
        for s in ["-map", "0:v:0", "-map", "1:a:0"] {
            args.push(s.to_string());
        }
    } else {
        args.push("-an".into());
    }
    for s in [
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "20",
        "-pix_fmt",
        "yuv420p",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-movflags",
        "+faststart",
    ] {
        args.push(s.to_string());
    }
    if audio.is_some() {
        for s in ["-c:a", "aac", "-b:a", "192k", "-shortest"] {
            args.push(s.to_string());
        }
    }
    args.push(out.display().to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps(n: u32, d: u32) -> Rational {
        Rational {
            numerator: n,
            denominator: d,
        }
    }

    #[test]
    fn odd_dimensions_rejected_before_spawn() {
        for (w, h) in [(63, 36), (64, 35), (1, 1)] {
            match Encoder::open(Path::new("x.mp4"), w, h, &fps(30, 1)) {
                Err(e) => assert!(e.to_string().contains("even"), "{w}x{h}: {e}"),
                Ok(_) => panic!("{w}x{h} should be rejected"),
            }
        }
    }

    #[test]
    fn args_carry_stream_shape_and_color() {
        let args = encode_args(Path::new("o.mp4"), 64, 36, &fps(30000, 1001), None);
        let pairs: Vec<(&str, &str)> = args
            .windows(2)
            .filter_map(|w| (w[0].starts_with('-')).then_some((w[0].as_str(), w[1].as_str())))
            .collect();
        let get = |flag: &str| pairs.iter().find(|(f, _)| *f == flag).map(|(_, v)| *v);
        assert_eq!(get("-f"), Some("rawvideo"));
        assert_eq!(get("-s"), Some("64x36"));
        // Rational fps reaches ffmpeg unquantized — NTSC stays exact.
        assert_eq!(get("-r"), Some("30000/1001"));
        assert_eq!(get("-c:v"), Some("libx264"));
        assert_eq!(args.last().map(String::as_str), Some("o.mp4"));
        assert!(args.iter().any(|a| a == "-an"), "silent program, no audio");
        for flag in ["-colorspace", "-color_primaries", "-color_trc"] {
            assert_eq!(get(flag), Some("bt709"), "{flag}");
        }
    }

    #[test]
    fn muxed_args_map_both_inputs() {
        let args = encode_args(
            Path::new("o.mp4"),
            64,
            36,
            &fps(30, 1),
            Some(Path::new("mix.wav")),
        );
        let pos = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        // two inputs: stdin rawvideo first, wav second
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 2);
        assert_eq!(args[pos("-i") + 1], "-");
        assert!(args.iter().any(|a| a == "mix.wav"));
        assert!(!args.iter().any(|a| a == "-an"), "muxed keeps audio");
        assert!(args.windows(2).any(|w| w == ["-map", "0:v:0"]));
        assert!(args.windows(2).any(|w| w == ["-map", "1:a:0"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "aac"]));
        assert!(args.iter().any(|a| a == "-shortest"));
        assert_eq!(args.last().map(String::as_str), Some("o.mp4"));
    }

    #[test]
    fn write_after_finish_is_an_error_not_a_panic() {
        // Can't open a real encoder without ffmpeg; exercise the closed
        // stdin path on a value constructed by hand.
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer); // EOF — the drain thread exits, so Drop::join returns
        let mut enc = Encoder {
            child: Command::new("true").spawn().unwrap(),
            stdin: None,
            stderr: StderrDrain::start(reader),
            finished: false, // Drop waits on the child — no zombie
        };
        assert!(enc.write_frame(&[0]).is_err());
    }
}
