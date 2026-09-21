//! Jev package — the final typed decision. State is the Gemini
//! analysis plus the keep list; the questions are the choice, the
//! completeness check, and the postable score. `rerun_window` names one
//! keep's window to re-analyze — the loop that calls it lives in the
//! runner; this module only produces and parses the decision.

use std::path::Path;

use scene_cap::{CapRequest, Registry, fulfill};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::brief::Brief;
use crate::error::MemeError;
use crate::gemini::GeminiAnalysis;
use crate::peaks::Candidate;

/// What the run does next.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "package", rename_all = "snake_case")]
pub enum Package {
    /// Ship it — emit the scene with the keep list as-is.
    Export,
    /// The sequence doesn't work; the runner should widen the net
    /// (looser thresholds or more candidates) and re-route.
    NeedMorePeaks,
    /// One keep's analysis failed or was empty — rerun Gemini on that
    /// window only; every other stage's cache stays valid.
    RerunWindow { keep_id: String },
}

/// The package stage's answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageOutcome {
    /// Flattened: the wire is `{"package":"export",...}` /
    /// `{"package":"rerun_window","keep_id":"f371",...}`.
    #[serde(flatten)]
    pub package: Package,
    /// Noul — did Gemini's analysis cover every keep.
    #[serde(default)]
    pub analysis_complete: bool,
    /// 1..5 — is the result postable.
    #[serde(default)]
    pub postable: u8,
    #[serde(default = "one")]
    pub confidence: f64,
    /// Optional model note — lands in the report.
    #[serde(default)]
    pub note: String,
}

fn one() -> f64 {
    1.0
}

/// The request document: the analysis JSON and the keep list, plus the
/// brief. Like routing — facts, never pixels or vectors.
pub fn package_doc(brief: &Brief, keeps: &[&Candidate], analysis: &GeminiAnalysis) -> Value {
    json!({
        "task": "jev_package",
        "model": brief.jev_model,
        "brief": {
            "job": brief.job,
            "description": brief.description,
        },
        "state": {
            "keeps": keeps
                .iter()
                .map(|k| json!({"id": k.id, "t": k.t, "frame": k.frame}))
                .collect::<Vec<_>>(),
            "gemini": serde_json::to_value(analysis).unwrap_or_default(),
        },
        "questions": {
            "package": ["export", "need_more_peaks", "rerun_window"],
            "keep_id": "if rerun_window: which keep's window failed",
            "analysis_complete": "true/false — did the analysis cover every keep",
            "postable": "1..5",
        },
    })
}

/// Parse the package response — strict: a malformed decision fails the
/// stage rather than guessing at intent.
pub fn parse_package(text: &str) -> Result<PackageOutcome, MemeError> {
    let out: PackageOutcome = serde_json::from_str(text)
        .map_err(|e| MemeError::Stage(format!("jev package response is not valid JSON: {e}")))?;
    if let Package::RerunWindow { keep_id } = &out.package
        && keep_id.is_empty()
    {
        return Err(MemeError::Stage(
            "jev package: rerun_window with no keep_id".into(),
        ));
    }
    Ok(out)
}

/// Run the package stage against the `jev` capability.
pub fn package(
    reg: &Registry,
    capability: &str,
    brief: &Brief,
    keeps: &[&Candidate],
    analysis: &GeminiAnalysis,
    out: &Path,
) -> Result<PackageOutcome, MemeError> {
    let req = CapRequest {
        capability,
        params: package_doc(brief, keeps, analysis),
        out,
    };
    let produced = fulfill(reg, &req)?;
    let text = std::fs::read_to_string(&produced).map_err(MemeError::io(&produced))?;
    let outcome = parse_package(&text)?;
    // Low confidence surfaces to a human — the runner refuses
    // auto-export rather than shipping a maybe.
    if outcome.confidence < brief.jev_min_confidence {
        return Ok(PackageOutcome {
            package: Package::NeedMorePeaks,
            note: format!(
                "package confidence {:.2} under floor {:.2} — needs a human",
                outcome.confidence, brief.jev_min_confidence
            ),
            ..outcome
        });
    }
    Ok(outcome)
}

/// The offline decision — what `run` reports when no jev capability is
/// registered. Deterministic: keeps exist and weren't flagged → export;
/// nothing kept → need_more_peaks.
pub fn fallback_package(keeps: &[&Candidate]) -> PackageOutcome {
    PackageOutcome {
        package: if keeps.is_empty() {
            Package::NeedMorePeaks
        } else {
            Package::Export
        },
        analysis_complete: true,
        postable: 0,
        confidence: 1.0,
        note: "no jev capability — offline decision".into(),
    }
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

    #[test]
    fn parse_all_decisions() {
        let export = parse_package(
            r#"{"package":"export","analysis_complete":true,"postable":4,"confidence":0.9}"#,
        )
        .unwrap();
        assert_eq!(export.package, Package::Export);
        assert!(export.analysis_complete);
        assert_eq!(export.postable, 4);

        let more = parse_package(r#"{"package":"need_more_peaks"}"#).unwrap();
        assert_eq!(more.package, Package::NeedMorePeaks);
        assert!(!more.analysis_complete);

        let rerun =
            parse_package(r#"{"package":"rerun_window","keep_id":"f371","confidence":0.8}"#)
                .unwrap();
        assert_eq!(
            rerun.package,
            Package::RerunWindow {
                keep_id: "f371".into()
            }
        );
    }

    #[test]
    fn rerun_without_keep_id_is_an_error() {
        assert!(parse_package(r#"{"package":"rerun_window","keep_id":""}"#).is_err());
        assert!(parse_package(r#"{"package":"bogus"}"#).is_err());
        assert!(parse_package("not json").is_err());
    }

    #[test]
    fn doc_carries_state_not_pixels() {
        let keeps = [cand(5)];
        let refs: Vec<&Candidate> = keeps.iter().collect();
        let analysis = GeminiAnalysis {
            verdict: "works".into(),
            ..Default::default()
        };
        let doc = package_doc(&Brief::default(), &refs, &analysis);
        let text = doc.to_string();
        assert!(text.contains("jev_package"));
        assert!(text.contains("\"verdict\":\"works\""));
        assert!(!text.contains("base64"));
        assert_eq!(doc["state"]["keeps"][0]["id"], "f5");
    }

    #[test]
    fn fallback_exports_when_keeps_exist() {
        let keeps = [cand(5)];
        let refs: Vec<&Candidate> = keeps.iter().collect();
        assert_eq!(fallback_package(&refs).package, Package::Export);
        assert_eq!(fallback_package(&[]).package, Package::NeedMorePeaks);
    }
}
