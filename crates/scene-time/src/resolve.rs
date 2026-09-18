//! Realization: resolve every symbolic anchor into concrete seconds, then
//! quantize to program frames and 48 kHz samples.
//!
//! Ordering, which is the whole trick:
//! 1. resolve every *explicit* `during` against the measured word stream —
//!    all diagnostics are produced by this pass
//! 2. program span = max(timing-source end, every resolved end)
//! 3. rebuild the tree — untimed elements inherit, children from parent,
//!    roots from program; anchor math repeats silently (errors already
//!    reported once by pass 1)

use scene_ir::*;

use crate::alignment::{TimingMap, TimingSource};
use crate::domain::*;

/// Canonical program sample rate. Every audio position quantizes here.
pub const SAMPLE_RATE: u32 = 48_000;

/// Frame/sample quantization epsilon: keeps values that landed exactly on
/// a boundary (via `value / fps * fps`) from jittering one quantum low.
const EPS: f64 = 1e-6;

/// Instant → containing program frame (floor).
fn frame_at(seconds: f64, fps: f64) -> u32 {
    (seconds * fps + EPS).floor().max(0.0) as u32
}

/// Instant → exclusive end frame (ceil) so ranges cover what they touch.
fn frame_end_at(seconds: f64, fps: f64) -> u32 {
    (seconds * fps - EPS).ceil().max(0.0) as u32
}

/// Instant → program sample (floor / ceil, same convention).
fn sample_at(seconds: f64) -> u64 {
    (seconds * f64::from(SAMPLE_RATE) + EPS).floor().max(0.0) as u64
}

fn sample_end_at(seconds: f64) -> u64 {
    (seconds * f64::from(SAMPLE_RATE) - EPS).ceil().max(0.0) as u64
}

fn resolved(start_s: f64, end_s: f64, fps: f64) -> ResolvedTiming {
    ResolvedTiming {
        start_s,
        end_s,
        frames: FrameRange {
            start: frame_at(start_s, fps),
            end: frame_end_at(end_s, fps),
        },
        samples: SampleRange {
            start: sample_at(start_s),
            end: sample_end_at(end_s),
        },
    }
}

/// Pure anchor math over one timing source. Errors come back as message
/// strings; the caller attaches the span.
struct Resolver<'a> {
    /// Word-boundary lattice of the script's timing source.
    boundaries: Vec<f64>,
    fps: f64,
    source: &'a TimingSource,
}

impl<'a> Resolver<'a> {
    fn new(source: &'a TimingSource, fps: f64) -> Self {
        Resolver {
            boundaries: source.word_boundaries(),
            fps,
            source,
        }
    }

    /// Move `pos` `n` word-boundaries along the lattice. `+n` lands on the
    /// nth word start after pos; `-n` on the nth before. Clamps at the ends.
    fn shift_words(&self, pos: f64, n: f64) -> f64 {
        if n == 0.0 {
            return pos;
        }
        let b = &self.boundaries;
        if b.is_empty() {
            return pos;
        }
        let idx = if n > 0.0 {
            let i = b.partition_point(|x| *x <= pos + EPS);
            (i + n as usize).saturating_sub(1)
        } else {
            let i = b.partition_point(|x| *x < pos - EPS);
            (i as i64 + n as i64).max(0) as usize
        };
        b[idx.min(b.len() - 1)]
    }

    fn anchor_seconds(&self, anchor: &Anchor) -> Result<f64, String> {
        match anchor {
            Anchor::Literal { value, unit } => Ok(match unit {
                OffsetUnit::Seconds => *value,
                OffsetUnit::Frames => *value / self.fps,
                OffsetUnit::Words => {
                    return Err("literal anchors do not take the `w` unit".to_string());
                }
            }),
            Anchor::Cue { cue, edge, offset } => {
                let line = self
                    .source
                    .line(cue)
                    .ok_or_else(|| format!("no timing data for cue `{cue}`"))?;
                let base = match edge {
                    Edge::Start => line.start_s(),
                    Edge::End => line.end_s(),
                }
                .ok_or_else(|| format!("cue `{cue}` has no aligned words"))?;
                let Some(offset) = offset else {
                    return Ok(base);
                };
                Ok(match offset.unit {
                    OffsetUnit::Seconds => base + offset.value,
                    OffsetUnit::Frames => base + offset.value / self.fps,
                    OffsetUnit::Words => self.shift_words(base, offset.value),
                })
            }
        }
    }

    fn range_seconds(&self, range: &AnchorRange) -> Result<(f64, f64), String> {
        let start = self.anchor_seconds(&range.start)?;
        let end = self.anchor_seconds(&range.end)?;
        if end <= start {
            return Err(format!(
                "anchor range resolves to an empty span ({start:.3}s..{end:.3}s)"
            ));
        }
        Ok((start, end))
    }
}

/// Realize a scene against measured timings. The resolved scene comes
/// back only when zero error diagnostics fired; diagnostics are returned
/// either way.
pub fn realize(scene: &Scene, timings: &TimingMap) -> (Option<ResolvedScene>, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let fps = scene.frame_rate.to_f64();

    // The script's timing source drives cue resolution.
    let source = scene.script.as_ref().and_then(|s| timings.get(&s.track));
    if let Some(script) = &scene.script
        && source.is_none()
    {
        diags.push(Diagnostic::error(
            format!("no timing data for script track `{}`", script.track),
            Some(script.span),
        ));
    }
    let empty_source = TimingSource::default();
    let source = source.unwrap_or(&empty_source);
    let resolver = Resolver::new(source, fps);

    // Pass 1 — validate every explicit `during`, find the program bound.
    let mut latest_end = source.end_s();
    for track in &scene.tracks {
        let mut stack: Vec<&Element> = track.elements.iter().collect();
        while let Some(element) = stack.pop() {
            if let Some(range) = &element.timing {
                match resolver.range_seconds(range) {
                    Ok((_, end)) => latest_end = latest_end.max(end),
                    Err(message) => {
                        diags.push(Diagnostic::error(message, Some(element.span)));
                    }
                }
            }
            stack.extend(&element.children);
        }
    }

    if latest_end <= 0.0 {
        diags.push(Diagnostic::error(
            "scene defines no timing — nothing resolves past t=0".to_string(),
            scene.script.as_ref().map(|s| s.span),
        ));
    }

    let program = resolved(0.0, latest_end, fps);

    // Pass 2 — build the resolved tree; untimed elements inherit downward.
    let tracks = scene
        .tracks
        .iter()
        .map(|track| ResolvedTrack {
            id: track.id.clone(),
            kind: track.kind,
            anchor: track.anchor.clone(),
            elements: track
                .elements
                .iter()
                .map(|e| resolve_element(e, (program.start_s, program.end_s), &resolver, fps))
                .collect(),
        })
        .collect();

    let scene_out = if has_errors(&diags) {
        None
    } else {
        Some(ResolvedScene {
            canvas: scene.canvas,
            frame_rate: scene.frame_rate,
            clear: scene.clear,
            script: scene.script.clone(),
            program,
            tracks,
        })
    };
    (scene_out, diags)
}

fn resolve_element(
    element: &Element,
    inherited: (f64, f64),
    resolver: &Resolver,
    fps: f64,
) -> ResolvedElement {
    let (start_s, end_s) = element
        .timing
        .as_ref()
        .and_then(|range| resolver.range_seconds(range).ok())
        .unwrap_or(inherited);
    ResolvedElement {
        id: element.id.clone(),
        kind: element.kind.clone(),
        timing: resolved(start_s, end_s, fps),
        placement: element.placement,
        anim: element.anim,
        children: element
            .children
            .iter()
            .map(|c| resolve_element(c, (start_s, end_s), resolver, fps))
            .collect(),
        span: element.span,
    }
}
