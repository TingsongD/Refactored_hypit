//! The perceive pass: one interleaved decode of video frames (scaled to
//! the analysis grid) and mono PCM hops, indexed identically — hop i
//! covers the same time span as frame i. Pure metric math lives in
//! `metrics.rs`; this file only drives the streams.

use std::path::Path;

use scene_media::{FrameStream, MediaInfo, PcmStream};
use scene_time::TimingMap;

use crate::brief::Brief;
use crate::error::MemeError;
use crate::metrics::{
    self, AudioHop, FrameMetrics, SpectralFlux, dhash64, frame_diff, laplacian_var, luma,
    luma_percentiles, mean_luma, rms_db, word_at, words_of,
};

/// Samples per audio hop — decode at `fps * HOP`, so hop i spans the
/// same `[i/fps, (i+1)/fps)` as video frame i. 512 is a power of two
/// (cheap FFT) and ~33 ms at 30 fps — fine enough for onsets.
const HOP: usize = 512;
/// Fixed motion scale: mean abs RGB diff /32 → 0..1. A hard cut
/// saturates (~80+/32); shake reads mid; a static shot is ~0.
const MOTION_SCALE: f64 = 32.0;
/// Median+6·MAD for flux spikes — same shape as `detect_cuts`.
const ONSET_K: f64 = 6.0;
/// A hop below −50 dBFS counts as silence — silence can't carry a beat.
const SILENCE_DB: f64 = -50.0;
/// Sharpness normalization window ≈ 3 s each side.
fn sharp_radius(fps: f64) -> usize {
    (fps * 3.0).round() as usize
}

/// Everything perceive learned from the media, in frame-indexed arrays.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Perceive {
    pub fps: f64,
    pub duration_s: f64,
    pub has_audio: bool,
    /// `"dhash"` or the encoder name — stamped so a cached metrics file
    /// can't be confused for a different signature space.
    pub encoder: String,
    pub frames: Vec<FrameMetrics>,
    pub audio: Vec<AudioHop>,
}

/// Decode `path` once and collect per-hop visual + audio metrics.
/// `timings` supplies the word lattice (empty map → no word/speech).
pub fn perceive(
    path: &Path,
    info: &MediaInfo,
    brief: &Brief,
    timings: &TimingMap,
) -> Result<Perceive, MemeError> {
    let fps = brief.fps;
    let size = brief.perceive_size;
    let mut frames = FrameStream::open_scaled(path, info, size, size, fps)?;
    let rate = (fps * HOP as f64).round() as u32;
    let mut pcm = PcmStream::open(path, info, rate, HOP)?;
    let mut flux = SpectralFlux::new(HOP);
    let words = words_of(timings);

    let mut out_frames = Vec::new();
    let mut out_audio = Vec::new();
    let mut prev_frame: Option<scene_media::Frame> = None;
    let mut prev_dhash = 0u64;

    for frame in frames.by_ref() {
        let frame = frame?;
        let i = frame.index;
        let t = i as f64 / fps;
        let l = luma(&frame);
        let dhash = dhash64(&l, size as usize, size as usize);
        let (p5, p95) = luma_percentiles(&l);
        let motion = prev_frame
            .as_ref()
            .map(|p| (frame_diff(p, &frame) / MOTION_SCALE).clamp(0.0, 1.0))
            .unwrap_or(0.0);
        // dHash mode: hamming distance is the inter-frame signature.
        // An `embed` connector overwrites `change` with cosine later.
        let change = if i == 0 {
            0.0
        } else {
            (prev_dhash ^ dhash).count_ones() as f64 / 64.0
        };
        out_frames.push(FrameMetrics {
            frame: i,
            t,
            sharpness: 0.0,
            sharp_raw: laplacian_var(&l, size as usize, size as usize),
            motion,
            brightness: mean_luma(&l),
            contrast: (p95 - p5).max(0.0),
            change,
            dhash,
            tags: Vec::new(),
        });
        prev_frame = Some(frame);
        prev_dhash = dhash;

        // One audio hop per frame, same index — stream end or no audio
        // stream both read as silence.
        let hop = match pcm.as_mut().and_then(|s| s.next()) {
            Some(chunk) => {
                let chunk = chunk?;
                let word = word_at(&words, t).map(|w| w.text.clone());
                AudioHop {
                    frame: i,
                    t,
                    loud_db: rms_db(&chunk.samples),
                    flux: flux.push(&chunk.samples),
                    onset: 0.0,
                    silence: false,
                    speech: word.is_some(),
                    word,
                }
            }
            None => AudioHop::silent(i, t),
        };
        out_audio.push(hop);
    }

    metrics::normalize_sharpness(&mut out_frames, sharp_radius(fps));
    metrics::detect_onsets(&mut out_audio, ONSET_K, SILENCE_DB);

    Ok(Perceive {
        fps,
        duration_s: out_frames.len() as f64 / fps,
        has_audio: pcm.is_some(),
        encoder: brief.encoder.clone(),
        frames: out_frames,
        audio: out_audio,
    })
}
