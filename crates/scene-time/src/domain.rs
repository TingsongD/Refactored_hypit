//! The frame domain: a scene after realization, where every timing is a
//! concrete frame/sample range. Rendering only ever sees this — never an
//! anchor, never a second.

use scene_ir::{
    AnchorRef, AnimKind, Canvas, Color, ElementKind, Placement, Rational, Script, Span, TrackKind,
};

/// Program frames; `end` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRange {
    pub start: u32,
    pub end: u32,
}

impl FrameRange {
    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// Canonical 48 kHz program samples; `end` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleRange {
    pub start: u64,
    pub end: u64,
}

/// One resolved span, expressed on every clock the pipeline cares about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedTiming {
    pub start_s: f64,
    pub end_s: f64,
    pub frames: FrameRange,
    pub samples: SampleRange,
}

#[derive(Debug, Clone)]
pub struct ResolvedElement {
    pub id: Option<String>,
    pub kind: ElementKind,
    pub timing: ResolvedTiming,
    pub placement: Option<Placement>,
    pub anim: Option<AnimKind>,
    pub children: Vec<ResolvedElement>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ResolvedTrack {
    pub id: String,
    pub kind: TrackKind,
    pub anchor: Option<AnchorRef>,
    pub elements: Vec<ResolvedElement>,
}

/// A scene whose timing is fully realized. `program` is the renderable
/// span — frame 0 through `program.frames.end`.
#[derive(Debug, Clone)]
pub struct ResolvedScene {
    pub canvas: Canvas,
    pub frame_rate: Rational,
    pub clear: Color,
    pub script: Option<Script>,
    pub program: ResolvedTiming,
    pub tracks: Vec<ResolvedTrack>,
}
