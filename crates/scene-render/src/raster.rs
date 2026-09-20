//! The draw executor: `layout_frame` output → a premultiplied RGBA
//! pixmap. One `Renderer` per worker — it owns the measurer, the text
//! engine, and the frame sources, all of which are per-thread state.
//!
//! Transform + opacity semantics: an element draws straight into its
//! destination when both are trivial (the common case — most frames have
//! no entrance anim); otherwise its content renders into a rect-sized
//! layer and composites once, so overlapping content fades/scales as a
//! unit instead of cross-fading its internals.

use tiny_skia::{
    Color as SkColor, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Shader,
    Transform as SkTransform,
};

use scene_ir::{BindProp, Color, ElementKind, NamedPlacement, Placement, TextContent};
use scene_layout::{Measure, PlacedElement, Rect, layout_frame};
use scene_media::Frame;
use scene_script::{DrawList, DrawOp, ProgramSource};
use scene_time::{AlignedLine, ResolvedScene, TimingMap, TimingSource};

use crate::media::{FrameSource, placeholder_frame};
use crate::text::{RichSpan, TextEngine};

/// Caption styling: spoken words accented, upcoming words white.
pub const CAPTION_SIZE_PX: f64 = 64.0;
pub const TEXT_SIZE_PX: f64 = 56.0;
const ACCENT: Color = Color {
    r: 0x5a,
    g: 0xc8,
    b: 0xfa,
    a: 255,
};
const PANEL: Color = Color {
    r: 0x10,
    g: 0x12,
    b: 0x18,
    a: 0xd8,
};
const PANEL_RADIUS: f32 = 24.0;
/// Max line width for wrapped text, in canvas pixels.
pub const TEXT_WRAP: f64 = 900.0;

/// Per-worker render context. Everything a frame needs flows through
/// here; the pool constructs one per thread so font shaping and decode
/// state never cross threads.
pub struct Renderer<'a> {
    pub scene: &'a ResolvedScene,
    /// Aligned words per timing source — caption content comes from here.
    pub timings: &'a TimingMap,
    pub measure: Box<dyn Measure + 'a>,
    pub text: Box<dyn TextEngine + 'a>,
    pub clips: Box<dyn FrameSource + 'a>,
    pub images: Box<dyn FrameSource + 'a>,
    pub programs: Box<dyn ProgramSource + 'a>,
}

impl<'a> Renderer<'a> {
    /// Layout + raster one program frame. Returns premultiplied RGBA
    /// bytes, `canvas.width × canvas.height × 4`.
    pub fn render(&mut self, frame: u32) -> Vec<u8> {
        let (w, h) = (self.scene.canvas.width, self.scene.canvas.height);
        let mut pixmap = Pixmap::new(w, h).expect("canvas dimensions are non-zero");
        let clear = self.scene.clear;
        pixmap.fill(SkColor::from_rgba8(clear.r, clear.g, clear.b, clear.a));

        let draw_list = layout_frame(self.scene, frame, self.measure.as_mut());
        for placed in &draw_list {
            self.draw_placed(&mut pixmap, placed, (0.0, 0.0));
        }
        pixmap.take()
    }

    /// Replay program `ops()` calls for `frames` in order, without
    /// rasterizing — rebuilds the script-side state a continuous run
    /// would carry into the next frame. Pool workers call this over
    /// their shard's prefix so a stateful program sees the same call
    /// sequence at any worker count. Costs layout only (no pixels).
    pub fn warm_programs(&mut self, frames: std::ops::Range<u32>) {
        for frame in frames {
            let draw_list = layout_frame(self.scene, frame, self.measure.as_mut());
            for placed in &draw_list {
                self.warm_placed(placed);
            }
        }
    }

    /// Mirror of `draw_placed`'s ordering for programs only: the same
    /// early-return gates apply (an element that wouldn't draw never
    /// gets its `ops()` call) and children only recurse under `Board`,
    /// matching `draw_content`.
    fn warm_placed(&mut self, placed: &PlacedElement) {
        if placed.rect.w <= 0.0 || placed.rect.h <= 0.0 || placed.opacity <= 0.0 {
            return;
        }
        match &placed.element.kind {
            ElementKind::Program { src, with } => {
                let _ = self.programs.ops(
                    placed.element as *const _ as usize,
                    src,
                    placed.local_frame,
                    with.as_deref().unwrap_or("{}"),
                    placed.rect.w,
                    placed.rect.h,
                );
            }
            ElementKind::Board => {
                for child in &placed.children {
                    self.warm_placed(child);
                }
            }
            _ => {}
        }
    }

    /// Draw one placed element into `dst`. `placed.rect` is relative to
    /// `origin` — children carry board-local rects, roots carry
    /// canvas-space ones.
    fn draw_placed(&mut self, dst: &mut Pixmap, placed: &PlacedElement, origin: (f64, f64)) {
        let rect = Rect {
            x: placed.rect.x + origin.0,
            y: placed.rect.y + origin.1,
            w: placed.rect.w,
            h: placed.rect.h,
        };
        if rect.w <= 0.0 || rect.h <= 0.0 || placed.opacity <= 0.0 {
            return;
        }
        if placed.transform.is_identity() && placed.opacity >= 1.0 {
            // Fast path: content straight into dst at its rect.
            self.draw_content(dst, placed, rect);
            return;
        }
        let (w, h) = (rect.w.ceil() as u32, rect.h.ceil() as u32);
        let Some(mut layer) = Pixmap::new(w, h) else {
            return;
        };
        self.draw_content(
            &mut layer,
            placed,
            Rect {
                x: 0.0,
                y: 0.0,
                w: rect.w,
                h: rect.h,
            },
        );
        // Scale about the rect centre, then offset — p_dst =
        // s·p_local + (rect + d + (1−s)·centre).
        let s = placed.transform.scale;
        let transform = SkTransform::from_row(
            s as f32,
            0.0,
            0.0,
            s as f32,
            (rect.x + placed.transform.dx + (1.0 - s) * rect.w / 2.0) as f32,
            (rect.y + placed.transform.dy + (1.0 - s) * rect.h / 2.0) as f32,
        );
        let paint = PixmapPaint {
            opacity: placed.opacity as f32,
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        };
        dst.draw_pixmap(0, 0, layer.as_ref(), &paint, transform, None);
    }

    /// Element content drawn at `rect` in dst space.
    fn draw_content(&mut self, dst: &mut Pixmap, placed: &PlacedElement, rect: Rect) {
        match &placed.element.kind {
            ElementKind::Clip { src } => {
                let frame = self
                    .clips
                    .sample(
                        src,
                        u64::from(placed.local_frame),
                        self.scene.frame_rate.to_f64(),
                    )
                    .unwrap_or_else(|| placeholder_frame(rect.w as u32, rect.h as u32));
                self.draw_cover(dst, &frame, rect, placed.focal);
            }
            ElementKind::Image { src } => {
                // Same contract as Clip/Program: a missing asset draws the
                // placeholder so the hole is visible, not a silent skip.
                let frame = self
                    .images
                    .sample(src, 0, 0.0)
                    .unwrap_or_else(|| placeholder_frame(rect.w as u32, rect.h as u32));
                self.draw_cover(dst, &frame, rect, placed.focal);
            }
            ElementKind::Board => {
                fill_rounded(dst, rect, PANEL);
                // Children draw into the same surface — their rects are
                // already relative to this board's origin.
                for child in &placed.children {
                    self.draw_placed(dst, child, (rect.x, rect.y));
                }
            }
            ElementKind::Text { content } => {
                if let Some(text) = self.resolve_text(content) {
                    self.text.draw(
                        dst,
                        &rect,
                        &[RichSpan::plain(text, Color::WHITE)],
                        TEXT_SIZE_PX,
                        placed.opacity,
                    );
                }
            }
            ElementKind::Captions { source, .. } => {
                let spans = self.caption_spans(source, placed);
                if !spans.is_empty() {
                    self.text
                        .draw(dst, &rect, &spans, CAPTION_SIZE_PX, placed.opacity);
                }
            }
            ElementKind::Program { src, with } => {
                match self.programs.ops(
                    placed.element as *const _ as usize,
                    src,
                    placed.local_frame,
                    with.as_deref().unwrap_or("{}"),
                    rect.w,
                    rect.h,
                ) {
                    Some(list) => self.draw_ops(dst, &list, rect),
                    None => {
                        let frame = placeholder_frame(rect.w as u32, rect.h as u32);
                        self.draw_cover(dst, &frame, rect, placed.focal);
                    }
                }
            }
            ElementKind::Music { .. } | ElementKind::Sound { .. } => {}
        }
    }

    /// Replay a program's DrawList into dst. Ops are element-local —
    /// `rect` carries the box they were generated against.
    fn draw_ops(&mut self, dst: &mut Pixmap, list: &DrawList, rect: Rect) {
        for op in &list.0 {
            match op {
                DrawOp::Rect { x, y, w, h, .. } => {
                    let color = op.color();
                    if let Some(r) = tiny_skia::Rect::from_xywh(
                        (rect.x + x) as f32,
                        (rect.y + y) as f32,
                        *w as f32,
                        *h as f32,
                    ) {
                        let paint = Paint {
                            shader: Shader::SolidColor(SkColor::from_rgba8(
                                color.r, color.g, color.b, color.a,
                            )),
                            ..Default::default()
                        };
                        dst.fill_rect(r, &paint, SkTransform::default(), None);
                    }
                }
                DrawOp::Circle { x, y, r, .. } => {
                    let color = op.color();
                    if *r > 0.0
                        && let Some(circle) = tiny_skia::Rect::from_xywh(
                            (rect.x + x - r) as f32,
                            (rect.y + y - r) as f32,
                            (r * 2.0) as f32,
                            (r * 2.0) as f32,
                        )
                        && let Some(path) = PathBuilder::from_oval(circle)
                    {
                        let paint = Paint {
                            shader: Shader::SolidColor(SkColor::from_rgba8(
                                color.r, color.g, color.b, color.a,
                            )),
                            ..Default::default()
                        };
                        dst.fill_path(
                            &path,
                            &paint,
                            FillRule::Winding,
                            SkTransform::default(),
                            None,
                        );
                    }
                }
                DrawOp::Text { t, x, y, size, .. } => {
                    let line = Rect {
                        x: rect.x + x,
                        y: rect.y + y,
                        w: (rect.x + rect.w - (rect.x + x)).max(1.0),
                        h: size * 1.5,
                    };
                    self.text.draw(
                        dst,
                        &line,
                        &[RichSpan::plain(t.clone(), op.color())],
                        *size,
                        1.0,
                    );
                }
            }
        }
    }

    /// `Fit::Cover`: scale the frame to cover `rect` (local coords),
    /// positioned by `focal`. Layer bounds do the crop.
    fn draw_cover(&mut self, dst: &mut Pixmap, frame: &Frame, rect: Rect, focal: Placement) {
        let (fw, fh) = (frame.width as f64, frame.height as f64);
        if fw <= 0.0 || fh <= 0.0 || rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }
        let scale = (rect.w / fw).max(rect.h / fh);
        let (sw, sh) = (fw * scale, fh * scale);
        let (fx, fy) = match focal {
            Placement::Point { x, y } => (
                (x / f64::from(self.scene.canvas.width)).clamp(0.0, 1.0),
                (y / f64::from(self.scene.canvas.height)).clamp(0.0, 1.0),
            ),
            Placement::Named(NamedPlacement::Top) => (0.5, 0.0),
            Placement::Named(NamedPlacement::Bottom) => (0.5, 1.0),
            Placement::Named(NamedPlacement::Left) => (0.0, 0.5),
            Placement::Named(NamedPlacement::Right) => (1.0, 0.5),
            Placement::Named(NamedPlacement::Center) => (0.5, 0.5),
        };
        let dx = rect.x + (rect.w - sw) * fx;
        let dy = rect.y + (rect.h - sh) * fy;

        let Some(size) = tiny_skia::IntSize::from_wh(frame.width, frame.height) else {
            return;
        };
        // Frame sources deliver straight-alpha RGBA (ffmpeg `-pix_fmt
        // rgba`, `image::to_rgba8`); tiny-skia pixmaps are premultiplied.
        // Without this a half-transparent pixel composites at full
        // brightness — straight (255,0,0,128) must arrive as (128,0,0,128).
        let mut pixels = frame.pixels.clone();
        premultiply(&mut pixels);
        let Some(src) = Pixmap::from_vec(pixels, size) else {
            return;
        };
        let paint = PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        };
        let transform =
            SkTransform::from_row(scale as f32, 0.0, 0.0, scale as f32, dx as f32, dy as f32);
        dst.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
    }

    /// Text content: literal or `bind="line.text"` → the script line's text.
    fn resolve_text(&self, content: &TextContent) -> Option<String> {
        match content {
            TextContent::Literal(text) => Some(text.clone()),
            TextContent::Bind(path) => match path.property {
                BindProp::Text => self
                    .scene
                    .script
                    .as_ref()?
                    .lines
                    .iter()
                    .find(|l| l.id == path.line)
                    .map(|l| l.text.clone()),
            },
        }
    }

    /// Caption content at this instant: spoken words accented, the rest
    /// white. `Granularity::Line` shows the whole line with no highlight.
    /// During a gap the previous line holds, fully spoken.
    fn caption_spans(&self, source: &scene_ir::AnchorRef, placed: &PlacedElement) -> Vec<RichSpan> {
        let Some(timing) = self.timings.get(&source.source) else {
            return Vec::new();
        };
        let t_s = placed.element.timing.start_s
            + placed.local_frame as f64 / self.scene.frame_rate.to_f64();
        let Some((line, active)) = active_line(timing, t_s) else {
            return Vec::new();
        };
        if matches!(source.granularity, scene_ir::Granularity::Line) {
            return vec![RichSpan::plain(
                line.words
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                Color::WHITE,
            )];
        }
        let mut spoken = String::new();
        let mut upcoming = String::new();
        for (i, word) in line.words.iter().enumerate() {
            let target = if i <= active {
                &mut spoken
            } else {
                &mut upcoming
            };
            if !target.is_empty() {
                target.push(' ');
            }
            target.push_str(&word.text);
        }
        let mut spans = vec![RichSpan {
            text: spoken,
            color: ACCENT,
            weight: 700,
        }];
        if !upcoming.is_empty() {
            spans[0].text.push(' ');
            spans.push(RichSpan::plain(upcoming, Color::WHITE));
        }
        spans
    }
}

/// The line containing `t_s` and the index of its live word. Between
/// lines the most recent finished line holds, fully spoken; before the
/// first word there is no caption.
pub fn active_line(timing: &TimingSource, t_s: f64) -> Option<(&AlignedLine, usize)> {
    let mut previous: Option<&AlignedLine> = None;
    for line in &timing.lines {
        let (Some(start), Some(end)) = (line.start_s(), line.end_s()) else {
            continue;
        };
        if t_s < start {
            break;
        }
        if t_s <= end {
            let active = line
                .words
                .iter()
                .position(|w| t_s < w.end_s)
                .unwrap_or(line.words.len() - 1);
            return Some((line, active));
        }
        previous = Some(line);
    }
    let line = previous?;
    Some((line, line.words.len() - 1))
}

/// A rounded-rect path — tiny-skia-path has no helper, so four lines
/// plus quadratic corners it is.
fn rounded_rect_path(rect: tiny_skia::Rect, radius: f32) -> Option<tiny_skia::Path> {
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let radius = radius.min((r - l) / 2.0).min((b - t) / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(l + radius, t);
    pb.line_to(r - radius, t);
    pb.quad_to(r, t, r, t + radius);
    pb.line_to(r, b - radius);
    pb.quad_to(r, b, r - radius, b);
    pb.line_to(l + radius, b);
    pb.quad_to(l, b, l, b - radius);
    pb.line_to(l, t + radius);
    pb.quad_to(l, t, l + radius, t);
    pb.close();
    pb.finish()
}

/// Straight-alpha RGBA → premultiplied (round-to-nearest). ffmpeg's
/// `-pix_fmt rgba` and `image::to_rgba8` both deliver straight alpha;
/// tiny-skia's pixmap contract is premultiplied.
fn premultiply(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        px[0] = ((u32::from(px[0]) * a + 127) / 255) as u8;
        px[1] = ((u32::from(px[1]) * a + 127) / 255) as u8;
        px[2] = ((u32::from(px[2]) * a + 127) / 255) as u8;
    }
}

fn fill_rounded(pixmap: &mut Pixmap, rect: Rect, color: Color) {
    let Some(sk_rect) = tiny_skia::Rect::from_ltrb(
        rect.x as f32,
        rect.y as f32,
        (rect.x + rect.w) as f32,
        (rect.y + rect.h) as f32,
    ) else {
        return;
    };
    let Some(path) = rounded_rect_path(sk_rect, PANEL_RADIUS) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(SkColor::from_rgba8(color.r, color.g, color.b, color.a));
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        SkTransform::identity(),
        None,
    );
}
