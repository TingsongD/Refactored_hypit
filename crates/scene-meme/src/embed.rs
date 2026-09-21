//! The optional embedding capability. `encoder = "dhash"` needs nothing
//! external — the pipeline is complete offline. Any other name resolves
//! as a capability whose connector decodes the clip at the analysis
//! grid and returns one embedding per frame plus vocab embeddings for
//! candidate tagging. Embeddings never enter a Jev request — they stay
//! local, in this file and the `emb-*.json` cache entry.

use std::path::Path;

use scene_cap::{CapRequest, Registry, fulfill};
use serde::{Deserialize, Serialize};

use crate::brief::Brief;
use crate::error::MemeError;
use crate::peaks::Candidate;
use crate::perceive::Perceive;

/// What the `embed` connector returns — one row per analysis frame,
/// plus one text embedding per `vocab` label for local zero-shot tags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedOut {
    /// `embeddings[i]` is frame i's vector — same index space as
    /// `Perceive.frames`.
    pub embeddings: Vec<Vec<f32>>,
    /// `vocab_embeddings[j]` aligns with `brief.tag_vocab[j]`.
    #[serde(default)]
    pub vocab_embeddings: Vec<Vec<f32>>,
}

impl EmbedOut {
    pub fn validate(&self, frames: usize, labels: usize) -> Result<(), MemeError> {
        let dimension = self.embeddings.first().map(Vec::len).unwrap_or(0);
        if self.embeddings.len() != frames
            || self.vocab_embeddings.len() != labels
            || dimension == 0
            || self
                .embeddings
                .iter()
                .chain(&self.vocab_embeddings)
                .any(|row| {
                    row.len() != dimension
                        || row.iter().any(|x| !x.is_finite())
                        || row.iter().all(|x| *x == 0.0)
                })
        {
            return Err(MemeError::Stage(
                "invalid embedding frame/vocabulary dimensions or values".into(),
            ));
        }
        Ok(())
    }
}

/// Cosine similarity −1..1 — normalized so both unit-scales of encoder
/// output behave identically.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (&u, &v) in a.iter().zip(b.iter()) {
        dot += (u * v) as f64;
        na += (u * u) as f64;
        nb += (v * v) as f64;
    }
    let denom = (na * nb).sqrt();
    if denom <= 0.0 {
        0.0
    } else {
        (dot / denom).clamp(-1.0, 1.0)
    }
}

/// Call the `embed` capability. `Ok(None)` when `brief.encoder` is
/// `"dhash"` — the offline signature. Any other name must be a real
/// capability in the registry; a typo fails loudly here rather than
/// silently degrading to dHash semantics.
pub fn run_embed(
    reg: Option<&Registry>,
    video: &Path,
    brief: &Brief,
    frames: usize,
    out: &Path,
) -> Result<Option<EmbedOut>, MemeError> {
    if brief.encoder == "dhash" {
        return Ok(None);
    }
    let reg = reg.ok_or_else(|| {
        MemeError::Stage(format!(
            "encoder `{}` needs a scene.toml capability; no config supplied",
            brief.encoder
        ))
    })?;
    // The request document carries only scalars and a path — no pixels,
    // no frames. The connector decodes the video itself.
    let req = CapRequest {
        capability: &brief.encoder,
        params: serde_json::json!({
            "video": video.display().to_string(),
            "size": brief.perceive_size,
            "fps": brief.fps,
            "vocab": brief.tag_vocab,
            "model": brief.embedding_model,
            "weights_id": brief.weights_id,
        }),
        out,
    };
    let produced = fulfill(reg, &req)?;
    let text = std::fs::read_to_string(&produced).map_err(MemeError::io(&produced))?;
    let parsed: EmbedOut = serde_json::from_str(&text)
        .map_err(|e| MemeError::Stage(format!("embed response is not valid JSON: {e}")))?;
    parsed.validate(frames, brief.tag_vocab.len())?;
    Ok(Some(parsed))
}

/// Rewrite `change` in embedding space: `1 - cosine(prev, cur)`. This
/// replaces the dHash-hamming `change` from perceive — the peaks key
/// carries the encoder name so the two spaces never share a cache.
pub fn apply_embeddings(p: &mut Perceive, emb: &[Vec<f32>]) {
    for i in 1..p.frames.len().min(emb.len()) {
        p.frames[i].change = (1.0 - cosine(&emb[i - 1], &emb[i])).clamp(0.0, 1.0);
    }
}

/// Tag candidates against the vocab — top-`k` labels by cosine, above
/// `min_sim`. Runs only on kept candidates (never every frame): a fact
/// sheet is the only consumer of tags.
pub fn tag_candidates(
    candidates: &mut [Candidate],
    emb: &[Vec<f32>],
    vocab_emb: &[Vec<f32>],
    vocab: &[String],
    top_k: usize,
    min_sim: f64,
) {
    for cand in candidates.iter_mut() {
        let Some(fe) = emb.get(cand.frame as usize) else {
            continue;
        };
        let mut scored: Vec<(f64, usize)> = vocab_emb
            .iter()
            .enumerate()
            .map(|(j, ve)| (cosine(fe, ve), j))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        cand.tags = scored
            .into_iter()
            .take(top_k)
            .filter(|(sim, _)| *sim >= min_sim)
            .filter_map(|(_, j)| vocab.get(j).cloned())
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-9);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-9);
        // Scale-invariant.
        assert!((cosine(&[2.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        // Zero vector doesn't divide-by-zero panic.
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    fn fake_perceive(changes: usize) -> Perceive {
        let frames = (0..changes)
            .map(|i| crate::metrics::FrameMetrics {
                frame: i as u64,
                t: i as f64 / 30.0,
                sharpness: 0.5,
                sharp_raw: 0.5,
                motion: 0.0,
                brightness: 0.5,
                contrast: 0.5,
                change: 0.01,
                dhash: 0,
                tags: Vec::new(),
            })
            .collect();
        Perceive {
            fps: 30.0,
            duration_s: changes as f64 / 30.0,
            has_audio: false,
            encoder: "dhash".into(),
            frames,
            audio: Vec::new(),
        }
    }

    #[test]
    fn apply_embeddings_rewrites_change_as_cosine() {
        let mut p = fake_perceive(3);
        let emb = vec![
            vec![1.0, 0.0],
            vec![1.0, 0.0], // identical → change 0
            vec![0.0, 1.0], // orthogonal → change 1
        ];
        apply_embeddings(&mut p, &emb);
        assert_eq!(p.frames[0].change, 0.01, "frame 0 has no predecessor");
        assert!(p.frames[1].change < 1e-9);
        assert!((p.frames[2].change - 1.0).abs() < 1e-9);
    }

    #[test]
    fn tag_candidates_picks_nearest_labels() {
        let mut cands = vec![Candidate {
            id: "f0".into(),
            frame: 0,
            t: 0.0,
            representative_t: 0.0,
            change: 0.5,
            sharpness: 0.9,
            motion: 0.5,
            brightness: 0.5,
            contrast: 0.5,
            importance: 0.5,
            same_shot: false,
            cut_on_beat: false,
            onset: 0.0,
            word: None,
            source: crate::peaks::PeakSource::Visual,
            sim_to_kept: 0.0,
            tags: Vec::new(),
        }];
        let emb = vec![vec![1.0, 0.0]];
        let vocab = vec!["face".to_string(), "wide".to_string()];
        let vocab_emb = vec![vec![0.99, 0.01], vec![0.0, 1.0]];
        tag_candidates(&mut cands, &emb, &vocab_emb, &vocab, 4, 0.2);
        assert_eq!(cands[0].tags, vec!["face"]);
    }

    #[test]
    fn dhash_needs_no_connector() {
        // Offline mode short-circuits before any capability lookup.
        let out = run_embed(
            None,
            Path::new("x.mp4"),
            &Brief::default(),
            10,
            Path::new("o.json"),
        );
        assert!(matches!(out, Ok(None)));
    }

    #[test]
    fn named_encoder_without_registry_fails_loud() {
        let brief = Brief {
            encoder: "mobileclip2-s0".into(),
            ..Default::default()
        };
        let err = run_embed(None, Path::new("x.mp4"), &brief, 10, Path::new("o.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("mobileclip2-s0"), "{err}");
        assert!(err.contains("capability"), "{err}");
    }
}
