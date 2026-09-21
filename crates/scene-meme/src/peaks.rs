//! Peak pick — pure code, no model. Fused candidate generation: visual
//! change spikes, audio onsets, and local maxima of `importance` are
//! co-equal generators; a coincidence bonus rewards cuts that land on
//! the beat. Dedup runs against the final kept set, not mid-iteration.

use scene_time::Word;

use crate::brief::Brief;
use crate::metrics::{detect_spikes, word_at};
use crate::perceive::Perceive;

/// What produced the candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeakSource {
    /// `change` spike — a visual cut or punch-in.
    Visual,
    /// Audio onset with no coincident visual spike — a sfx/voice hit.
    Audio,
    /// A local maximum of `importance` alone.
    Importance,
    /// Visual spike ∧ onset within the snap radius — cut on the beat.
    Fused,
}

/// One candidate beat — everything Jev/Gemini need, nothing they must
/// not see. `frame` is the representative still (sharpest in the snap
/// window); `t` is the beat time, snapped to an onset or word boundary
/// when one is near.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Candidate {
    /// `f{frame}` — stable id for Jev fact rows and frame files.
    pub id: String,
    pub frame: u64,
    pub t: f64,
    pub change: f64,
    pub sharpness: f64,
    pub motion: f64,
    pub brightness: f64,
    pub contrast: f64,
    pub importance: f64,
    /// `change < change_skip` — the beat fired on a static shot.
    pub same_shot: bool,
    /// Visual spike ∧ audio onset within the snap radius.
    pub cut_on_beat: bool,
    /// Normalized onset strength at the snapped beat time (0 if none).
    pub onset: f64,
    /// Aligned word at `t`, when timings were supplied.
    pub word: Option<String>,
    pub source: PeakSource,
    /// Max signature similarity to any earlier keep (0..1; 0 if none).
    /// Jev's own `too_similar` judgement is a separate field downstream.
    pub sim_to_kept: f64,
    /// Zero-shot labels — populated only in embed mode, only on
    /// candidates (never every frame).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Similarity between two frames: cosine on embeddings when present,
/// else `1 - hamming(dhash)/64`. Both land in 0..1 where 1 is identical.
fn similarity(p: &Perceive, emb: Option<&[Vec<f32>]>, a: u64, b: u64) -> f64 {
    if let Some(emb) = emb
        && let (Some(x), Some(y)) = (emb.get(a as usize), emb.get(b as usize))
    {
        let mut dot = 0.0;
        let mut na = 0.0;
        let mut nb = 0.0;
        for (&u, &v) in x.iter().zip(y.iter()) {
            dot += (u * v) as f64;
            na += (u * u) as f64;
            nb += (v * v) as f64;
        }
        let denom = (na * nb).sqrt();
        if denom > 0.0 {
            return (dot / denom).clamp(-1.0, 1.0).mul_add(0.5, 0.5); // → 0..1
        }
    }
    // Two frames must match in BOTH structure (dHash) and level (luma)
    // to count as the same beat — flat-color shots share a hash but a
    // red beat and a blue beat are different beats.
    let (ha, hb) = (p.frames[a as usize].dhash, p.frames[b as usize].dhash);
    let struct_dist = (ha ^ hb).count_ones() as f64 / 64.0;
    let level_dist =
        (4.0 * (p.frames[a as usize].brightness - p.frames[b as usize].brightness).abs()).min(1.0);
    1.0 - struct_dist.max(level_dist)
}

/// Indices where `vals[i]` is a local maximum ≥ `thresh`, deduped by
/// `min_gap` (strongest wins inside the gap).
fn threshold_peaks(vals: &[f64], thresh: f64, min_gap: usize) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for i in 0..vals.len() {
        let v = vals[i];
        if v < thresh {
            continue;
        }
        let left = i == 0 || vals[i - 1] <= v;
        let right = i + 1 >= vals.len() || vals[i + 1] < v;
        if !(left && right) {
            continue;
        }
        match out.last() {
            Some(&l) if i - l < min_gap => {
                if v > vals[l] {
                    *out.last_mut().unwrap() = i;
                }
            }
            _ => out.push(i),
        }
    }
    out
}

/// argmax of `f` over `lo..=hi` (clamped to `n`); ties go to the index
/// nearest `center`, then the lowest — deterministic.
fn argmax_near(lo: usize, hi: usize, n: usize, center: usize, f: impl Fn(usize) -> f64) -> usize {
    let hi = hi.min(n.saturating_sub(1));
    let lo = lo.min(hi);
    let mut best = center.clamp(lo, hi);
    for i in lo..=hi {
        if f(i) > f(best) || (f(i) == f(best) && i.abs_diff(center) < best.abs_diff(center)) {
            best = i;
        }
    }
    best
}

/// The fused candidate list. `emb` is the optional embedding sidecar
/// (parallel to `p.frames`); without it, dHash is the signature space.
pub fn pick_peaks(
    p: &Perceive,
    emb: Option<&[Vec<f32>]>,
    brief: &Brief,
    words: &[Word],
) -> Vec<Candidate> {
    let n = p.frames.len();
    if n == 0 {
        return Vec::new();
    }
    let gap = brief.min_gap_frames as usize;
    let snap = brief.snap_radius_frames as usize;

    // importance[i] — the spec's weighted product; onsets add their
    // normalized strength on top (the coincidence bonus). Flux
    // normalizes by max, not p95 — sparse curves have p95 = 0.
    let flux_max = p.audio.iter().map(|h| h.flux).fold(0.0, f64::max);
    let onset_norm = |i: usize| -> f64 {
        if flux_max <= 0.0 {
            0.0
        } else {
            (p.audio[i].onset / flux_max).clamp(0.0, 1.0)
        }
    };
    let base_imp = |i: usize| -> f64 {
        let f = &p.frames[i];
        f.change * (0.5 * f.motion + 0.3 * f.sharpness + 0.2 * f.contrast)
    };
    let imp: Vec<f64> = (0..n)
        .map(|i| base_imp(i) + brief.onset_weight * onset_norm(i))
        .collect();

    // Four generators, co-equal: signature-change spikes, pixel-diff
    // (motion) spikes — a hard cut between flat shots moves every pixel
    // while leaving the dHash untouched — audio onsets, and adaptive
    // local maxima of importance.
    let change: Vec<f64> = p.frames.iter().map(|f| f.change).collect();
    let motion: Vec<f64> = p.frames.iter().map(|f| f.motion).collect();
    let mut raw: Vec<(usize, PeakSource)> = threshold_peaks(&change, brief.change_keep, gap)
        .into_iter()
        .map(|i| (i, PeakSource::Visual))
        .collect();
    raw.extend(
        detect_spikes(&motion, 6.0, 0.35, gap)
            .into_iter()
            .map(|i| (i, PeakSource::Visual)),
    );
    raw.extend(
        (0..n)
            .filter(|&i| p.audio[i].onset > 0.0)
            .map(|i| (i, PeakSource::Audio)),
    );
    raw.extend(
        detect_spikes(&imp, 3.0, 0.0, gap)
            .into_iter()
            .map(|i| (i, PeakSource::Importance)),
    );
    raw.sort_by_key(|&(i, _)| i);

    // Merge raw hits closer than min_gap into one beat — a visual spike
    // and the onset it landed on are the same moment, not two.
    let mut merged: Vec<(Vec<usize>, Vec<PeakSource>)> = Vec::new();
    for (i, src) in raw {
        match merged.last_mut() {
            Some((idxs, ss)) if i - idxs[0] < gap => {
                idxs.push(i);
                ss.push(src);
            }
            _ => merged.push((vec![i], vec![src])),
        }
    }

    // Clip-wide texture level: when the p95 of raw sharpness is ~0 the
    // clip is flat (title-card memes) and sharpness carries no signal.
    let sharp_p95 = {
        let mut v: Vec<f64> = p.frames.iter().map(|f| f.sharp_raw).collect();
        v.sort_by(f64::total_cmp);
        v[(v.len() * 95 / 100).min(v.len() - 1)]
    };

    let mut keeps: Vec<Candidate> = Vec::new();
    for (idxs, srcs) in merged {
        // Beat position = strongest-importance index in the group;
        // deterministic on ties (lowest index).
        let base = idxs
            .iter()
            .copied()
            .reduce(|a, b| if imp[b] > imp[a] { b } else { a })
            .unwrap();
        // `cut_on_beat` needs a real visual spike — an importance max
        // caused BY the onset must not masquerade as a visual cut.
        let has_visual = srcs.contains(&PeakSource::Visual);
        let has_audio = srcs.contains(&PeakSource::Audio);
        let cut_on_beat = has_visual && has_audio;

        // Representative frame: sharpest in the window. Visual beats
        // look forward of the cut (the boundary frame is often smear);
        // audio beats look ±snap so the still stays on the beat.
        let rep = if has_visual {
            argmax_near(base, base + gap, n, base, |i| p.frames[i].sharpness)
        } else {
            argmax_near(base.saturating_sub(snap), base + snap, n, base, |i| {
                p.frames[i].sharpness
            })
        };
        let f = &p.frames[rep];

        // Gates run on the representative frame — if even the sharpest
        // post-cut frame is mush, the whole beat is mush. On a clip
        // that's textureless throughout (flat-color meme format), every
        // frame scores ~0 and sharpness can't tell smear from content —
        // the gate would drop the entire clip, so it stands down.
        if sharp_p95 > 0.02 && f.sharpness < brief.min_sharpness {
            continue;
        }
        if f.brightness < brief.brightness_min || f.brightness > brief.brightness_max {
            continue;
        }

        // Beat time: onset-snap first (cut on the beat), then word
        // boundary — both within ±snap_radius_frames.
        let mut t = base as f64 / p.fps;
        let snap_s = snap as f64 / p.fps;
        let onset_t = (base.saturating_sub(snap)..=(base + snap).min(n - 1))
            .filter(|&i| p.audio[i].onset > 0.0)
            .map(|i| (i as f64 / p.fps, i))
            .min_by(|a, b| (a.0 - t).abs().total_cmp(&(b.0 - t).abs()));
        let mut onset_strength = 0.0;
        if let Some((ot, oi)) = onset_t {
            t = ot;
            onset_strength = onset_norm(oi);
        } else if let Some(w) = words
            .iter()
            .filter(|w| (w.start_s - t).abs() <= snap_s)
            .min_by(|a, b| (a.start_s - t).abs().total_cmp(&(b.start_s - t).abs()))
        {
            t = w.start_s;
        }
        let word = word_at(words, t).map(|w| w.text.clone());

        // Dedup against keeps so far: the strongest similarity to an
        // already-kept frame is recorded and gated.
        let sim_to_kept = keeps
            .iter()
            .map(|k| similarity(p, emb, rep as u64, k.frame))
            .fold(0.0, f64::max);
        if sim_to_kept > brief.dup_cosine {
            continue;
        }

        let importance = imp[rep]
            + if cut_on_beat {
                brief.onset_weight * onset_strength
            } else {
                0.0
            };
        let source = match (has_visual, has_audio) {
            (true, true) => PeakSource::Fused,
            (true, false) => PeakSource::Visual,
            (false, true) => PeakSource::Audio,
            (false, false) => PeakSource::Importance,
        };
        keeps.push(Candidate {
            id: format!("f{rep}"),
            frame: rep as u64,
            t,
            // `change` is the beat's strength — the spike at `base`, not
            // the rep still's own (post-cut) inter-frame delta.
            change: p.frames[base].change,
            sharpness: f.sharpness,
            motion: f.motion,
            brightness: f.brightness,
            contrast: f.contrast,
            importance,
            same_shot: p.frames[base].change < brief.change_skip,
            cut_on_beat,
            onset: onset_strength,
            word,
            source,
            sim_to_kept,
            tags: Vec::new(),
        });
    }
    keeps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{AudioHop, FrameMetrics};

    fn frame_m(frame: u64, change: f64, sharpness: f64) -> FrameMetrics {
        FrameMetrics {
            frame,
            t: frame as f64 / 30.0,
            sharpness,
            sharp_raw: sharpness,
            motion: 0.5,
            brightness: 0.5,
            contrast: 0.5,
            change,
            dhash: frame.wrapping_mul(0x9e3779b97f4a7c15), // distinct per frame
            tags: Vec::new(),
        }
    }

    fn hop(frame: u64, loud_db: f64, flux: f64) -> AudioHop {
        AudioHop {
            frame,
            t: frame as f64 / 30.0,
            loud_db,
            flux,
            onset: 0.0,
            silence: false,
            word: None,
            speech: false,
        }
    }

    fn perceive(frames: Vec<FrameMetrics>, mut audio: Vec<AudioHop>) -> Perceive {
        crate::metrics::detect_onsets(&mut audio, 6.0, -50.0);
        Perceive {
            fps: 30.0,
            duration_s: frames.len() as f64 / 30.0,
            has_audio: true,
            encoder: "dhash".into(),
            frames,
            audio,
        }
    }

    fn silent_hops(n: usize) -> Vec<AudioHop> {
        (0..n).map(|i| hop(i as u64, -20.0, 0.0)).collect()
    }

    #[test]
    fn static_shot_yields_no_keeps() {
        let p = perceive(
            (0..90).map(|i| frame_m(i, 0.01, 0.8)).collect(),
            silent_hops(90),
        );
        assert!(pick_peaks(&p, None, &Brief::default(), &[]).is_empty());
    }

    #[test]
    fn hard_cuts_land_on_cuts_not_mush() {
        // Cuts every ~200 ms (6 frames); the boundary frame is blurry,
        // the post-cut frame sharp — the keep snaps forward.
        let mut frames: Vec<_> = (0..90).map(|i| frame_m(i, 0.01, 0.9)).collect();
        for &cut in &[12usize, 36, 60] {
            frames[cut].change = 0.8;
            frames[cut].sharpness = 0.1; // transition smear
            frames[cut + 1].sharpness = 0.95; // sharpest post-cut
        }
        let p = perceive(frames, silent_hops(90));
        let keeps = pick_peaks(&p, None, &Brief::default(), &[]);
        assert_eq!(keeps.len(), 3, "{keeps:?}");
        for (k, cut) in keeps.iter().zip([12usize, 36, 60]) {
            assert_eq!(k.frame, (cut + 1) as u64, "snap to sharpest post-cut");
            assert_eq!(k.t, cut as f64 / 30.0, "beat time stays on the cut");
            assert_eq!(k.source, PeakSource::Visual);
        }
    }

    #[test]
    fn near_identical_punches_dedup_to_one() {
        // Repeated punch frames with the same signature — spaced beyond
        // min_gap so they're separate candidates, collapsed by dedup.
        let mut frames: Vec<_> = (0..90).map(|i| frame_m(i, 0.01, 0.9)).collect();
        for &i in &[20usize, 30, 40] {
            frames[i].change = 0.8;
            frames[i].dhash = 0xdeadbeef; // identical signatures
        }
        let p = perceive(frames, silent_hops(90));
        let keeps = pick_peaks(&p, None, &Brief::default(), &[]);
        assert_eq!(keeps.len(), 1, "near-dupes collapse, {keeps:?}");
        assert_eq!(keeps[0].sim_to_kept, 0.0);
    }

    #[test]
    fn audio_only_onset_is_a_candidate() {
        let mut audio = silent_hops(90);
        audio[45].flux = 50.0; // sfx hit, no visual change
        audio[45].loud_db = -15.0;
        let p = perceive((0..90).map(|i| frame_m(i, 0.01, 0.8)).collect(), audio);
        let keeps = pick_peaks(&p, None, &Brief::default(), &[]);
        assert_eq!(keeps.len(), 1);
        assert_eq!(keeps[0].source, PeakSource::Audio);
        assert!(keeps[0].same_shot, "beat on a static shot");
        assert!((keeps[0].t - 1.5).abs() < 1e-9, "t snaps onto the onset");
    }

    #[test]
    fn cut_on_beat_fuses_and_boosts() {
        let mut frames: Vec<_> = (0..90).map(|i| frame_m(i, 0.01, 0.9)).collect();
        frames[30].change = 0.8;
        let mut audio = silent_hops(90);
        audio[31].flux = 60.0;
        audio[31].loud_db = -15.0;
        let p = perceive(frames, audio);
        let keeps = pick_peaks(&p, None, &Brief::default(), &[]);
        assert_eq!(keeps.len(), 1);
        let k = &keeps[0];
        assert!(k.cut_on_beat);
        assert_eq!(k.source, PeakSource::Fused);
        assert!((k.t - 31.0 / 30.0).abs() < 1e-9, "t snaps to the onset");
        assert!(k.importance > 0.0);
        assert!(k.onset > 0.0);
    }

    #[test]
    fn silence_never_produces_onsets() {
        let mut audio = silent_hops(90);
        for h in audio.iter_mut() {
            h.loud_db = -70.0;
        }
        audio[40].flux = 80.0; // flux spike in dead air — noise
        let p = perceive((0..90).map(|i| frame_m(i, 0.01, 0.8)).collect(), audio);
        assert!(p.audio[40].silence);
        assert_eq!(p.audio[40].onset, 0.0);
        assert!(pick_peaks(&p, None, &Brief::default(), &[]).is_empty());
    }

    #[test]
    fn blurry_and_bright_extremes_drop() {
        let mut frames: Vec<_> = (0..90).map(|i| frame_m(i, 0.01, 0.9)).collect();
        frames[30].change = 0.9;
        for f in frames.iter_mut().take(34).skip(30) {
            f.sharpness = 0.05; // whole window is mush
        }
        frames[60].change = 0.9;
        frames[60].brightness = 0.99; // near-white flash
        let p = perceive(frames, silent_hops(90));
        assert!(pick_peaks(&p, None, &Brief::default(), &[]).is_empty());
    }

    #[test]
    fn word_boundary_snaps_the_beat() {
        use scene_time::Word;
        let mut frames: Vec<_> = (0..90).map(|i| frame_m(i, 0.01, 0.9)).collect();
        frames[30].change = 0.8; // cut at t=1.0, word starts at 1.033
        let p = perceive(frames, silent_hops(90));
        let words = vec![Word {
            text: "boom".into(),
            start_s: 31.0 / 30.0,
            end_s: 40.0 / 30.0,
        }];
        let keeps = pick_peaks(&p, None, &Brief::default(), &words);
        assert_eq!(keeps.len(), 1);
        assert!(
            (keeps[0].t - 31.0 / 30.0).abs() < 1e-9,
            "snapped to word start"
        );
        assert_eq!(keeps[0].word.as_deref(), Some("boom"));
    }

    #[test]
    fn min_gap_allows_close_beats() {
        // 2-frame-apart cuts are real flash beats — min_gap=2 keeps both.
        let mut frames: Vec<_> = (0..90).map(|i| frame_m(i, 0.01, 0.9)).collect();
        frames[30].change = 0.8;
        frames[32].change = 0.9;
        let p = perceive(frames, silent_hops(90));
        let keeps = pick_peaks(&p, None, &Brief::default(), &[]);
        assert_eq!(keeps.len(), 2);
    }

    #[test]
    fn deterministic_across_calls() {
        let mut frames: Vec<_> = (0..90).map(|i| frame_m(i, 0.01, 0.9)).collect();
        frames[20].change = 0.8;
        frames[50].change = 0.7;
        let mut audio = silent_hops(90);
        audio[70].flux = 40.0;
        audio[70].loud_db = -15.0;
        let p = perceive(frames, audio);
        let a = pick_peaks(&p, None, &Brief::default(), &[]);
        let b = pick_peaks(&p, None, &Brief::default(), &[]);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!((x.frame, x.t, x.source), (y.frame, y.t, y.source));
        }
    }
}
