//! Shot detection on thinned-out frames. Decode at a small size and low
//! fps, score each consecutive pair by mean absolute pixel difference,
//! and call a cut where the score spikes. All pure pieces take plain
//! data so the tests never touch ffmpeg.

use crate::AdaptError;
use scene_media::{Frame, FrameStream};
use std::path::Path;

/// Analysis resolution/frame-rate. Small enough that a 10-minute clip
/// decodes in seconds, big enough that a hard cut is unmistakable.
const SAMPLE_W: u32 = 96;
const SAMPLE_H: u32 = 54;
const SAMPLE_FPS: f64 = 4.0;
/// Shots shorter than this are likely transition noise — merge them.
const MIN_SHOT_S: f64 = 0.5;

/// What we learned about a media file.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub path: std::path::PathBuf,
    pub duration_s: f64,
    pub width: u32,
    pub height: u32,
    /// Source average frame rate (as a rational `num/den` pair).
    pub fps: Option<(u32, u32)>,
    pub has_audio: bool,
    /// Cut times in seconds, ascending, strictly inside (0, duration).
    pub cuts: Vec<f64>,
}

/// Mean absolute byte difference between two same-size frames, 0–255.
/// Returns `None` for mismatched buffers instead of panicking.
pub fn frame_diff(a: &[u8], b: &[u8]) -> Option<f64> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let sum: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    Some(sum as f64 / a.len() as f64)
}

/// Pick cut points from per-frame-pair diff `scores` sampled at `fps`.
///
/// A cut is a local spike: score above an adaptive threshold (mean +
/// 4σ, floor 8.0 so static-ish footage never "cuts"), then enforced
/// minimum spacing of `min_shot_s`. Returns times in seconds.
pub fn detect_cuts(scores: &[f64], fps: f64, min_shot_s: f64) -> Vec<f64> {
    if scores.len() < 2 || fps <= 0.0 {
        return Vec::new();
    }
    // Robust threshold: median + 6·MAD. Mean/σ would let the spikes
    // themselves inflate the cutoff above the spikes — the very cuts
    // we're hunting would mask themselves. Floor at 8.0 so static
    // footage never produces phantom cuts.
    let mut sorted = scores.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let mut dev: Vec<f64> = scores.iter().map(|s| (s - median).abs()).collect();
    dev.sort_by(f64::total_cmp);
    let mad = dev[dev.len() / 2];
    let threshold = (median + 6.0 * mad).max(8.0);
    let min_gap_frames = (min_shot_s * fps).max(1.0);

    // Candidate spikes as (frame, score); within a min-gap window only
    // the strongest survives.
    let mut spikes: Vec<(f64, f64)> = Vec::new();
    for (i, &score) in scores.iter().enumerate() {
        if score <= threshold {
            continue;
        }
        let frame = (i + 1) as f64; // score[i] compares frame i → i+1
        match spikes.last_mut() {
            Some((prev_frame, prev_score)) if frame - *prev_frame < min_gap_frames => {
                if score > *prev_score {
                    *prev_frame = frame;
                    *prev_score = score;
                }
            }
            _ => spikes.push((frame, score)),
        }
    }
    spikes.iter().map(|(frame, _)| frame / fps).collect()
}

/// Probe + shot-detect `path` (already local — see [`crate::ingest`]).
pub fn analyze(path: &Path) -> Result<Analysis, AdaptError> {
    let info = scene_media::probe(path)?;
    let video = info
        .video
        .as_ref()
        .ok_or(scene_media::MediaError::NoVideoStream)?;

    let stream = FrameStream::open_scaled(path, &info, SAMPLE_W, SAMPLE_H, SAMPLE_FPS)?;
    let mut scores = Vec::new();
    let mut prev: Option<Frame> = None;
    for frame in stream {
        let frame = frame.map_err(AdaptError::Media)?;
        if let Some(p) = &prev
            && let Some(d) = frame_diff(&p.pixels, &frame.pixels)
        {
            scores.push(d);
        }
        prev = Some(frame);
    }

    let fps = video.frame_rate.map(|r| (r.numerator, r.denominator));
    let mut cuts = detect_cuts(&scores, SAMPLE_FPS, MIN_SHOT_S);
    // Never emit a cut at/past the end.
    cuts.retain(|&t| t > 0.0 && t < info.duration_s - 0.05);

    Ok(Analysis {
        path: path.to_path_buf(),
        duration_s: info.duration_s,
        width: video.width,
        height: video.height,
        fps,
        has_audio: info.audio.is_some(),
        cuts,
    })
}
