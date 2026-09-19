//! Frame-parallel rendering: contiguous shards, one `Renderer` per
//! worker thread, results collected per frame and emitted in order.
//!
//! The determinism contract is the whole point: a frame is a pure
//! function of (scene, timings, frame index, worker-local resources), so
//! worker count can change throughput but never bytes. The tests pin it.

use std::ops::Range;
use std::sync::mpsc;
use std::thread;

use scene_ir::ElementKind;
use scene_time::{ResolvedElement, ResolvedScene, TimingMap};

use crate::raster::Renderer;

/// Any `Program` element anywhere in the scene — the warm-up pass below
/// is skipped entirely for program-free scenes.
fn scene_has_programs(scene: &ResolvedScene) -> bool {
    fn any(elements: &[ResolvedElement]) -> bool {
        elements
            .iter()
            .any(|e| matches!(e.kind, ElementKind::Program { .. }) || any(&e.children))
    }
    scene.tracks.iter().any(|t| any(&t.elements))
}

/// One rendered frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFrame {
    pub index: u32,
    /// Premultiplied RGBA, `w*h*4` bytes.
    pub pixels: Vec<u8>,
}

/// Builds the per-worker `Renderer`. Called once per thread — this is
/// where font databases, decode streams, and measure state get their
/// own-thread instances.
pub type RendererFactory<'a> =
    dyn Fn(&'a ResolvedScene, &'a TimingMap) -> Renderer<'a> + Send + Sync;

/// Render `frames` of `scene`, at most `workers` threads. Returns every
/// frame in index order — sharding never reorders output.
///
/// `make_renderer` runs inside each worker thread; `scene`/`timings` are
/// shared read-only across workers. A worker panic is caught and comes
/// back as `Err` — a raster bug must not kill the process (it would
/// take `engine ui` down mid-request and lose every completed frame).
pub fn render_frames<'a>(
    scene: &'a ResolvedScene,
    timings: &'a TimingMap,
    frames: Range<u32>,
    workers: usize,
    make_renderer: &RendererFactory<'a>,
) -> Result<Vec<RenderedFrame>, String> {
    let total = frames.len();
    if total == 0 {
        return Ok(Vec::new());
    }
    let workers = workers.clamp(1, total);
    // Contiguous shards, first workers get the remainder one each.
    let base = total / workers;
    let extra = total % workers;
    let range_start = frames.start;
    // Script state is sequential: a program's `render()` may accumulate
    // across frames, so a worker that starts mid-range must first replay
    // the calls a continuous run would have made — otherwise output
    // changes with the worker count.
    let has_programs = scene_has_programs(scene);
    let (tx, rx) = mpsc::channel::<Result<Vec<RenderedFrame>, String>>();

    thread::scope(|scope| {
        let mut cursor = frames.start;
        for w in 0..workers {
            let len = base + usize::from(w < extra);
            let shard = cursor..cursor + len as u32;
            cursor = shard.end;
            let tx = tx.clone();
            scope.spawn(move || {
                // AssertUnwindSafe: the renderer is thread-local and
                // dropped during unwind; nothing escapes but the Err.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut renderer = make_renderer(scene, timings);
                    if has_programs {
                        renderer.warm_programs(range_start..shard.start);
                    }
                    let mut out = Vec::with_capacity(len);
                    for index in shard {
                        out.push(RenderedFrame {
                            index,
                            pixels: renderer.render(index),
                        });
                    }
                    out
                }))
                .map_err(|payload| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string())
                });
                let _ = tx.send(result); // rx may be gone if we failed early
            });
        }
        drop(tx);
    });

    let mut out = Vec::with_capacity(total);
    for shard in rx.iter() {
        out.extend(shard?);
    }
    out.sort_by_key(|f| f.index);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scene_ir::*;
    use scene_layout::{Measure, Size};
    use scene_media::Frame;
    use scene_time::*;

    use crate::media::FrameSource;
    use crate::text::{RichSpan, TextEngine};
    use scene_layout::Rect;
    use scene_time::ResolvedElement;
    use tiny_skia::Pixmap;

    /// A text engine that never needs a font: fixed metrics, deterministic
    /// block fill.
    struct StubText;
    impl TextEngine for StubText {
        fn measure(&mut self, text: &str, size_px: f64, _wrap: f64) -> Size {
            Size {
                w: (text.chars().count() as f64 * size_px * 0.5).clamp(1.0, 800.0),
                h: size_px * 1.25,
            }
        }
        fn draw(&mut self, pixmap: &mut Pixmap, rect: &Rect, spans: &[RichSpan], _: f64, a: f64) {
            // Fill the rect with the first span's color — enough to make
            // "was text drawn here" observable in bytes.
            let Some(span) = spans.first() else { return };
            let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
            let (x0, y0) = (rect.x as i32, rect.y as i32);
            let data = pixmap.data_mut();
            for y in y0.max(0)..(y0 + rect.h as i32).min(h) {
                for x in x0.max(0)..(x0 + rect.w as i32).min(w) {
                    let i = ((y * w + x) * 4) as usize;
                    data[i] = span.color.r;
                    data[i + 1] = span.color.g;
                    data[i + 2] = span.color.b;
                    data[i + 3] = (f64::from(span.color.a) * a).round() as u8;
                }
            }
        }
    }

    struct StubMeasure;
    impl Measure for StubMeasure {
        fn measure(&mut self, element: &ResolvedElement, _local_s: f64) -> Option<Size> {
            match &element.kind {
                ElementKind::Text { .. } => Some(Size { w: 200.0, h: 70.0 }),
                ElementKind::Captions { .. } => Some(Size { w: 400.0, h: 80.0 }),
                ElementKind::Board => Some(Size { w: 200.0, h: 100.0 }),
                _ => None,
            }
        }
    }

    /// Solid-color clip source, deterministic per src+frame.
    struct StubFrames;
    impl FrameSource for StubFrames {
        fn sample(&mut self, src: &str, frame: u64, _fps: f64) -> Option<Frame> {
            let seed = src.bytes().fold(0u8, |a, b| a.wrapping_add(b));
            let v = seed.wrapping_add((frame * 3) as u8);
            let mut pixels = vec![0u8; 8 * 8 * 4];
            for c in pixels.chunks_exact_mut(4) {
                c.copy_from_slice(&[v, v / 2, 255 - v, 255]);
            }
            Some(Frame {
                index: frame,
                width: 8,
                height: 8,
                pixels,
            })
        }
    }

    fn timing(start: u32, end: u32) -> ResolvedTiming {
        ResolvedTiming {
            start_s: start as f64 / 30.0,
            end_s: end as f64 / 30.0,
            frames: FrameRange { start, end },
            samples: SampleRange {
                start: u64::from(start) * 1600,
                end: u64::from(end) * 1600,
            },
        }
    }

    fn test_scene() -> ResolvedScene {
        let board = ResolvedElement {
            id: Some("card".into()),
            kind: ElementKind::Board,
            timing: timing(10, 80),
            placement: Some(Placement::Named(NamedPlacement::Center)),
            anim: Some(AnimKind::Rise),
            children: vec![ResolvedElement {
                id: None,
                kind: ElementKind::Text {
                    content: TextContent::Literal("hello scene".into()),
                },
                timing: timing(10, 80),
                placement: None,
                anim: None,
                children: Vec::new(),
                span: Span::new(0, 0),
            }],
            span: Span::new(0, 0),
        };
        ResolvedScene {
            canvas: Canvas {
                width: 320,
                height: 180,
            },
            frame_rate: Rational {
                numerator: 30,
                denominator: 1,
            },
            clear: Color {
                r: 8,
                g: 8,
                b: 12,
                a: 255,
            },
            script: None,
            program: timing(0, 90),
            tracks: vec![ResolvedTrack {
                id: "v".into(),
                kind: TrackKind::Visual,
                anchor: None,
                elements: vec![
                    ResolvedElement {
                        id: Some("bg".into()),
                        kind: ElementKind::Clip {
                            src: "a.mp4".into(),
                        },
                        timing: timing(0, 90),
                        placement: None,
                        anim: None,
                        children: Vec::new(),
                        span: Span::new(0, 0),
                    },
                    board,
                ],
            }],
        }
    }

    fn renderer<'a>(scene: &'a ResolvedScene, timings: &'a TimingMap) -> Renderer<'a> {
        let _ = timings;
        Renderer {
            scene,
            timings,
            measure: Box::new(StubMeasure),
            text: Box::new(StubText),
            clips: Box::new(StubFrames),
            images: Box::new(StubFrames),
            programs: Box::new(scene_script::NullPrograms),
        }
    }

    #[test]
    fn frames_come_back_in_order() {
        let scene = test_scene();
        let timings = TimingMap::default();
        let out = render_frames(&scene, &timings, 0..90, 4, &renderer).unwrap();
        assert_eq!(out.len(), 90);
        for (i, f) in out.iter().enumerate() {
            assert_eq!(f.index, i as u32);
            assert_eq!(f.pixels.len(), 320 * 180 * 4);
        }
    }

    #[test]
    fn worker_count_never_changes_bytes() {
        let scene = test_scene();
        let timings = TimingMap::default();
        let one = render_frames(&scene, &timings, 0..30, 1, &renderer).unwrap();
        let four = render_frames(&scene, &timings, 0..30, 4, &renderer).unwrap();
        let seven = render_frames(&scene, &timings, 0..30, 7, &renderer).unwrap();
        assert_eq!(one, four);
        assert_eq!(four, seven);
    }

    #[test]
    fn render_twice_is_byte_identical() {
        let scene = test_scene();
        let timings = TimingMap::default();
        let a = render_frames(&scene, &timings, 0..10, 2, &renderer).unwrap();
        let b = render_frames(&scene, &timings, 0..10, 2, &renderer).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn frames_differ_across_time() {
        // The stub clip source varies by frame; the rise anim varies too.
        let scene = test_scene();
        let timings = TimingMap::default();
        let out = render_frames(&scene, &timings, 0..30, 2, &renderer).unwrap();
        assert_ne!(out[0].pixels, out[15].pixels);
    }

    #[test]
    fn empty_range_and_more_workers_than_frames() {
        let scene = test_scene();
        let timings = TimingMap::default();
        assert!(
            render_frames(&scene, &timings, 0..0, 4, &renderer)
                .unwrap()
                .is_empty()
        );
        let out = render_frames(&scene, &timings, 0..3, 8, &renderer).unwrap();
        assert_eq!(out.len(), 3);
    }

    /// A `ProgramSource` that draws a fixed red rect in op-space —
    /// enough to observe "the program's ops reached the pixels".
    struct FixedPrograms;
    impl scene_script::ProgramSource for FixedPrograms {
        fn ops(
            &mut self,
            _src: &str,
            local_frame: u32,
            _with: &str,
            _w: f64,
            _h: f64,
        ) -> Option<scene_script::DrawList> {
            let (list, _) = scene_script::DrawList::from_json(&format!(
                r##"[{{"op":"rect","x":10,"y":10,"w":20,"h":{},"c":"#ff0000"}}]"##,
                local_frame + 10
            ));
            Some(list)
        }
    }

    #[test]
    fn program_ops_reach_the_pixels() {
        let mut scene = test_scene();
        scene.tracks[0].elements.push(ResolvedElement {
            id: None,
            kind: ElementKind::Program {
                src: "fx.js".into(),
                with: None,
            },
            timing: timing(0, 90),
            placement: None,
            anim: None,
            children: Vec::new(),
            span: Span::new(0, 0),
        });
        let timings = TimingMap::default();
        fn prog_renderer<'a>(scene: &'a ResolvedScene, timings: &'a TimingMap) -> Renderer<'a> {
            let mut r = renderer(scene, timings);
            r.programs = Box::new(FixedPrograms);
            r
        }
        let out = render_frames(&scene, &timings, 0..2, 2, &prog_renderer).unwrap();
        // Frame 0 draws rect(10,10,20,10) red over the stub clip; the clip
        // is colorful so "red exactly here" is the program's fingerprint.
        let px = |f: &RenderedFrame, x: usize, y: usize| {
            let i = (y * 320 + x) * 4;
            (f.pixels[i], f.pixels[i + 1], f.pixels[i + 2])
        };
        assert_eq!(px(&out[0], 15, 15), (255, 0, 0));
        // Frame 1's rect is taller — the op saw local_frame=1.
        assert_eq!(px(&out[1], 15, 20), (255, 0, 0));
        assert_ne!(px(&out[0], 15, 20), (255, 0, 0));
    }

    /// A *stateful* program: the drawn rect's width equals the number of
    /// `ops()` calls this worker has made — the fingerprint of script
    /// state accumulation. Without the shard-prefix warm-up, a worker
    /// starting mid-range would see a fresh counter and produce
    /// different pixels than a single-worker run.
    struct CountingPrograms {
        calls: u32,
    }
    impl scene_script::ProgramSource for CountingPrograms {
        fn ops(
            &mut self,
            _src: &str,
            _local_frame: u32,
            _with: &str,
            _w: f64,
            _h: f64,
        ) -> Option<scene_script::DrawList> {
            self.calls += 1;
            let (list, _) = scene_script::DrawList::from_json(&format!(
                r##"[{{"op":"rect","x":0,"y":0,"w":{},"h":10,"c":"#ff0000"}}]"##,
                self.calls
            ));
            Some(list)
        }
    }

    #[test]
    fn stateful_programs_are_worker_count_invariant() {
        let mut scene = test_scene();
        scene.tracks[0].elements.push(ResolvedElement {
            id: None,
            kind: ElementKind::Program {
                src: "counter.js".into(),
                with: None,
            },
            timing: timing(0, 90),
            placement: None,
            anim: None,
            children: Vec::new(),
            span: Span::new(0, 0),
        });
        let timings = TimingMap::default();
        fn counting<'a>(scene: &'a ResolvedScene, timings: &'a TimingMap) -> Renderer<'a> {
            let mut r = renderer(scene, timings);
            r.programs = Box::new(CountingPrograms { calls: 0 });
            r
        }
        let one = render_frames(&scene, &timings, 0..40, 1, &counting).unwrap();
        let four = render_frames(&scene, &timings, 0..40, 4, &counting).unwrap();
        assert_eq!(
            one, four,
            "stateful program output must not depend on worker count"
        );
    }

    /// A renderer that explodes on frame 5 — the pool must report it as
    /// `Err`, not propagate the panic and kill the process.
    struct FragileFrames;
    impl FrameSource for FragileFrames {
        fn sample(&mut self, _src: &str, frame: u64, _fps: f64) -> Option<Frame> {
            if frame == 5 {
                panic!("raster exploded at frame 5");
            }
            Some(Frame {
                index: frame,
                width: 8,
                height: 8,
                pixels: vec![0u8; 8 * 8 * 4],
            })
        }
    }

    #[test]
    fn worker_panic_comes_back_as_an_error() {
        let scene = test_scene();
        let timings = TimingMap::default();
        fn fragile<'a>(scene: &'a ResolvedScene, timings: &'a TimingMap) -> Renderer<'a> {
            let mut r = renderer(scene, timings);
            r.clips = Box::new(FragileFrames);
            r
        }
        let res = render_frames(&scene, &timings, 0..30, 4, &fragile);
        let err = res.expect_err("frame-5 panic should surface as Err");
        assert!(err.contains("frame 5"), "panic message preserved: {err}");
    }
}
