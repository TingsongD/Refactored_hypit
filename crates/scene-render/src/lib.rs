//! scene-render — the deterministic software rasterizer.
//!
//! `ResolvedScene` + `TimingMap` → `layout_frame` draw list → tiny-skia
//! pixmap → premultiplied RGBA → NV12 for the encoder. Text is shaped by
//! cosmic-text; media frames arrive through the `FrameSource` seam; work
//! is sharded by `render_frames_into` — frames stream to the consumer in
//! order over bounded per-shard channels, same bytes at any worker count.

mod media;
mod nv12;
mod pool;
mod raster;
mod text;

use scene_ir::ElementKind;
use scene_layout::{Measure, Size};
use scene_time::{ResolvedElement, ResolvedScene, TimingMap};

pub use media::{
    FrameSource, SeqFrameSource, StillFrameSource, WarnSink, decode_still, placeholder_frame,
};
pub use nv12::rgba_to_nv12;
pub use pool::{RenderedFrame, RendererFactory, render_frames, render_frames_into};
pub use raster::{CAPTION_SIZE_PX, Renderer, TEXT_SIZE_PX, TEXT_WRAP};
pub use text::{CosmicText, RichSpan, TextEngine};

/// Production `Measure`: text and captions are measured with the real
/// text engine; a `board` with no children has no intrinsic content and
/// stays unplaced.
pub struct RenderMeasure<'a> {
    pub scene: &'a ResolvedScene,
    pub timings: &'a TimingMap,
    pub text: Box<dyn TextEngine>,
}

impl Measure for RenderMeasure<'_> {
    fn measure(&mut self, element: &ResolvedElement, local_s: f64) -> Option<Size> {
        match &element.kind {
            ElementKind::Text { content } => {
                let text = match content {
                    scene_ir::TextContent::Literal(t) => t.clone(),
                    scene_ir::TextContent::Bind(path) => self
                        .scene
                        .script
                        .as_ref()?
                        .lines
                        .iter()
                        .find(|l| l.id == path.line)?
                        .text
                        .clone(),
                };
                if text.is_empty() {
                    return None;
                }
                Some(self.text.measure(&text, TEXT_SIZE_PX, TEXT_WRAP))
            }
            ElementKind::Captions { source, .. } => {
                let timing = self.timings.get(&source.source)?;
                let t_s = element.timing.start_s + local_s;
                let (line, _) = raster::active_line(timing, t_s)?;
                let text = line
                    .words
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                Some(self.text.measure(&text, CAPTION_SIZE_PX, TEXT_WRAP))
            }
            _ => None,
        }
    }
}
