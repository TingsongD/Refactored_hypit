//! DrawList — the data boundary between sandboxed JS and the rasterizer.
//! Scripts emit op *objects*; the host replays them with tiny-skia. No
//! handles, no callbacks — a program can only ever produce this data.

use scene_ir::Color;
use serde::Deserialize;

/// One drawing operation, in element-local coordinates.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum DrawOp {
    Rect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        c: String,
    },
    Circle {
        x: f64,
        y: f64,
        r: f64,
        c: String,
    },
    Text {
        t: String,
        x: f64,
        y: f64,
        size: f64,
        c: String,
    },
}

impl DrawOp {
    /// Parse the op's `#rrggbb[aa]` fill once — a bad color degrades to
    /// transparent rather than failing the frame.
    pub fn color(&self) -> Color {
        let s = match self {
            DrawOp::Rect { c, .. } | DrawOp::Circle { c, .. } | DrawOp::Text { c, .. } => c,
        };
        Color::parse(s).unwrap_or(Color::TRANSPARENT)
    }
}

/// Ordered ops for one frame — replayed back-to-front.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrawList(pub Vec<DrawOp>);

impl DrawList {
    /// The JS side returns ops as a JSON string; parse tolerantly —
    /// malformed ops are the program's bug, not a reason to kill the
    /// frame. Returns the list plus how many ops were dropped.
    pub fn from_json(json: &str) -> (Self, usize) {
        let values: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
        let mut ops = Vec::with_capacity(values.len());
        let mut dropped = 0;
        for v in values {
            match serde_json::from_value(v) {
                Ok(op) => ops.push(op),
                Err(_) => dropped += 1,
            }
        }
        (DrawList(ops), dropped)
    }
}
