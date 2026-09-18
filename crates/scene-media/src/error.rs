//! Error type for media operations. Tools are external processes; every
//! failure names the tool and carries its stderr when available.

use std::io;

/// Which external binary an operation needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Ffprobe => "ffprobe",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("could not run {tool}: {source}")]
    Spawn {
        tool: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("{tool} exited with {status}: {stderr}")]
    Failed {
        tool: &'static str,
        status: String,
        stderr: String,
    },

    #[error("could not parse ffprobe output: {0}")]
    ProbeParse(String),

    #[error("media has no video stream")]
    NoVideoStream,

    #[error("unexpected end of stream: expected {expected} bytes, got {got}")]
    ShortFrame { expected: usize, got: usize },

    #[error(transparent)]
    Io(#[from] io::Error),
}
