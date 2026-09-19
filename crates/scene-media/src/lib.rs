//! scene-media — external media access, all through ffmpeg/ffprobe
//! subprocesses. No codecs in-process; ffmpeg is the one media dependency.
//!
//! - [`probe`] — `ffprobe` JSON → [`MediaInfo`]
//! - [`FrameStream`] — sequential `ffmpeg` rawvideo decode → RGBA frames
//! - [`Encoder`] — NV12 program stream → H.264/mp4

mod decode;
mod encode;
mod error;
mod probe;
mod stderr;

pub use decode::{Frame, FrameStream};
pub use encode::Encoder;
pub use error::{MediaError, Tool};
pub use probe::{AudioInfo, MediaInfo, VideoInfo, parse_probe_json, probe};
