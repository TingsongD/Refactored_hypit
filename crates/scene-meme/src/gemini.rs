//! Gemini analysis — the only stage that sees pixels, and only the
//! approved ones: the kept beats as labeled stills, or short windows
//! around them. Never the whole source at native fps, never frames
//! code didn't pick.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use scene_cap::{CapRequest, Registry, fulfill};
use scene_media::{MediaError, Tool, output_timeout};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::brief::Brief;
use crate::error::MemeError;
use crate::peaks::Candidate;

/// Still extraction is one-shot ffmpeg — a hung decoder is a bug, and
/// the deadline keeps it a reported error rather than a stuck run.
const EXTRACT_LIMIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiMode {
    /// One labeled still per keep — the default, cheapest path.
    Stills,
    /// A `{gemini_window_sec}`-wide clip per keep at `gemini_fps`.
    Windows,
}

/// What Gemini returns — the connector normalizes into this shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeminiAnalysis {
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub joke: String,
    #[serde(default)]
    pub text_on_screen: Vec<String>,
    #[serde(default)]
    pub beats: Vec<Beat>,
    #[serde(default)]
    pub drop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl GeminiAnalysis {
    /// A window retry replaces only that beat; the full analysis stays available.
    pub fn merge_window(&mut self, id: &str, update: Self) {
        if !update.beats.iter().any(|b| b.id == id) {
            return;
        }
        self.beats.retain(|b| b.id != id);
        self.beats
            .extend(update.beats.into_iter().filter(|b| b.id == id));
        self.beats
            .sort_by(|a, b| a.t.total_cmp(&b.t).then(a.id.cmp(&b.id)));
        self.drop.retain(|entry| entry != id);
        self.drop
            .extend(update.drop.into_iter().filter(|entry| entry == id));
        for text in update.text_on_screen {
            if !self.text_on_screen.contains(&text) {
                self.text_on_screen.push(text);
            }
        }
        self.usage = update.usage;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Beat {
    #[serde(default)]
    pub id: String,
    pub t: f64,
    #[serde(default)]
    pub role: String,
    #[serde(default = "yes")]
    pub keep: bool,
    #[serde(default)]
    pub note: String,
}

fn yes() -> bool {
    true
}

/// One materialized file for a keep — path is relative to the request
/// `out` dir so the doc stays self-describing.
#[derive(Default)]
pub struct KeepFile {
    pub id: String,
    pub t: f64,
    pub frame: u64,
    pub file: PathBuf,
    pub representative_t: f64,
    pub cut_on_beat: bool,
    pub onset: f64,
    pub word: Option<String>,
}

/// Extract one full-res PNG per keep: `ffmpeg -ss {t} -i src -frames:v 1`.
/// Files land in `dir` — the caller confines that under the out root.
pub fn materialize_stills(
    video: &Path,
    keeps: &[&Candidate],
    dir: &Path,
) -> Result<Vec<KeepFile>, MemeError> {
    std::fs::create_dir_all(dir).map_err(MemeError::io(dir))?;
    let tool = std::env::var("FFMPEG").unwrap_or_else(|_| Tool::Ffmpeg.name().to_string());
    let mut files = Vec::new();
    for k in keeps {
        let file = dir.join(format!("{}.png", k.id));
        let staged = scene_media::StagedOutput::new(&file).map_err(MemeError::io(&file))?;
        let mut cmd = Command::new(&tool);
        cmd.args([
            "-y",
            "-v",
            "error",
            "-ss",
            &format!("{:.6}", k.representative_t),
        ])
        .arg("-i")
        .arg(video)
        .args(["-frames:v", "1", "-f", "image2"])
        .arg(staged.path());
        let out = output_timeout(&mut cmd, "ffmpeg", EXTRACT_LIMIT)?;
        if !out.status.success() || !staged.path().exists() {
            return Err(MemeError::Media(MediaError::Failed {
                tool: "ffmpeg",
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }));
        }
        staged.publish().map_err(MemeError::io(&file))?;
        files.push(KeepFile {
            id: k.id.clone(),
            t: k.t,
            frame: k.frame,
            representative_t: k.representative_t,
            cut_on_beat: k.cut_on_beat,
            onset: k.onset,
            word: k.word.clone(),
            file,
        });
    }
    Ok(files)
}

/// Transcode a `{gemini_window_sec}`-wide clip per keep at
/// `gemini_fps` — Gemini's default 1 fps sampling would step over
/// 2–6-frame cuts, so windows are re-sampled explicitly.
pub fn materialize_windows(
    video: &Path,
    keeps: &[&Candidate],
    dir: &Path,
    brief: &Brief,
) -> Result<Vec<KeepFile>, MemeError> {
    std::fs::create_dir_all(dir).map_err(MemeError::io(dir))?;
    let tool = std::env::var("FFMPEG").unwrap_or_else(|_| Tool::Ffmpeg.name().to_string());
    let w = brief.gemini_window_sec;
    let mut files = Vec::new();
    for k in keeps {
        let file = dir.join(format!("{}.mp4", k.id));
        let from = (k.t - w / 2.0).max(0.0);
        let to = k.t + w / 2.0;
        let staged = scene_media::StagedOutput::new(&file).map_err(MemeError::io(&file))?;
        let mut cmd = Command::new(&tool);
        cmd.args([
            "-y",
            "-v",
            "error",
            "-ss",
            &format!("{:.6}", from),
            "-to",
            &format!("{:.6}", to),
            "-i",
        ])
        .arg(video)
        .args([
            "-r",
            &format!("{}", brief.gemini_fps),
            "-c:v",
            "libx264",
            "-crf",
            "28",
            "-pix_fmt",
            "yuv420p",
            "-an",
        ])
        .arg(staged.path());
        let out = output_timeout(&mut cmd, "ffmpeg", EXTRACT_LIMIT)?;
        if !out.status.success() || !staged.path().exists() {
            return Err(MemeError::Media(MediaError::Failed {
                tool: "ffmpeg",
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }));
        }
        staged.publish().map_err(MemeError::io(&file))?;
        files.push(KeepFile {
            id: k.id.clone(),
            t: k.t,
            frame: k.frame,
            representative_t: k.representative_t,
            cut_on_beat: k.cut_on_beat,
            onset: k.onset,
            word: k.word.clone(),
            file,
        });
    }
    Ok(files)
}

/// The analysis prompt — built in code, carrying the brief and the
/// ordered beat list. The connector only ships it.
fn prompt(brief: &Brief, mode: GeminiMode, files: &[KeepFile]) -> String {
    let what = match mode {
        GeminiMode::Stills => {
            "Each image is one kept beat, labeled by id. Cuts in this genre land \
             2–6 frames apart; each still is the sharpest frame at its beat."
        }
        GeminiMode::Windows => {
            "Each clip is a short window centered on a kept beat, re-sampled \
             high enough that 2–6-frame cuts stay visible."
        }
    };
    let list = files
        .iter()
        .map(|f| format!("{} (beat_t={:.6}, representative_t={:.6}, frame {}, cut_on_beat={}, onset={}, word={:?})", f.id, f.t, f.representative_t, f.frame, f.cut_on_beat, f.onset, f.word))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are analyzing the kept beats of a flash-cut meme. Brief: {} \
         {} The beats in order: {}. Reply with ONLY a JSON object: \
         {{\"verdict\": one line, \"joke\": what the meme is doing, \
         \"text_on_screen\": [strings seen], \"beats\": [{{\"id\", \"t\", \
         \"role\", \"keep\": bool, \"note\"}}], \"drop\": [ids or notes to \
         cut], \"missing\": what beat the sequence lacks, or null}}",
        brief.description, what, list
    )
}

/// The request document: keeps carry file *paths* relative to nothing —
/// the connector resolves them on its side. No bytes travel in params.
pub fn gemini_doc(brief: &Brief, mode: GeminiMode, files: &[KeepFile]) -> Value {
    let mode_s = match mode {
        GeminiMode::Stills => "stills",
        GeminiMode::Windows => "windows",
    };
    json!({
        "task": "gemini_analyze",
        "mode": mode_s,
        "model": brief.gemini_model,
        "max_output_tokens": brief.max_output_tokens,
        "brief": {
            "job": brief.job,
            "description": brief.description,
            "gemini_fps": brief.gemini_fps,
        },
        "keeps": files
            .iter()
            .map(|f| json!({
                "id": f.id,
                "t": f.t,
                "frame": f.frame,
                "representative_t": f.representative_t,
                "cut_on_beat": f.cut_on_beat,
                "onset": f.onset,
                "word": f.word,
                "file": f.file.display().to_string(),
            }))
            .collect::<Vec<_>>(),
        "prompt": prompt(brief, mode, files),
    })
}

/// Run the analysis stage. `files` is the *subset* this call covers —
/// pass all keeps for the main call, or one window for a rerun.
pub fn analyze(
    reg: &Registry,
    capability: &str,
    brief: &Brief,
    mode: GeminiMode,
    files: &[KeepFile],
    out: &Path,
) -> Result<GeminiAnalysis, MemeError> {
    let req = CapRequest {
        capability,
        params: gemini_doc(brief, mode, files),
        out,
    };
    let produced = fulfill(reg, &req)?;
    let text = std::fs::read_to_string(&produced).map_err(MemeError::io(&produced))?;
    parse_analysis(&text)
}

/// Parse the response — tolerates a markdown-fenced JSON body and a
/// `{"text": "...json..."}` wrapper, since connectors normalize the
/// vendor's shape themselves.
pub fn parse_analysis(text: &str) -> Result<GeminiAnalysis, MemeError> {
    let t = text.trim();
    let inner = if let Ok(v) = serde_json::from_str::<Value>(t)
        && let Some(s) = v.get("text").and_then(|x| x.as_str())
    {
        s.to_string()
    } else {
        t.to_string()
    };
    let stripped = inner
        .trim()
        .strip_prefix("```json")
        .or_else(|| inner.trim().strip_prefix("```"))
        .unwrap_or(inner.trim())
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(stripped)
        .map_err(|e| MemeError::Stage(format!("gemini response is not valid JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_carries_paths_not_bytes() {
        let files = vec![
            KeepFile {
                id: "f30".into(),
                t: 1.0,
                representative_t: 1.0,
                frame: 30,
                file: PathBuf::from("frames/f30.png"),
                ..Default::default()
            },
            KeepFile {
                id: "f60".into(),
                t: 2.0,
                representative_t: 2.0,
                frame: 60,
                file: PathBuf::from("frames/f60.png"),
                ..Default::default()
            },
        ];
        let doc = gemini_doc(&Brief::default(), GeminiMode::Stills, &files);
        let text = doc.to_string();
        assert!(!text.contains("base64"), "no pixel bytes in the doc");
        assert!(!text.contains("data:"), "no data URIs");
        assert_eq!(doc["keeps"].as_array().unwrap().len(), 2);
        assert_eq!(doc["keeps"][0]["file"], "frames/f30.png");
        assert!(doc["prompt"].as_str().unwrap().contains("f30"));
    }

    #[test]
    fn parse_tolerates_fences_and_wrappers() {
        let plain = r#"{"verdict":"works","joke":"snap zoom","text_on_screen":["NO"],"beats":[],"drop":[],"missing":null}"#;
        let a = parse_analysis(plain).unwrap();
        assert_eq!(a.verdict, "works");
        let fenced = format!("```json\n{plain}\n```");
        assert_eq!(parse_analysis(&fenced).unwrap().verdict, "works");
        let wrapped = format!("{{\"text\": {}}}", serde_json::to_string(&fenced).unwrap());
        assert_eq!(parse_analysis(&wrapped).unwrap().verdict, "works");
        assert!(parse_analysis("not json at all").is_err());
        assert!(
            parse_analysis("{}").is_ok(),
            "empty object parses as defaults"
        );
    }
}
