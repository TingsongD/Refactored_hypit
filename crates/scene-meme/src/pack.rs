//! Score pack — candidates become readable fact rows. This is the
//! whole Jev boundary: rows carry numbers and labels Jev can reason
//! over, and nothing else. No embedding vectors, no base64 pixels, no
//! video. If a field can't be stated as a fact, it doesn't ship.

use serde::Serialize;
use serde_json::{Value, json};

use crate::brief::Brief;
use crate::peaks::{Candidate, PeakSource};
use crate::perceive::Perceive;

/// One fact-sheet row — the per-candidate record Jev routes on.
#[derive(Debug, Clone, Serialize)]
pub struct FactRow {
    pub id: String,
    pub t: f64,
    pub frame: u64,
    pub change: f64,
    pub sharpness: f64,
    pub motion: f64,
    pub brightness: f64,
    pub contrast: f64,
    pub importance: f64,
    /// Zero-shot tags (embed mode only; empty offline).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Most similar other candidate — computed after the full pass,
    /// so the answer doesn't depend on candidate order.
    pub nearest_kept: Option<String>,
    pub nearest_kept_sim: f64,
    /// change < change_skip — the beat fired on a static shot.
    pub same_shot: bool,
    // ---- audio columns -------------------------------------------------
    pub loud_db: f64,
    /// Normalized onset strength at the beat (0 = none).
    pub onset: f64,
    /// Inside an aligned word at `t`.
    pub speech: bool,
    /// Visual spike ∧ onset within the snap radius.
    pub cut_on_beat: bool,
    /// The aligned word covering `t`, when timings exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub word: Option<String>,
    /// What fired this candidate.
    pub source: PeakSource,
}

/// Signature similarity between two candidate frames — cosine under an
/// encoder, dHash hamming/64 offline. Local to this call.
fn sim(p: &Perceive, emb: Option<&[Vec<f32>]>, a: &Candidate, b: &Candidate) -> f64 {
    crate::peaks::similarity(p, emb, a.frame, b.frame)
}

/// Build the fact sheet. Caps at `brief.max_candidates_to_jev` by
/// importance, then re-sorts the selection by time — Jev reads a
/// timeline, not a leaderboard.
pub fn score_pack(
    candidates: &[Candidate],
    p: &Perceive,
    emb: Option<&[Vec<f32>]>,
    brief: &Brief,
) -> Value {
    // Top-N by importance; ties break on frame index (deterministic).
    let mut picked: Vec<&Candidate> = candidates.iter().collect();
    picked.sort_by(|a, b| {
        b.importance
            .total_cmp(&a.importance)
            .then(a.frame.cmp(&b.frame))
    });
    picked.truncate(brief.max_candidates_to_jev);
    picked.sort_by_key(|c| c.frame);

    // nearest_kept runs against the FINAL set — post-pass, order-free.
    let rows: Vec<FactRow> = picked
        .iter()
        .map(|c| {
            let (nearest, nearest_sim) = picked
                .iter()
                .filter(|o| o.id != c.id)
                .map(|o| (o.id.clone(), sim(p, emb, c, o)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(id, s)| (Some(id), s))
                .unwrap_or((None, 0.0));
            let hop = p.audio.get((c.t * p.fps).round() as usize);
            FactRow {
                id: c.id.clone(),
                t: c.t,
                frame: c.frame,
                change: c.change,
                sharpness: c.sharpness,
                motion: c.motion,
                brightness: c.brightness,
                contrast: c.contrast,
                importance: c.importance,
                tags: c.tags.clone(),
                nearest_kept: nearest,
                nearest_kept_sim: nearest_sim,
                same_shot: c.same_shot,
                loud_db: hop.map(|h| h.loud_db).unwrap_or(-100.0),
                onset: c.onset,
                speech: hop.is_some_and(|h| h.speech),
                cut_on_beat: c.cut_on_beat,
                word: c.word.clone(),
                source: c.source,
            }
        })
        .collect();

    json!({
        "brief": brief.description,
        "candidates": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{AudioHop, FrameMetrics};

    fn cand(i: u64, importance: f64) -> Candidate {
        Candidate {
            id: format!("f{i}"),
            frame: i,
            t: i as f64 / 30.0,
            representative_t: i as f64 / 30.0,
            change: 0.5,
            sharpness: 0.9,
            motion: 0.5,
            brightness: 0.5,
            contrast: 0.5,
            importance,
            same_shot: false,
            cut_on_beat: false,
            onset: 0.0,
            word: None,
            source: PeakSource::Visual,
            sim_to_kept: 0.0,
            tags: Vec::new(),
        }
    }

    fn perceive(n: usize, distinct: bool) -> Perceive {
        let frames = (0..n)
            .map(|i| FrameMetrics {
                frame: i as u64,
                t: i as f64 / 30.0,
                sharpness: 0.9,
                sharp_raw: 0.9,
                motion: 0.5,
                brightness: 0.5,
                contrast: 0.5,
                change: 0.5,
                dhash: if distinct { i as u64 * 0x9e3779b9 } else { 42 },
                tags: Vec::new(),
            })
            .collect();
        let audio = (0..n)
            .map(|i| AudioHop {
                frame: i as u64,
                t: i as f64 / 30.0,
                loud_db: -20.0,
                flux: 0.0,
                onset: 0.0,
                silence: false,
                word: None,
                speech: false,
            })
            .collect();
        Perceive {
            fps: 30.0,
            duration_s: n as f64 / 30.0,
            has_audio: true,
            encoder: "dhash".into(),
            frames,
            audio,
        }
    }

    #[test]
    fn pack_shape_has_no_pixels_or_vectors() {
        let p = perceive(5, true);
        let cands = vec![cand(1, 0.9), cand(3, 0.5)];
        let pack = score_pack(&cands, &p, None, &Brief::default());
        let text = pack.to_string();
        // The hard boundary: no embedding arrays, no base64, no hex blobs.
        assert!(!text.contains("embedding"));
        assert!(!text.contains("dhash"), "raw hashes are not facts");
        assert!(pack["candidates"].is_array());
        assert_eq!(pack["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(pack["candidates"][0]["id"], "f1");
    }

    #[test]
    fn cap_keeps_top_importance_then_time_orders() {
        let p = perceive(10, true);
        let cands: Vec<Candidate> = (0..10).map(|i| cand(i, i as f64)).collect();
        let brief = Brief {
            max_candidates_to_jev: 3,
            ..Default::default()
        };
        let pack = score_pack(&cands, &p, None, &brief);
        let rows = pack["candidates"].as_array().unwrap();
        // Top-3 by importance: frames 7,8,9 — emitted in time order.
        let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["f7", "f8", "f9"]);
    }

    #[test]
    fn nearest_kept_is_order_independent() {
        let p = perceive(4, false); // all dhash identical
        let cands = vec![cand(0, 0.9), cand(3, 0.5)];
        let pack = score_pack(&cands, &p, None, &Brief::default());
        let rows = pack["candidates"].as_array().unwrap();
        // Identical hashes → each row's nearest is the other, sim 1.0.
        assert_eq!(rows[0]["nearest_kept"], "f3");
        assert_eq!(rows[1]["nearest_kept"], "f0");
        assert_eq!(rows[0]["nearest_kept_sim"], 1.0);
    }

    #[test]
    fn audio_columns_ride_the_rows() {
        let mut p = perceive(4, true);
        p.audio[2].loud_db = -8.0;
        p.audio[2].speech = true;
        let mut c = cand(2, 0.9);
        c.cut_on_beat = true;
        c.onset = 0.7;
        c.word = Some("boom".into());
        let pack = score_pack(&[c], &p, None, &Brief::default());
        let row = &pack["candidates"][0];
        assert_eq!(row["loud_db"], -8.0);
        assert_eq!(row["speech"], true);
        assert_eq!(row["cut_on_beat"], true);
        assert_eq!(row["word"], "boom");
        assert_eq!(row["onset"], 0.7);
    }
}
