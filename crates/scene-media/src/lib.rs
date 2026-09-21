//! scene-media — external media access, all through ffmpeg/ffprobe
//! subprocesses. No codecs in-process; ffmpeg is the one media dependency.
//!
//! - [`probe`] — `ffprobe` JSON → [`MediaInfo`]
//! - [`FrameStream`] — sequential `ffmpeg` rawvideo decode → RGBA frames
//! - [`Encoder`] — NV12 program stream → H.264/mp4

mod confine;
mod decode;
mod encode;
mod error;
mod output;
mod probe;
mod proc;
mod stderr;
mod watchdog;

pub use confine::{Escapes, confine_under_root};
pub use decode::{Frame, FrameStream};
pub use encode::Encoder;
pub use error::{MediaError, Tool};
pub use output::StagedOutput;
pub use probe::{AudioInfo, MediaInfo, VideoInfo, parse_probe_json, probe};
pub use proc::{ProcOutput, output_timeout, run_with_input_timeout, spawn_grouped, wait_timeout};
pub use watchdog::{Heartbeat, StallWatchdog, beat, heartbeat, watch_io};
