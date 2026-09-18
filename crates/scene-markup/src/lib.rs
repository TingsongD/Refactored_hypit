//! `.scene` markup: parse, then lower.
//!
//! ```text
//! source text ──parse_document──▶ RawNode ──lower──▶ Scene + diagnostics
//! ```
//!
//! The parser knows syntax only; the lowering stage knows the vocabulary.
//! Errors carry byte spans into the original source.

mod lower;
mod parser;
mod raw;

pub use lower::lower;
pub use parser::parse_document;
pub use raw::{RawAttr, RawChild, RawNode};

use scene_ir::{Diagnostic, Scene};

/// Everything one pass over a `.scene` file can say about it.
pub struct Outcome {
    /// The lowered scene, present only when no Error diagnostics fired.
    pub scene: Option<Scene>,
    /// All findings, warnings included.
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse and lower in one step — the common entry point.
pub fn compile(source: &str) -> Outcome {
    match parse_document(source) {
        Ok(raw) => {
            let (scene, diagnostics) = lower(&raw);
            Outcome { scene, diagnostics }
        }
        Err(fatal) => Outcome {
            scene: None,
            diagnostics: vec![fatal],
        },
    }
}
