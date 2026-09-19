//! WhisperX connector: service JSON → `TimingSource`. Pure parsing —
//! the HTTP fetch lives in the capability layer (M8); this module only
//! knows the document shape and how to order-match it to a script.
//!
//! WhisperX emits `segments[]` with measured `words[]`. Segments are
//! matched to script lines *by order* — WhisperX doesn't know our cue
//! ids, and alignment output preserves spoken order.

use scene_ir::{Diagnostic, Script};
use scene_time::{AlignedLine, TimingSource, Word};
use serde::Deserialize;

#[derive(Deserialize)]
struct WxOutput {
    #[serde(default)]
    segments: Vec<WxSegment>,
}

#[derive(Deserialize)]
struct WxSegment {
    #[serde(default)]
    words: Vec<WxWord>,
}

#[derive(Deserialize)]
struct WxWord {
    word: Option<String>,
    start: Option<f64>,
    end: Option<f64>,
}

/// Parse WhisperX JSON against `script` → `TimingSource` for
/// `script.track`. Word-level times are real measurements — no spreading.
///
/// Mismatches are warnings, not fatal: unaligned words drop out (an
/// aligner reports them without times), extra/missing segments warn, and
/// a line left with no words warns — `realize` errors only if an anchor
/// touches it.
pub fn whisperx_to_timing(
    json: &str,
    script: &Script,
) -> Result<(TimingSource, Vec<Diagnostic>), String> {
    let wx: WxOutput = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut diags = Vec::new();
    let mut lines = Vec::new();

    for (i, line) in script.lines.iter().enumerate() {
        let Some(seg) = wx.segments.get(i) else {
            diags.push(Diagnostic::warning(
                format!(
                    "cue `{}` has no segment {} in alignment output",
                    line.id,
                    i + 1
                ),
                Some(line.span),
            ));
            continue;
        };
        let words: Vec<Word> = seg
            .words
            .iter()
            .filter_map(|w| {
                Some(Word {
                    text: w.word.clone()?,
                    start_s: w.start?,
                    end_s: w.end?,
                })
            })
            .collect();
        if words.is_empty() {
            diags.push(Diagnostic::warning(
                format!("cue `{}` aligned to zero timed words", line.id),
                Some(line.span),
            ));
        }
        lines.push(AlignedLine {
            cue: line.id.clone(),
            words,
        });
    }

    if wx.segments.len() > script.lines.len() {
        diags.push(Diagnostic::warning(
            format!(
                "alignment returned {} extra segment(s), ignored",
                wx.segments.len() - script.lines.len()
            ),
            None,
        ));
    }
    Ok((TimingSource { lines }, diags))
}

/// Wrap a `TimingSource` in the `TimingMap` shape `engine render
/// --timings` consumes, keyed on the script's own track name.
pub fn timing_map_for(script: &Script, source: TimingSource) -> scene_time::TimingMap {
    let mut map = scene_time::TimingMap::default();
    map.insert(script.track.clone(), source);
    map.script_hash = Some(scene_ir::script_fingerprint(script));
    map
}
