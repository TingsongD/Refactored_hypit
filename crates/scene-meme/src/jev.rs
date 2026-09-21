//! Jev stages — routing and packaging. Jev sees the locked brief and
//! readable fact sheets only: no embedding vectors, no pixels, no
//! video. Code asks the questions; Jev answers them; code applies the
//! keep-rule. Anything numeric the rule needs (cut_strength,
//! too_similar, confidence) comes back typed, never as prose.

use std::collections::BTreeMap;
use std::path::Path;

use scene_cap::{CapRequest, Registry, fulfill};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::brief::Brief;
use crate::error::MemeError;
use crate::peaks::Candidate;

/// The routing judgement for one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    Keep,
    Skip,
    Maybe,
}

/// One candidate's answers, typed — Jev's reply parses into this or
/// the call fails (a malformed answer is a connector bug, not data).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RouteAnswer {
    pub route: Route,
    /// 1..5 — idle .. peak meme beat.
    pub cut_strength: u8,
    /// Jev's judgement that this beat duplicates `nearest_kept`, 0..1.
    pub too_similar: f64,
    /// The connector's self-reported confidence, 0..1.
    #[serde(default = "one")]
    pub confidence: f64,
}

fn one() -> f64 {
    1.0
}

/// What routing produced: the keeps Jev approved, the maybes held in
/// reserve, and the ids whose confidence fell under the brief's floor —
/// those surface to a human instead of auto-exporting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteOutcome {
    pub keeps: Vec<String>,
    pub maybes: Vec<String>,
    pub skips: Vec<String>,
    /// Confidence below `jev_min_confidence` — the runner reports these
    /// and does not auto-export.
    pub low_confidence: Vec<String>,
    pub answers: BTreeMap<String, RouteAnswer>,
}

/// The Jev route request document. `params` are self-describing — the
/// connector script translates them into the vendor's wire shape; the
/// engine never builds vendor JSON.
pub fn route_doc(pack: &Value, brief: &Brief) -> Value {
    serde_json::json!({
        "task": "jev_route",
        "model": brief.jev_model,
        "brief": {
            "job": brief.job,
            "description": brief.description,
            "fps": brief.fps,
        },
        "state": pack,
        "questions": {
            "route": ["keep", "skip", "maybe"],
            "cut_strength": "1..5 (idle .. peak meme beat)",
            "too_similar": "0..1 — same beat as nearest_kept",
        },
        "instructions": "Answer every candidate id. Do not invent frames, \
                         do not ask for pixels — reason over the rows only.",
    })
}

/// Parse the connector's response: `{"answers": {id: {route, ...}}}`.
/// Missing candidates get `Skip` — an unanswered question is not a keep.
pub fn parse_answers(text: &str, cands: &[Candidate]) -> Result<RouteOutcome, MemeError> {
    #[derive(Deserialize)]
    struct Reply {
        #[serde(default)]
        answers: BTreeMap<String, RouteAnswer>,
    }
    let reply: Reply = serde_json::from_str(text)
        .map_err(|e| MemeError::Stage(format!("jev route response is not valid JSON: {e}")))?;
    if reply.answers.values().any(|a| {
        !(1..=5).contains(&a.cut_strength)
            || !(0.0..=1.0).contains(&a.too_similar)
            || !(0.0..=1.0).contains(&a.confidence)
    }) {
        return Err(MemeError::Stage(
            "jev routing values are outside their ranges".into(),
        ));
    }
    Ok(apply_keep_rule(&reply.answers, cands))
}

/// The keep rule from the brief: `keep` ∧ `too_similar < 0.5` ∧
/// `cut_strength ≥ 3`; `maybe` ∧ `cut_strength ≥ 3` is held in reserve;
/// everything else skips. Code applies it — Jev only answers.
pub fn apply_keep_rule(
    answers: &BTreeMap<String, RouteAnswer>,
    cands: &[Candidate],
) -> RouteOutcome {
    let mut out = RouteOutcome::default();
    for c in cands {
        match answers.get(&c.id).copied() {
            Some(a) => {
                out.answers.insert(c.id.clone(), a);
                match a.route {
                    Route::Keep if a.too_similar < 0.5 && a.cut_strength >= 3 => {
                        out.keeps.push(c.id.clone())
                    }
                    Route::Maybe if a.cut_strength >= 3 => out.maybes.push(c.id.clone()),
                    _ => out.skips.push(c.id.clone()),
                }
            }
            None => out.skips.push(c.id.clone()),
        }
    }
    out
}

/// Flag ids under the brief's confidence floor — called after
/// `apply_keep_rule` so the floor lives in one place.
pub fn flag_low_confidence(out: &mut RouteOutcome, brief: &Brief) {
    out.low_confidence = out
        .answers
        .iter()
        .filter(|(_, a)| a.confidence < brief.jev_min_confidence)
        .map(|(id, _)| id.clone())
        .collect();
    out.low_confidence.sort();
}

/// Run the route stage against a registered `jev` capability.
/// `pack_json` is the fact-sheet Value; the response file lands at `out`.
pub fn route(
    reg: &Registry,
    capability: &str,
    pack: &Value,
    brief: &Brief,
    cands: &[Candidate],
    out: &Path,
) -> Result<RouteOutcome, MemeError> {
    let req = CapRequest {
        capability,
        params: route_doc(pack, brief),
        out,
    };
    let produced = fulfill(reg, &req)?;
    let text = std::fs::read_to_string(&produced).map_err(MemeError::io(&produced))?;
    let mut outcome = parse_answers(&text, cands)?;
    flag_low_confidence(&mut outcome, brief);
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peaks::PeakSource;

    fn cand(i: u64) -> Candidate {
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
            importance: 0.5,
            same_shot: false,
            cut_on_beat: false,
            onset: 0.0,
            word: None,
            source: PeakSource::Visual,
            sim_to_kept: 0.0,
            tags: Vec::new(),
        }
    }

    fn answer(route: Route, cut_strength: u8, too_similar: f64) -> RouteAnswer {
        RouteAnswer {
            route,
            cut_strength,
            too_similar,
            confidence: 0.9,
        }
    }

    #[test]
    fn keep_rule_table() {
        let cands = vec![cand(0), cand(1), cand(2), cand(3), cand(4), cand(5)];
        let mut answers = BTreeMap::new();
        answers.insert("f0".into(), answer(Route::Keep, 4, 0.2)); // keep
        answers.insert("f1".into(), answer(Route::Keep, 4, 0.9)); // dup → skip
        answers.insert("f2".into(), answer(Route::Keep, 2, 0.1)); // weak → skip
        answers.insert("f3".into(), answer(Route::Maybe, 4, 0.2)); // maybe pool
        answers.insert("f4".into(), answer(Route::Maybe, 2, 0.2)); // weak maybe → skip
        answers.insert("f5".into(), answer(Route::Skip, 5, 0.0)); // explicit skip
        let out = apply_keep_rule(&answers, &cands);
        assert_eq!(out.keeps, vec!["f0"]);
        assert_eq!(out.maybes, vec!["f3"]);
        assert_eq!(out.skips, vec!["f1", "f2", "f4", "f5"]);
    }

    #[test]
    fn unanswered_candidates_skip() {
        let cands = vec![cand(0), cand(1)];
        let out = apply_keep_rule(&BTreeMap::new(), &cands);
        assert!(out.keeps.is_empty());
        assert_eq!(out.skips.len(), 2);
    }

    #[test]
    fn low_confidence_flags_below_floor() {
        let cands = vec![cand(0), cand(1)];
        let mut answers = BTreeMap::new();
        answers.insert(
            "f0".into(),
            RouteAnswer {
                confidence: 0.9,
                ..answer(Route::Keep, 5, 0.0)
            },
        );
        answers.insert(
            "f1".into(),
            RouteAnswer {
                confidence: 0.3,
                ..answer(Route::Keep, 5, 0.0)
            },
        );
        let mut out = apply_keep_rule(&answers, &cands);
        flag_low_confidence(&mut out, &Brief::default());
        assert_eq!(out.low_confidence, vec!["f1"]);
    }

    #[test]
    fn parse_roundtrip_and_garbage() {
        let cands = vec![cand(0)];
        let good = r#"{"answers":{"f0":{"route":"keep","cut_strength":4,"too_similar":0.1,"confidence":0.9}}}"#;
        let out = parse_answers(good, &cands).unwrap();
        assert_eq!(out.keeps, vec!["f0"]);
        assert!(parse_answers("not json", &cands).is_err());
        // Missing `answers` wrapper → empty, all skip.
        let out = parse_answers("{}", &cands).unwrap();
        assert_eq!(out.skips, vec!["f0"]);
    }

    #[test]
    fn route_doc_carries_facts_not_vectors() {
        let pack = serde_json::json!({"brief": "b", "candidates": [{"id": "f0", "change": 0.5}]});
        let doc = route_doc(&pack, &Brief::default());
        let text = doc.to_string();
        assert!(text.contains("jev_route"));
        assert!(!text.contains("embedding"), "no vector keys in the request");
        assert_eq!(
            doc["questions"]["route"],
            serde_json::json!(["keep", "skip", "maybe"])
        );
    }
}
