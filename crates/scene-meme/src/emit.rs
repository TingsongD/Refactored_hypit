//! Emit the flash-cut `.scene` — the export artifact. Each keep becomes
//! a `<clip>` beat on the visual track plus a matching `<sound>` slice
//! on the audio track, so the meme's audio cuts with its picture. Two
//! beat modes: `from` offsets against the source (normal — no media
//! duplication), or materialized beat clips (`--materialize`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use scene_media::{MediaError, Tool, output_timeout};
use serde::Serialize;

use crate::error::MemeError;
use crate::peaks::Candidate;

/// Beat re-encode is one-shot ffmpeg per keep.
const BEAT_LIMIT: Duration = Duration::from_secs(120);

/// Seconds-typed anchor literal, ms-rounded — `AnchorRange::parse`
/// grammar.
fn t(s: f64) -> String {
    format!("{:.3}s", (s * 1000.0).round() / 1000.0)
}

fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One beat's resolved placement: source window → program slot.
#[derive(Debug, Clone, Serialize)]
pub struct BeatSpan {
    pub id: String,
    /// Source window this beat plays.
    pub src_from: f64,
    pub src_to: f64,
    /// Program slot (sequential, `beat_sec` each).
    pub start: f64,
    pub end: f64,
    /// For materialized beats — the physical file; `None` for
    /// `from`-mode beats.
    pub file: Option<PathBuf>,
}

/// Resolve keeps to beats: `t ± beat_sec/2` clamped to `[0, duration]`,
/// sequential program slots. Keeps are re-sorted by time — callers may
/// hand back Jev's order.
pub fn beat_spans(keeps: &[&Candidate], beat_sec: f64, duration_s: f64) -> Vec<BeatSpan> {
    let mut sorted: Vec<&Candidate> = keeps.to_vec();
    sorted.sort_by(|a, b| a.t.total_cmp(&b.t));
    let mut out = Vec::new();
    for (i, k) in sorted.iter().enumerate() {
        let mut from = (k.t - beat_sec / 2.0).max(0.0);
        let mut to = (k.t + beat_sec / 2.0).min(duration_s);
        // Tail keeps clamp left; never emit a degenerate or reversed cut.
        if to - from < 0.05 {
            to = (from + beat_sec).min(duration_s);
            from = (to - beat_sec).max(0.0);
        }
        if to - from < 0.05 {
            continue;
        }
        out.push(BeatSpan {
            id: k.id.clone(),
            src_from: from,
            src_to: to,
            start: i as f64 * beat_sec,
            end: (i + 1) as f64 * beat_sec,
            file: None,
        });
    }
    out
}

/// Physically cut each beat to `dir/beat_NN.mp4` (re-encoded — a clean
/// cut needs keyframes, `-c copy` would land on the nearest one). The
/// materialized file carries the audio slice too.
pub fn materialize_beats(
    video: &Path,
    spans: &mut [BeatSpan],
    dir: &Path,
) -> Result<(), MemeError> {
    std::fs::create_dir_all(dir).map_err(MemeError::io(dir))?;
    let tool = std::env::var("FFMPEG").unwrap_or_else(|_| Tool::Ffmpeg.name().to_string());
    for (i, s) in spans.iter_mut().enumerate() {
        let file = dir.join(format!("beat_{i:02}.mp4"));
        let mut cmd = Command::new(&tool);
        cmd.args([
            "-y",
            "-v",
            "error",
            "-ss",
            &format!("{:.6}", s.src_from),
            "-to",
            &format!("{:.6}", s.src_to),
            "-i",
        ])
        .arg(video)
        .args([
            "-c:v", "libx264", "-crf", "20", "-pix_fmt", "yuv420p", "-c:a", "aac",
        ])
        .arg(&file);
        let out = output_timeout(&mut cmd, "ffmpeg", BEAT_LIMIT)?;
        if !out.status.success() || !file.exists() {
            return Err(MemeError::Media(MediaError::Failed {
                tool: "ffmpeg",
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }));
        }
        s.file = Some(file);
    }
    Ok(())
}

pub struct EmitSpec<'a> {
    /// Source path as written into the scene (runner makes it resolve).
    pub src: &'a str,
    pub canvas: (u32, u32),
    /// Source rate carried through; `None` → 30.
    pub fps: Option<(u64, u64)>,
    /// The source has an audio stream — controls the `<sound>` slices.
    pub has_audio: bool,
    /// Optional extra bed under the whole montage.
    pub music_src: Option<&'a str>,
    pub render_target: &'a str,
}

/// Build the scene markup. In `from`-mode each beat references the
/// source with a `from` offset; materialized beats reference their
/// physical file with no offset (audio baked in).
pub fn emit_flash_scene(spans: &[BeatSpan], spec: &EmitSpec) -> String {
    let (w, h) = (spec.canvas.0.max(2), spec.canvas.1.max(2));
    let fps = match spec.fps {
        Some((n, d)) if n > 0 && d > 0 => format!("{n}/{d}"),
        _ => "30".to_string(),
    };
    let total = spans.last().map(|s| s.end).unwrap_or(0.0);
    let src = esc_attr(spec.src);

    let mut out = String::new();
    out.push_str(&format!(
        "<scene canvas=\"{w}x{h}\" fps=\"{fps}\" clear=\"#000\">\n  <track kind=\"visual\">\n"
    ));
    for s in spans {
        let (beat_src, from) = match &s.file {
            Some(f) => (esc_attr(&f.display().to_string()), String::new()),
            None => (src.clone(), format!(" from=\"{}\"", t(s.src_from))),
        };
        out.push_str(&format!(
            "    <clip src=\"{beat_src}\"{from} during=\"{}..{}\"/>\n",
            t(s.start),
            t(s.end)
        ));
    }
    out.push_str("  </track>\n");

    // Audio follows the picture — one sound slice per beat from the same
    // source window (materialized beats already carry their audio).
    if spec.has_audio {
        out.push_str("  <track kind=\"audio\">\n");
        for s in spans {
            let (beat_src, from) = match &s.file {
                Some(f) => (esc_attr(&f.display().to_string()), String::new()),
                None => (src.clone(), format!(" from=\"{}\"", t(s.src_from))),
            };
            out.push_str(&format!(
                "    <sound src=\"{beat_src}\"{from} during=\"{}..{}\"/>\n",
                t(s.start),
                t(s.end)
            ));
        }
        if let Some(music) = spec.music_src {
            out.push_str(&format!(
                "    <music src=\"{}\" gain=\"-16dB\" during=\"0s..{}\"/>\n",
                esc_attr(music),
                t(total)
            ));
        }
        out.push_str("  </track>\n");
    }

    out.push_str(&format!(
        "  <render target=\"{}\"/>\n</scene>\n",
        esc_attr(spec.render_target)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peaks::PeakSource;

    fn cand(i: u64, t: f64) -> Candidate {
        Candidate {
            id: format!("f{i}"),
            frame: i,
            t,
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

    fn spec<'a>() -> EmitSpec<'a> {
        EmitSpec {
            src: "source.mp4",
            canvas: (1080, 1920),
            fps: Some((30, 1)),
            has_audio: true,
            music_src: None,
            render_target: "meme.mp4",
        }
    }

    #[test]
    fn beats_are_sequential_and_windows_clamped() {
        let keeps = [cand(30, 1.0), cand(10, 0.33), cand(300, 9.99)];
        let refs: Vec<&Candidate> = keeps.iter().collect();
        let spans = beat_spans(&refs, 0.6, 10.0);
        // Time-sorted: 0.33, 1.0, 9.99 → program slots 0, 0.6, 1.2.
        assert_eq!(spans[0].id, "f10");
        assert_eq!(spans[0].start, 0.0);
        assert_eq!(spans[1].start, 0.6);
        // Head clamp: 0.33 - 0.3 = 0.03 — window stays ≥ 0.
        assert!(spans[0].src_from >= 0.0);
        // Tail clamp: 9.99 + 0.3 → 10.0.
        assert!(spans[2].src_to <= 10.0);
    }

    #[test]
    fn emitted_scene_parses() {
        let keeps = [cand(30, 1.0), cand(90, 3.0)];
        let refs: Vec<&Candidate> = keeps.iter().collect();
        let spans = beat_spans(&refs, 0.6, 10.0);
        let markup = emit_flash_scene(&spans, &spec());
        let out = scene_markup::compile(&markup);
        let errs: Vec<_> = out.diagnostics.iter().filter(|d| d.is_error()).collect();
        assert!(errs.is_empty(), "{errs:?}");
        let scene = out.scene.expect("lowers");
        assert_eq!(scene.tracks.len(), 2);
        let clips = scene.tracks[0]
            .elements
            .iter()
            .filter_map(|e| match &e.kind {
                scene_ir::ElementKind::Clip { src, from_s, .. } => Some((src, *from_s)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].0, "source.mp4");
        assert!((clips[0].1 - 0.7).abs() < 0.001, "from = t - beat/2");
    }

    #[test]
    fn materialized_beats_drop_the_from_offset() {
        let keeps = [cand(30, 1.0)];
        let refs: Vec<&Candidate> = keeps.iter().collect();
        let mut spans = beat_spans(&refs, 0.6, 10.0);
        spans[0].file = Some(PathBuf::from("beats/beat_00.mp4"));
        let markup = emit_flash_scene(&spans, &spec());
        assert!(markup.contains("src=\"beats/beat_00.mp4\""));
        assert!(
            !markup.contains("from="),
            "materialized beats have no offset"
        );
    }

    #[test]
    fn music_bed_and_escaping() {
        let mut s = spec();
        s.music_src = Some("be&d.mp3");
        let keeps = [cand(30, 1.0)];
        let refs: Vec<&Candidate> = keeps.iter().collect();
        let spans = beat_spans(&refs, 0.6, 10.0);
        let markup = emit_flash_scene(&spans, &s);
        assert!(markup.contains("<music src=\"be&amp;d.mp3\""));
        assert!(markup.contains("gain=\"-16dB\""));
    }
}
