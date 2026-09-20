//! Shaped text via cosmic-text: measurement for layout, glyph blending
//! for raster. One `FontSystem` per worker — font metrics are part of a
//! worker's deterministic context, so workers never share one.

use cosmic_text::{
    Align, Attrs, Buffer, Color as CosmicColor, Family, FontSystem, Metrics, Shaping, SwashCache,
    Weight, Wrap,
};
use tiny_skia::Pixmap;

use scene_ir::Color;
use scene_layout::{Rect, Size};

/// One styled run inside a text block — karaoke captions are three runs:
/// spoken words, the live word, upcoming words.
#[derive(Debug, Clone, PartialEq)]
pub struct RichSpan {
    pub text: String,
    pub color: Color,
    /// Font weight (400 regular, 700 bold).
    pub weight: u16,
}

impl RichSpan {
    pub fn plain(text: impl Into<String>, color: Color) -> Self {
        RichSpan {
            text: text.into(),
            color,
            weight: 400,
        }
    }
}

/// What the rasterizer needs from a text engine. Kept a trait so tests
/// can substitute a stub and the worker pool can own one per thread.
pub trait TextEngine {
    /// Laid-out size of `text` at `size_px`, wrapped to `wrap_width`.
    fn measure(&mut self, text: &str, size_px: f64, wrap_width: f64) -> Size;

    /// Blend `spans` onto `pixmap`, centred horizontally and vertically
    /// inside `rect`, wrapped to `rect.w`, line height `1.25 × size_px`.
    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        rect: &Rect,
        spans: &[RichSpan],
        size_px: f64,
        opacity: f64,
    );
}

/// cosmic-text-backed engine. `new()` loads the system font database —
/// deterministic per machine, which is the unit determinism is defined
/// over (workers on one machine share one fontdb outcome).
pub struct CosmicText {
    fonts: FontSystem,
    cache: SwashCache,
}

impl Default for CosmicText {
    fn default() -> Self {
        Self::new()
    }
}

impl CosmicText {
    pub fn new() -> Self {
        CosmicText {
            fonts: FontSystem::new(),
            cache: SwashCache::new(),
        }
    }

    /// Shape `spans` into a buffer wrapped to `width`.
    fn buffer(&mut self, spans: &[RichSpan], size_px: f64, wrap_width: f64) -> Buffer {
        let metrics = Metrics::new(size_px as f32, size_px as f32 * 1.25);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        buffer.set_size(&mut self.fonts, Some(wrap_width as f32), None);
        buffer.set_wrap(&mut self.fonts, Wrap::WordOrGlyph);
        let rich: Vec<(&str, Attrs)> = spans
            .iter()
            .map(|s| {
                (
                    s.text.as_str(),
                    Attrs::new()
                        .family(Family::SansSerif)
                        .weight(Weight(s.weight))
                        .color(CosmicColor::rgba(
                            s.color.r, s.color.g, s.color.b, s.color.a,
                        )),
                )
            })
            .collect();
        buffer.set_rich_text(
            &mut self.fonts,
            rich,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            Some(Align::Center),
        );
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }

    fn laid_out_size(buffer: &Buffer) -> Size {
        let mut w = 0.0_f64;
        let mut h = 0.0_f64;
        for run in buffer.layout_runs() {
            w = w.max(f64::from(run.line_w));
            h += f64::from(run.line_height);
        }
        Size { w, h }
    }
}

impl TextEngine for CosmicText {
    fn measure(&mut self, text: &str, size_px: f64, wrap_width: f64) -> Size {
        let buffer = self.buffer(&[RichSpan::plain(text, Color::WHITE)], size_px, wrap_width);
        Self::laid_out_size(&buffer)
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        rect: &Rect,
        spans: &[RichSpan],
        size_px: f64,
        opacity: f64,
    ) {
        if spans.iter().all(|s| s.text.is_empty()) || rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }
        let buffer = self.buffer(spans, size_px, rect.w);
        let text_size = Self::laid_out_size(&buffer);
        // Centre the shaped block inside the rect.
        let ox = rect.x + (rect.w - text_size.w).max(0.0) / 2.0;
        let oy = rect.y + (rect.h - text_size.h).max(0.0) / 2.0;
        let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
        let data = pixmap.data_mut();
        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            CosmicColor::rgba(255, 255, 255, 255),
            |x, y, w, h, color| {
                // The callback reports the span's authored color.
                let (px, py) = (x + ox as i32, y + oy as i32);
                for row in py.max(0)..(py + h as i32).min(ph) {
                    for col in px.max(0)..(px + w as i32).min(pw) {
                        let i = ((row * pw + col) * 4) as usize;
                        let a = f64::from(color.a()) / 255.0 * opacity;
                        if a <= 0.0 {
                            continue;
                        }
                        // src-over onto premultiplied destination; the
                        // alpha channel blends toward fully opaque.
                        data[i] = blend(data[i], color.r(), a);
                        data[i + 1] = blend(data[i + 1], color.g(), a);
                        data[i + 2] = blend(data[i + 2], color.b(), a);
                        data[i + 3] = blend(data[i + 3], 255, a);
                    }
                }
            },
        );
    }
}

/// dst (premultiplied) + src·a — src-over for a premultiplied surface.
fn blend(dst: u8, src: u8, a: f64) -> u8 {
    (f64::from(src) * a + f64::from(dst) * (1.0 - a))
        .round()
        .clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real shaping path: measure + draw with the system fontdb. A
    /// fontless host gets a trivial pass — what this pins is "no panic
    /// and, where fonts exist, ink actually lands".
    #[test]
    fn cosmic_text_draws_ink() {
        let mut text = CosmicText::new();
        let size = text.measure("Nobody talks", 48.0, 800.0);
        if size.is_empty() {
            return; // no fonts on this host
        }
        assert!(size.w > 0.0 && size.h > 0.0);
        let mut pixmap = Pixmap::new(400, 100).unwrap();
        text.draw(
            &mut pixmap,
            &Rect {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 100.0,
            },
            &[RichSpan::plain(
                "Nobody talks",
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
            )],
            48.0,
            1.0,
        );
        assert!(pixmap.data().as_chunks::<4>().0.iter().any(|px| px[3] > 0));
    }

    #[test]
    fn draw_is_deterministic_per_engine() {
        let mut a = CosmicText::new();
        let mut b = CosmicText::new();
        let rect = Rect {
            x: 10.0,
            y: 10.0,
            w: 300.0,
            h: 80.0,
        };
        let spans = [RichSpan::plain(
            "the third rule",
            Color {
                r: 255,
                g: 255,
                b: 255,
                a: 255,
            },
        )];
        if a.measure("x", 48.0, 800.0).is_empty() {
            return;
        }
        let mut pa = Pixmap::new(320, 100).unwrap();
        let mut pb = Pixmap::new(320, 100).unwrap();
        a.draw(&mut pa, &rect, &spans, 48.0, 1.0);
        b.draw(&mut pb, &rect, &spans, 48.0, 1.0);
        assert_eq!(pa.data(), pb.data());
    }
}
