//! The markers-file connector: a human-authorable alignment format.
//!
//! ```text
//! # comments and blank lines are ignored
//! hook 0.00 1.50
//! payoff 1.60 3.00
//! ```
//!
//! One `cue start_s end_s` per line. The cue names a script line; the
//! span says when it was spoken. Words are the script line's own text,
//! distributed evenly across the span — so a markers file is a fully
//! deterministic stand-in for a real aligner, and the cheapest way to
//! hand-author timing.

use scene_ir::{Diagnostic, Script, Span};
use scene_time::{AlignedLine, TimingSource, Word};

/// One parsed marker line: `cue [t0, t1)` in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub cue: String,
    pub start_s: f64,
    pub end_s: f64,
    /// Byte span of the source line — parse errors render against the
    /// markers file itself, so offsets must be real.
    pub span: Span,
    /// 1-based source line — carried into warning text, since a
    /// marker-side span can't render inside the scene file.
    pub line: usize,
}

/// Parse marker text. Malformed lines collect as diagnostics rather than
/// failing the whole file — a typo in line 40 shouldn't lose lines 1–39.
pub fn parse_markers(text: &str) -> (Vec<Marker>, Vec<Diagnostic>) {
    let mut markers = Vec::new();
    let mut diags = Vec::new();
    let mut offset = 0usize;
    for (n, raw) in text.lines().enumerate() {
        let span = Span::new(offset, offset + raw.len());
        offset += raw.len() + 1; // + the '\n' lines() strips
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(cue), Some(a), Some(b)) = (parts.next(), parts.next(), parts.next()) else {
            diags.push(Diagnostic::error(
                format!("expected `cue start_s end_s`, found `{line}`"),
                Some(span),
            ));
            continue;
        };
        if parts.next().is_some() {
            diags.push(Diagnostic::error(
                format!("trailing content after `cue start_s end_s`: `{line}`"),
                Some(span),
            ));
            continue;
        }
        let (Ok(start_s), Ok(end_s)) = (a.parse::<f64>(), b.parse::<f64>()) else {
            diags.push(Diagnostic::error(
                format!("times must be numbers: `{line}`"),
                Some(span),
            ));
            continue;
        };
        if end_s <= start_s {
            diags.push(Diagnostic::error(
                format!("end must be after start: `{line}`"),
                Some(span),
            ));
            continue;
        }
        markers.push(Marker {
            cue: cue.to_string(),
            start_s,
            end_s,
            span,
            line: n + 1,
        });
    }
    (markers, diags)
}

/// Markers + the script they describe → a `TimingSource` for
/// `script.track`. Unknown cues and unmarked script lines are warnings —
/// `realize` will hard-error only if an anchor actually touches one.
pub fn markers_to_timing(markers: &[Marker], script: &Script) -> (TimingSource, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let mut lines = Vec::new();
    for line in &script.lines {
        let Some(m) = markers.iter().find(|m| m.cue == line.id) else {
            diags.push(Diagnostic::warning(
                format!(
                    "cue `{}` has no marker — anchors on it won't resolve",
                    line.id
                ),
                Some(line.span),
            ));
            continue;
        };
        lines.push(AlignedLine {
            cue: line.id.clone(),
            words: spread(&line.text, m.start_s, m.end_s),
        });
    }
    for m in markers {
        if !script.lines.iter().any(|l| l.id == m.cue) {
            // Marker-side span would render inside the wrong file —
            // the message carries its location instead.
            diags.push(Diagnostic::warning(
                format!("marker for unknown cue `{}` (markers:{})", m.cue, m.line),
                None,
            ));
        }
    }
    (TimingSource { lines }, diags)
}

/// Split `text` into words laid evenly across `[start_s, end_s)`.
/// Deterministic to the nanosecond — same inputs, same lattice.
fn spread(text: &str, start_s: f64, end_s: f64) -> Vec<Word> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let n = words.len() as f64;
    if n == 0.0 {
        return Vec::new();
    }
    let step = (end_s - start_s) / n;
    words
        .iter()
        .enumerate()
        .map(|(i, w)| Word {
            text: w.to_string(),
            start_s: start_s + i as f64 * step,
            end_s: start_s + (i + 1) as f64 * step,
        })
        .collect()
}
