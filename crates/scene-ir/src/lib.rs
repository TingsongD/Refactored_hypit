//! Canonical scene IR.
//!
//! Everything a stage produces or consumes lives here as a plain data type.
//! Wire format identifier: [`IR_FORMAT`]. Serialized structures are immutable
//! within a major version — evolve them by adding optional fields or bumping
//! the format, never by changing existing meanings.

mod anchor;
mod diagnostic;
mod scene;
mod span;

pub use anchor::{
    Anchor, AnchorRange, AnchorRef, BindPath, BindProp, Edge, Granularity, Offset, OffsetUnit,
};
pub use diagnostic::{Diagnostic, Severity, has_errors};
pub use scene::{
    AnimKind, Canvas, CaptionStyle, Color, Element, ElementKind, IR_FORMAT, NamedPlacement,
    Placement, Rational, RenderTarget, Scene, Script, ScriptLine, TextContent, Track, TrackKind,
    parse_gain_db, parse_secs, script_fingerprint,
};
pub use span::Span;
