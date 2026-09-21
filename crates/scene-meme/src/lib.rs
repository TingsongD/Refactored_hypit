//! Flash-cut meme analysis: find the important frames in a clip, route
//! them through cheap stages before expensive ones, and emit a `.scene`
//! draft of the reproduction.
//!
//! The pipeline's hard boundary: code sees pixels and does the math,
//! Jev routes on readable fact sheets, Gemini sees only approved
//! frames/windows. Embeddings and raw video never enter a Jev request;
//! Gemini never gets the whole source.

pub mod brief;
pub mod cache;
pub mod embed;
pub mod emit;
mod error;
pub mod gemini;
pub mod jev;
pub mod metrics;
pub mod pack;
pub mod package;
pub mod peaks;
pub mod perceive;
pub mod run;

pub use brief::{Brief, DEFAULT_TAG_VOCAB};
pub use error::MemeError;
pub use metrics::{AudioHop, FrameMetrics};
pub use peaks::{Candidate, PeakSource, pick_peaks};
pub use perceive::{Perceive, perceive};
