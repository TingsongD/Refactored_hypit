//! Runs the emitted filtergraph: clips → one 48 kHz stereo program WAV.
//! The WAV is what `scene-media::Encoder` muxes against the NV12 pipe —
//! keeping the audio render a separate pass means the encode process
//! stays a dumb stdin→file pipe either way.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use scene_media::{MediaError, Tool, output_timeout};

use crate::emit::mix_args;
use crate::graph::AudioGraph;

/// A mix is O(program length) and local — ten minutes is a hang, not a
/// slow render.
const MIX_TIMEOUT: Duration = Duration::from_secs(600);

/// Render `graph` to `out_wav`. Empty graphs are a caller error.
pub fn mix_program(graph: &AudioGraph, out_wav: &Path) -> Result<(), MediaError> {
    debug_assert!(!graph.clips.is_empty(), "mixing nothing is a bug");
    let tool = std::env::var("FFMPEG").unwrap_or_else(|_| Tool::Ffmpeg.name().to_string());
    let output = output_timeout(
        Command::new(&tool).args(mix_args(graph, out_wav)),
        "ffmpeg",
        MIX_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(MediaError::Failed {
            tool: "ffmpeg",
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}
