//! Per-hop metrics — pure math on decoded pixels and PCM. Everything
//! here is deterministic and subprocess-free; the same functions feed
//! the live perceive pass and the tests.
//!
//! Normalization is fixed-scale where thresholds are tuned against it
//! (`motion`), and rolling-window where the absolute scale is
//! content-dependent (`sharpness`, `flux` onsets).

use rustfft::{Fft, num_complex::Complex32};
use scene_media::Frame;
use scene_time::{TimingMap, Word};

/// Per-frame visual metrics — one row per analysis frame.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrameMetrics {
    pub frame: u64,
    /// Seconds into the source.
    pub t: f64,
    /// Laplacian variance normalized by a rolling-window p95, 0..1+.
    /// Set by [`normalize_sharpness`]; raw value in `sharp_raw`.
    pub sharpness: f64,
    /// Raw Laplacian variance of the luma plane.
    pub sharp_raw: f64,
    /// Mean abs RGB diff vs the previous frame, fixed scale /32, 0..1.
    /// A hard cut saturates (~80+/32); shake reads mid; a static shot ~0.
    pub motion: f64,
    /// Mean luma /255, 0..1.
    pub brightness: f64,
    /// (p95 − p5) luma /255, 0..1 — low contrast is haze/flat.
    pub contrast: f64,
    /// 0 at frame 0; afterwards the inter-frame signature distance —
    /// dHash hamming/64 in offline mode, 1−cosine once `embed` runs.
    pub change: f64,
    /// 8×8 difference hash — the always-available visual signature.
    /// Serialized as a u64; embeddings (if any) live in a sidecar file.
    pub dhash: u64,
    /// Zero-shot tags — only populated in embed mode, on candidates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Per-hop audio metrics — one row per analysis frame (same index).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioHop {
    /// Index of the video frame this hop covers.
    pub frame: u64,
    pub t: f64,
    /// RMS level, dBFS, floored at -100.
    pub loud_db: f64,
    /// Spectral flux — summed positive magnitude delta vs the previous
    /// hop. Raw scale; onset detection is relative (median+MAD).
    pub flux: f64,
    /// Onset strength at this hop (0 when not an onset) — set by
    /// [`detect_onsets`].
    pub onset: f64,
    /// True when the hop sits in a sustained low-energy run.
    pub silence: bool,
    /// The aligned word covering `t`, when timings were supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub word: Option<String>,
    /// True when `t` falls inside an aligned word.
    pub speech: bool,
}

impl AudioHop {
    /// A hop with no audio stream (or past its end) — dead air.
    pub fn silent(frame: u64, t: f64) -> AudioHop {
        AudioHop {
            frame,
            t,
            loud_db: -100.0,
            flux: 0.0,
            onset: 0.0,
            silence: true,
            word: None,
            speech: false,
        }
    }
}

// ---------- visual ----------

/// Luma plane (Rec.601) for an RGBA frame — all visual metrics run on
/// luma, not the four-channel bytes.
pub fn luma(frame: &Frame) -> Vec<u8> {
    frame
        .pixels
        .chunks_exact(4)
        .map(|p| ((299 * p[0] as u32 + 587 * p[1] as u32 + 114 * p[2] as u32) / 1000) as u8)
        .collect()
}

/// Mean abs RGB diff between two same-size frames (alpha ignored).
/// Raw byte units 0..255 — the caller applies the fixed scale.
pub fn frame_diff(prev: &Frame, cur: &Frame) -> f64 {
    debug_assert_eq!(prev.pixels.len(), cur.pixels.len());
    let mut sum = 0u64;
    let mut n = 0u64;
    for (a, b) in prev.pixels.chunks_exact(4).zip(cur.pixels.chunks_exact(4)) {
        sum += (a[0] as i32 - b[0] as i32).unsigned_abs() as u64;
        sum += (a[1] as i32 - b[1] as i32).unsigned_abs() as u64;
        sum += (a[2] as i32 - b[2] as i32).unsigned_abs() as u64;
        n += 3;
    }
    if n == 0 { 0.0 } else { sum as f64 / n as f64 }
}

/// Mean luma 0..1 — the brightness metric.
pub fn mean_luma(l: &[u8]) -> f64 {
    if l.is_empty() {
        return 0.0;
    }
    l.iter().map(|&v| v as u64).sum::<u64>() as f64 / (l.len() as f64 * 255.0)
}

/// (p5, p95) of luma via a 256-bin histogram — cheap and exact enough
/// for contrast, no sorting.
pub fn luma_percentiles(l: &[u8]) -> (f64, f64) {
    if l.is_empty() {
        return (0.0, 0.0);
    }
    let mut bins = [0u64; 256];
    for &v in l {
        bins[v as usize] += 1;
    }
    let n = l.len() as u64;
    let at = |p: u64| {
        let mut acc = 0u64;
        let target = n * p / 100;
        for (v, &c) in bins.iter().enumerate() {
            acc += c;
            if acc > target {
                return v as f64 / 255.0;
            }
        }
        1.0
    };
    (at(5), at(95))
}

/// Variance of the 3×3 Laplacian over luma — high when the frame has
/// crisp edges, ~0 on blur/mush. Raw units; normalized by the caller.
pub fn laplacian_var(l: &[u8], w: usize, h: usize) -> f64 {
    if w < 3 || h < 3 || l.len() < w * h {
        return 0.0;
    }
    let mut sum = 0f64;
    let mut sum_sq = 0f64;
    let mut n = 0u64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let lap = 4 * l[i] as i32
                - l[i - 1] as i32
                - l[i + 1] as i32
                - l[i - w] as i32
                - l[i + w] as i32;
            sum += lap as f64;
            sum_sq += (lap * lap) as f64;
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    let mean = sum / n as f64;
    (sum_sq / n as f64 - mean * mean).max(0.0)
}

/// dHash: downsample luma to 9×8 cell averages, then bit i is set when
/// cell[i] > cell[i+1] (left-to-right). Hamming distance between hashes
/// is the offline-mode signature distance (`change = dist/64`).
pub fn dhash64(l: &[u8], w: usize, h: usize) -> u64 {
    let mut cell = [0u32; 9 * 8];
    let mut count = [0u32; 9 * 8];
    for y in 0..h.min(l.len() / w.max(1)) {
        for x in 0..w {
            let c = (x * 9 / w).min(8) + 9 * (y * 8 / h).min(7);
            cell[c] += l[y * w + x] as u32;
            count[c] += 1;
        }
    }
    let mut bits = 0u64;
    for row in 0..8 {
        for col in 0..8 {
            let a = cell[row * 9 + col] as f64 / count[row * 9 + col].max(1) as f64;
            let b = cell[row * 9 + col + 1] as f64 / count[row * 9 + col + 1].max(1) as f64;
            if a > b {
                bits |= 1 << (row * 8 + col);
            }
        }
    }
    bits
}

/// Normalize raw sharpness by the rolling-window p95 (±`radius` frames
/// each side) — adaptive to the clip's own crispness without making a
/// static clip's noise floor look sharp.
pub fn normalize_sharpness(frames: &mut [FrameMetrics], radius: usize) {
    let raws: Vec<f64> = frames.iter().map(|f| f.sharp_raw).collect();
    for (i, f) in frames.iter_mut().enumerate() {
        let lo = i.saturating_sub(radius);
        let hi = (i + radius + 1).min(raws.len());
        let mut win: Vec<f64> = raws[lo..hi].to_vec();
        win.sort_by(f64::total_cmp);
        let p95 = win
            .get((win.len() as f64 * 0.95) as usize)
            .copied()
            .unwrap_or(0.0);
        f.sharpness = (f.sharp_raw / p95.max(1e-9)).clamp(0.0, 1.0);
    }
}

// ---------- audio ----------

/// RMS level in dBFS, floored at −100 dB.
pub fn rms_db(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return -100.0;
    }
    let rms = (samples.iter().map(|&s| (s * s) as f64).sum::<f64>() / samples.len() as f64).sqrt();
    (20.0 * rms.log10()).max(-100.0)
}

/// Spectral flux over fixed-size hops: Hann window → FFT → summed
/// positive magnitude deltas. One instance per stream — `prev` carries
/// the last hop's spectrum.
pub struct SpectralFlux {
    fft: std::sync::Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    prev: Vec<f64>,
    /// First push has no previous spectrum — reporting the whole
    /// spectrum as "new" would fabricate an onset at hop 0.
    primed: bool,
}

impl SpectralFlux {
    pub fn new(hop_samples: usize) -> SpectralFlux {
        let fft = rustfft::FftPlanner::new().plan_fft_forward(hop_samples);
        // Hann window — smooths the hop edges so a steady tone doesn't
        // read as constant novelty.
        let window = (0..hop_samples)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / hop_samples as f32).cos())
            .collect();
        SpectralFlux {
            fft,
            window,
            prev: vec![0.0; hop_samples / 2 + 1],
            primed: false,
        }
    }

    /// Flux of one hop — ≥0, larger when spectral content appears.
    pub fn push(&mut self, samples: &[f32]) -> f64 {
        let n = self.window.len();
        let mut buf: Vec<Complex32> = (0..n)
            .map(|i| Complex32::new(samples.get(i).copied().unwrap_or(0.0) * self.window[i], 0.0))
            .collect();
        self.fft.process(&mut buf);
        let mut flux = 0.0;
        for (i, c) in buf.iter().take(self.prev.len()).enumerate() {
            let m = c.norm() as f64;
            if self.primed {
                let d = m - self.prev[i];
                if d > 0.0 {
                    flux += d;
                }
            }
            self.prev[i] = m;
        }
        self.primed = true;
        flux / self.prev.len() as f64
    }
}

/// Median+`k`·MAD spike detection with a minimum gap — strongest spike
/// wins within a gap window. Shared by visual `change` and audio `flux`
/// onset detection; mirrors `scene_adapt`'s cut detector shape.
pub fn detect_spikes(vals: &[f64], k: f64, floor: f64, min_gap: usize) -> Vec<usize> {
    if vals.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<f64> = vals.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let mut dev: Vec<f64> = vals.iter().map(|v| (v - median).abs()).collect();
    dev.sort_by(f64::total_cmp);
    let mad = dev[dev.len() / 2];
    let threshold = (median + k * mad).max(floor);
    let mut out: Vec<usize> = Vec::new();
    let mut last: Option<usize> = None;
    for (i, &v) in vals.iter().enumerate() {
        // Strictly above the threshold — a flat signal at the threshold
        // value is not a spike, and a floor of 0 must not admit zeros.
        if v <= threshold {
            continue;
        }
        match last {
            Some(l) if i - l < min_gap => {
                // Inside the gap — keep whichever spike is stronger.
                if v > vals[out.last().copied().unwrap_or(l)]
                    && let Some(slot) = out.last_mut()
                {
                    *slot = i;
                }
            }
            _ => {
                out.push(i);
                last = Some(i);
            }
        }
    }
    out
}

/// Mark onset hops from a flux curve; writes `hop.onset` with the
/// hop's spike height. `silence` is also set here — a hop is silent
/// when below `silence_db` (a fixed dBFS floor).
pub fn detect_onsets(hops: &mut [AudioHop], k: f64, silence_db: f64) {
    for hop in hops.iter_mut() {
        hop.silence = hop.loud_db < silence_db;
    }
    let flux: Vec<f64> = hops.iter().map(|h| h.flux).collect();
    for i in detect_spikes(&flux, k, 0.0, 2) {
        // A flux spike during silence is noise, not a beat.
        if !hops[i].silence {
            hops[i].onset = flux[i];
        }
    }
}

// ---------- word lattice ----------

/// Every aligned word across every timing source, sorted by start —
/// the meme has one audio bed, so merging sources is correct here.
pub fn words_of(timings: &TimingMap) -> Vec<Word> {
    let mut words: Vec<Word> = timings
        .sources
        .values()
        .flat_map(|s| s.lines.iter().flat_map(|l| l.words.iter().cloned()))
        .collect();
    words.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
    words
}

/// The word covering `t` (start ≤ t < end), if any.
pub fn word_at(words: &[Word], t: f64) -> Option<&Word> {
    // Sorted by start — partition_point finds the last word starting
    // at or before t; it covers t iff its end is past t.
    let i = words.partition_point(|w| w.start_s <= t);
    let w = words.get(i.checked_sub(1)?)?;
    (w.end_s > t).then_some(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scene_time::{AlignedLine, TimingSource};

    fn frame_of(v: u8, w: u32, h: u32) -> Frame {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for p in pixels.chunks_exact_mut(4) {
            p.copy_from_slice(&[v, v, v, 255]);
        }
        Frame {
            index: 0,
            width: w,
            height: h,
            pixels,
        }
    }

    /// Left half dark, right half bright — edges in both directions.
    fn split_frame(w: u32, h: u32) -> Frame {
        let (w, h) = (w as usize, h as usize);
        let mut pixels = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 30 } else { 220 };
                pixels[(y * w + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        Frame {
            index: 0,
            width: w as u32,
            height: h as u32,
            pixels,
        }
    }

    fn sine(freq: f32, amp: f32, n: usize, rate: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate).sin())
            .collect()
    }

    #[test]
    fn diff_and_brightness_basics() {
        let a = frame_of(0, 8, 8);
        let b = frame_of(64, 8, 8);
        assert_eq!(frame_diff(&a, &b), 64.0);
        assert_eq!(frame_diff(&a, &a), 0.0);
        assert_eq!(mean_luma(&luma(&frame_of(255, 8, 8))), 1.0);
        assert_eq!(mean_luma(&luma(&frame_of(0, 8, 8))), 0.0);
    }

    #[test]
    fn contrast_reads_dynamic_range() {
        let flat = frame_of(128, 16, 16);
        let (lo, hi) = luma_percentiles(&luma(&flat));
        assert_eq!(hi - lo, 0.0);
        let split = split_frame(16, 16);
        let (lo, hi) = luma_percentiles(&luma(&split));
        assert!((hi - lo) > 0.7, "split frame should be high-contrast");
    }

    #[test]
    fn sharpness_separates_edges_from_flat() {
        let flat = frame_of(128, 16, 16);
        let split = split_frame(16, 16);
        let lf = laplacian_var(&luma(&flat), 16, 16);
        let ls = laplacian_var(&luma(&split), 16, 16);
        assert_eq!(lf, 0.0);
        assert!(ls > 1000.0, "edge frame should read sharp, got {ls}");
    }

    /// Vertical stripes alternating per dHash cell — maximal horizontal
    /// gradient signal (a flat or single-edge frame exercises almost no
    /// bits, so the test needs texture at cell scale).
    fn stripes(w: u32, h: u32, a: u8, b: u8) -> Frame {
        let (w, h) = (w as usize, h as usize);
        let mut pixels = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = if (x * 9 / w) % 2 == 0 { a } else { b };
                pixels[(y * w + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        Frame {
            index: 0,
            width: w as u32,
            height: h as u32,
            pixels,
        }
    }

    #[test]
    fn dhash_similar_frames_close_distant_frames_far() {
        let a = stripes(18, 16, 30, 220);
        let b = stripes(18, 16, 30, 220);
        let flat = frame_of(150, 18, 16);
        let inv = stripes(18, 16, 220, 30);
        let (ha, hb) = (dhash64(&luma(&a), 18, 16), dhash64(&luma(&b), 18, 16));
        let hc = dhash64(&luma(&flat), 18, 16);
        let hd = dhash64(&luma(&inv), 18, 16);
        assert_eq!(ha, hb, "identical frames hash identically");
        let dist_flat = (ha ^ hc).count_ones() as f64 / 64.0;
        let dist_inv = (ha ^ hd).count_ones() as f64 / 64.0;
        assert!(
            dist_flat > 0.2,
            "stripes vs flat should differ, {dist_flat}"
        );
        assert!(
            dist_inv > 0.9,
            "inverted stripes flip every bit, {dist_inv}"
        );
    }

    #[test]
    fn rms_db_reads_levels() {
        assert_eq!(rms_db(&[]), -100.0);
        assert_eq!(rms_db(&[0.0; 512]), -100.0);
        let loud = sine(440.0, 1.0, 512, 15360.0);
        let db = rms_db(&loud);
        assert!((db - -3.0).abs() < 0.2, "full-amp sine is ~-3dB, got {db}");
        let quiet = sine(440.0, 0.01, 512, 15360.0);
        assert!(rms_db(&quiet) < -40.0);
    }

    #[test]
    fn flux_fires_on_new_tone_not_steady_tone() {
        let mut flux = SpectralFlux::new(512);
        // Steady 440 Hz — after the first hop, flux should be near zero.
        let steady = sine(440.0, 0.5, 512 * 6, 15360.0);
        let mut vals = Vec::new();
        for hop in steady.chunks(512) {
            vals.push(flux.push(hop));
        }
        // Hop 0 has no previous spectrum — reporting one would fabricate
        // an onset at t=0 on every clip.
        assert_eq!(vals[0], 0.0, "first hop primes, never spikes");
        let later = vals[3..].iter().copied().fold(0.0, f64::max);
        assert!(later < 0.05, "steady tone stays quiet, got {later}");

        // Switching frequencies mid-stream spikes flux once.
        let mut flux = SpectralFlux::new(512);
        for hop in sine(440.0, 0.5, 512 * 3, 15360.0).chunks(512) {
            flux.push(hop);
        }
        let spike = flux.push(&sine(880.0, 0.5, 512, 15360.0));
        let after = flux.push(&sine(880.0, 0.5, 512, 15360.0));
        assert!(spike > after * 4.0, "freq change {spike} >> steady {after}");
    }

    #[test]
    fn spike_detection_gaps_and_floors() {
        // Flat signal — nothing.
        assert!(detect_spikes(&[1.0; 50], 6.0, 8.0, 4).is_empty());
        // One clear spike above the floor.
        let mut v = vec![1.0; 50];
        v[20] = 50.0;
        assert_eq!(detect_spikes(&v, 6.0, 8.0, 4), vec![20]);
        // Two spikes inside min_gap — the stronger survives.
        v[22] = 40.0;
        assert_eq!(detect_spikes(&v, 6.0, 8.0, 4), vec![20]);
        v[22] = 80.0;
        assert_eq!(detect_spikes(&v, 6.0, 8.0, 4), vec![22]);
        // Far enough apart — both kept.
        v[30] = 60.0;
        assert_eq!(detect_spikes(&v, 6.0, 8.0, 4), vec![22, 30]);
    }

    #[test]
    fn onsets_skip_silence() {
        let mut hops: Vec<AudioHop> = (0..10)
            .map(|i| AudioHop {
                frame: i,
                t: i as f64 / 30.0,
                loud_db: -60.0,
                flux: if i == 5 { 100.0 } else { 0.0 },
                onset: 0.0,
                silence: false,
                word: None,
                speech: false,
            })
            .collect();
        detect_onsets(&mut hops, 3.0, -50.0);
        // The spike landed in a silent hop — not a beat.
        assert!(hops[5].onset == 0.0);
        assert!(hops.iter().all(|h| h.silence));

        for h in hops.iter_mut() {
            h.loud_db = -20.0;
        }
        detect_onsets(&mut hops, 3.0, -50.0);
        assert!(hops[5].onset > 0.0, "loud flux spike is a beat");
        assert!(hops.iter().all(|h| !h.silence));
    }

    #[test]
    fn word_lookup() {
        let mut map = TimingMap::default();
        map.insert(
            "voice",
            TimingSource {
                lines: vec![AlignedLine {
                    cue: "l1".into(),
                    words: vec![
                        Word {
                            text: "boom".into(),
                            start_s: 0.5,
                            end_s: 0.8,
                        },
                        Word {
                            text: "crash".into(),
                            start_s: 1.0,
                            end_s: 1.4,
                        },
                    ],
                }],
            },
        );
        let words = words_of(&map);
        assert_eq!(words.len(), 2);
        assert_eq!(word_at(&words, 0.6).unwrap().text, "boom");
        // t == word.end is the gap between words — end is exclusive.
        assert!(word_at(&words, 0.8).is_none());
        assert!(word_at(&words, 0.9).is_none());
        assert_eq!(word_at(&words, 1.2).unwrap().text, "crash");
        assert!(word_at(&words, 2.0).is_none());
        assert!(word_at(&words, 0.1).is_none());
    }
}
