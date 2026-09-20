//! Frame-parallel rendering: contiguous shards, one `Renderer` per
//! worker thread, frames streamed to the consumer in index order over
//! bounded per-shard channels — memory stays flat with duration.
//!
//! The determinism contract is the whole point: a frame is a pure
//! function of (scene, timings, frame index, worker-local resources), so
//! worker count can change throughput but never bytes. The tests pin it.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
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

/// Per-shard channel depth — how far a shard's worker may run ahead of
/// the consumer before its `send` blocks. Frame memory ceiling is
/// `workers × (SHARD_QUEUE + 1) + 1` frames (queued + the frame each
/// worker holds mid-render/send + the consumer's current frame): about
/// 340 MB at 8 workers and 1080×1920, independent of video duration.
const SHARD_QUEUE: usize = 4;

/// Render `frames` of `scene` across at most `workers` threads,
/// streaming each frame to `consume` in index order as soon as it is
/// ready. Returns the number of frames delivered.
///
/// Each worker owns a contiguous shard and pushes frames into a small
/// bounded channel; the caller drains shard 0, then shard 1, and so on —
/// disjoint ordered ranges make sequential draining ordered
/// concatenation, so no reorder buffer is needed. Encoding (or any other
/// consumer) overlaps rendering, and the first frame is consumed while
/// the last is still being drawn.
///
/// `consume` runs on the calling thread, so it may borrow `&mut` state
/// (an encoder) without `Send`/`'static` bounds. If it returns `Err`,
/// delivery stops: a cancel flag plus the dropped receivers release
/// every blocked `send`, all workers are joined, and the initiating
/// error propagates — frames already consumed are irreversible.
///
/// `make_renderer` runs inside each worker thread; `scene`/`timings` are
/// shared read-only across workers. A worker panic is caught and comes
/// back as `Err` — a raster bug must not kill the process. (Requires
/// unwinding: `panic = "abort"` would defeat `catch_unwind`.)
pub fn render_frames_into<'a>(
    scene: &'a ResolvedScene,
    timings: &'a TimingMap,
    frames: Range<u32>,
    workers: usize,
    make_renderer: &RendererFactory<'a>,
    mut consume: impl FnMut(RenderedFrame) -> Result<(), String>,
) -> Result<usize, String> {
    let total = frames.len();
    if total == 0 {
        return Ok(0);
    }
    let workers = workers.clamp(1, total);
    // Contiguous shards, first workers get the remainder one each.
    let base = total / workers;
    let extra = total % workers;
    // Script state is sequential: a program's `render()` may accumulate
    // across frames, so a worker that starts mid-range must first replay
    // the calls a continuous run would have made — otherwise output
    // changes with the worker count. The replay starts at frame *zero*,
    // not the window's start: a `--frames 90:120` window still carries
    // the state built over 0..90 (layout-only, no pixels).
    let has_programs = scene_has_programs(scene);

    // Cancellation has two halves: the flag makes workers stop *starting*
    // work, and dropping the receivers makes any worker blocked in `send`
    // fail out of it — a blocked sender never observes the flag.
    let cancel = Arc::new(AtomicBool::new(false));
    // Terminal reports — one per worker, tagged with its shard index, on
    // a separate channel so a failure isn't queued behind frame traffic.
    let (ctl_tx, ctl_rx) = mpsc::channel::<(usize, Result<(), String>)>();

    let mut consumed = 0usize;
    thread::scope(|scope| -> Result<(), String> {
        // Spawn every worker first so later shards warm up in parallel,
        // then drain — receiving must happen inside the scope: with
        // bounded channels, a receive-after-join deadlocks on `send`.
        let mut receivers = Vec::with_capacity(workers);
        let mut cursor = frames.start;
        for w in 0..workers {
            let len = base + usize::from(w < extra);
            let shard = cursor..cursor + len as u32;
            cursor = shard.end;
            let (tx, rx) = mpsc::sync_channel::<RenderedFrame>(SHARD_QUEUE);
            receivers.push(rx);
            let ctl_tx = ctl_tx.clone();
            let cancel = cancel.clone();
            scope.spawn(move || {
                // AssertUnwindSafe: the renderer is thread-local and
                // dropped during unwind; nothing escapes but the Err.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut renderer = make_renderer(scene, timings);
                    if has_programs && !cancel.load(Ordering::Relaxed) {
                        renderer.warm_programs(0..shard.start);
                    }
                    for index in shard {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        // A dropped receiver means the consumer failed —
                        // stop rather than render into a dead pipe.
                        if tx
                            .send(RenderedFrame {
                                index,
                                pixels: renderer.render(index),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }))
                .map_err(|payload| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string())
                });
                let _ = ctl_tx.send((w, result)); // caller may be gone already
            });
        }
        drop(ctl_tx);

        let bail = |e: String| -> String {
            cancel.store(true, Ordering::Relaxed);
            e
        };
        // Reports that arrived before their shard's turn — a later worker
        // can finish (or die) while an earlier shard is still draining.
        let mut pending: Vec<(usize, Result<(), String>)> = Vec::new();
        let mut expected = frames.start;
        for (w, rx) in receivers.iter().enumerate() {
            while let Ok(frame) = rx.recv() {
                if frame.index != expected {
                    return Err(bail(format!(
                        "internal: shard {w} delivered frame {} while expecting {expected}",
                        frame.index
                    )));
                }
                expected += 1;
                if let Err(e) = consume(frame) {
                    return Err(bail(e));
                }
                consumed += 1;
                while let Ok(report) = ctl_rx.try_recv() {
                    match report {
                        (who, Err(e)) => {
                            return Err(bail(format!("render worker {who} failed: {e}")));
                        }
                        ok => pending.push(ok),
                    }
                }
            }
            // The shard's channel closed ⇒ its worker exited ⇒ its
            // report is enqueued or in flight — collect until it arrives.
            // A *different* worker's failure found on the way still wins.
            let report = loop {
                if let Some(i) = pending.iter().position(|(who, _)| *who == w) {
                    break pending.remove(i).1;
                }
                match ctl_rx.recv() {
                    Ok((who, Err(e))) => {
                        return Err(bail(format!("render worker {who} failed: {e}")));
                    }
                    Ok(report) if report.0 == w => break report.1,
                    Ok(report) => pending.push(report),
                    Err(_) => {
                        return Err(bail(format!("render worker {w} exited without a report")));
                    }
                }
            };
            report.map_err(|e| bail(format!("render worker {w} failed: {e}")))?;
        }
        Ok(())
    })?;
    Ok(consumed)
}

/// Collect-everything wrapper over [`render_frames_into`] — holds every
/// frame in RAM (≈8 MB each at 1080×1920), so it suits tests and short
/// clips; production renders should stream via `render_frames_into`.
pub fn render_frames<'a>(
    scene: &'a ResolvedScene,
    timings: &'a TimingMap,
    frames: Range<u32>,
    workers: usize,
    make_renderer: &RendererFactory<'a>,
) -> Result<Vec<RenderedFrame>, String> {
    let mut out = Vec::with_capacity(frames.len());
    render_frames_into(scene, timings, frames, workers, make_renderer, |frame| {
        out.push(frame);
        Ok(())
    })?;
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
            for c in pixels.as_chunks_mut::<4>().0 {
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
            _element: usize,
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
            _element: usize,
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

    /// The review finding: `--frames 20:40` used to warm from *frame 20*,
    /// so a stateful program's window output skipped the state built over
    /// 0..20. The window must pixel-match the same frames of a full run.
    #[test]
    fn frame_windows_match_the_full_run_for_stateful_programs() {
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
        let full = render_frames(&scene, &timings, 0..40, 1, &counting).unwrap();
        // One worker *and* sharded — the prefix replay must land either way.
        let window_one = render_frames(&scene, &timings, 20..40, 1, &counting).unwrap();
        let window_two = render_frames(&scene, &timings, 20..40, 2, &counting).unwrap();
        for (i, w) in window_one.iter().enumerate() {
            assert_eq!(
                *w,
                full[20 + i],
                "window frame {} differs from the full run",
                w.index
            );
        }
        for (i, w) in window_two.iter().enumerate() {
            assert_eq!(
                *w,
                full[20 + i],
                "sharded window frame {} differs from the full run",
                w.index
            );
        }
    }

    /// Half-transparent red — straight alpha, the shape frame sources
    /// deliver (`ffmpeg -pix_fmt rgba`, `image::to_rgba8`).
    struct HalfRed;
    impl FrameSource for HalfRed {
        fn sample(&mut self, _src: &str, frame: u64, _fps: f64) -> Option<Frame> {
            let mut pixels = vec![0u8; 4 * 4 * 4];
            for c in pixels.as_chunks_mut::<4>().0 {
                c.copy_from_slice(&[255, 0, 0, 128]);
            }
            Some(Frame {
                index: frame,
                width: 4,
                height: 4,
                pixels,
            })
        }
    }

    #[test]
    fn straight_alpha_images_premultiply_before_compositing() {
        let mut scene = test_scene();
        // Opaque black clear: correct premultiply gives src-over of
        // (128,0,0,128) on black → ~128 red. Unconverted straight alpha
        // reads 255 as premultiplied → ~255 — twice as bright as legal.
        scene.clear = Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        };
        scene.tracks[0].elements = vec![ResolvedElement {
            id: None,
            kind: ElementKind::Image {
                src: "a.png".into(),
            },
            timing: timing(0, 90),
            placement: None,
            anim: None,
            children: Vec::new(),
            span: Span::new(0, 0),
        }];
        let timings = TimingMap::default();
        fn halfred<'a>(scene: &'a ResolvedScene, timings: &'a TimingMap) -> Renderer<'a> {
            let mut r = renderer(scene, timings);
            r.images = Box::new(HalfRed);
            r
        }
        let out = render_frames(&scene, &timings, 0..1, 1, &halfred).unwrap();
        let i = (90 * 320 + 160) * 4; // frame centre
        let (r, g, b) = (out[0].pixels[i], out[0].pixels[i + 1], out[0].pixels[i + 2]);
        assert!(
            (124..=132).contains(&r) && g < 4 && b < 4,
            "expected ~128 red (premultiplied blend), got ({r},{g},{b})"
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

    #[test]
    fn frames_arrive_in_index_order_across_shards() {
        let scene = test_scene();
        let timings = TimingMap::default();
        for workers in [1, 4] {
            let mut next = 5u32;
            let n = render_frames_into(&scene, &timings, 5..90, workers, &renderer, |f| {
                assert_eq!(f.index, next, "shards concatenate in order");
                next += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(n, 85);
            assert_eq!(next, 90);
        }
    }

    /// First shard stalls mid-delivery — later workers fill their bounded
    /// queues and block; the drain still completes in order.
    struct SlowStart;
    impl FrameSource for SlowStart {
        fn sample(&mut self, src: &str, frame: u64, fps: f64) -> Option<Frame> {
            if frame == 0 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            StubFrames.sample(src, frame, fps)
        }
    }

    #[test]
    fn stalled_first_shard_still_delivers_in_order() {
        let scene = test_scene();
        let timings = TimingMap::default();
        fn slow<'a>(scene: &'a ResolvedScene, timings: &'a TimingMap) -> Renderer<'a> {
            let mut r = renderer(scene, timings);
            r.clips = Box::new(SlowStart);
            r
        }
        let mut next = 0u32;
        render_frames_into(&scene, &timings, 0..30, 4, &slow, |f| {
            assert_eq!(f.index, next);
            next += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(next, 30);
    }

    #[test]
    fn consume_error_stops_delivery_and_releases_workers() {
        // Later workers fill their shard queues and block in `send`; a
        // consume failure must drop the receivers and join them — a
        // deadlock here hangs the test instead of failing it.
        let scene = test_scene();
        let timings = TimingMap::default();
        let mut delivered = 0;
        let err = render_frames_into(&scene, &timings, 0..90, 4, &renderer, |f| {
            delivered += 1;
            if f.index == 3 {
                return Err("encoder exploded".to_string());
            }
            Ok(())
        })
        .expect_err("consume error must propagate");
        assert_eq!(err, "encoder exploded");
        assert_eq!(delivered, 4);
    }

    #[test]
    fn consume_error_wins_over_a_concurrent_worker_failure() {
        // FragileFrames dies on frame 5; a consume failure at frame 3 is
        // the initiating error and must be the one reported.
        let scene = test_scene();
        let timings = TimingMap::default();
        fn fragile<'a>(scene: &'a ResolvedScene, timings: &'a TimingMap) -> Renderer<'a> {
            let mut r = renderer(scene, timings);
            r.clips = Box::new(FragileFrames);
            r
        }
        let err = render_frames_into(&scene, &timings, 0..30, 4, &fragile, |f| {
            if f.index == 3 {
                Err("encoder exploded".to_string())
            } else {
                Ok(())
            }
        })
        .expect_err("both paths fail — the consume error initiated");
        assert_eq!(err, "encoder exploded");
    }

    /// Panics inside `ops()` — mid-shard renders hit it directly and
    /// workers replaying the warm-up prefix hit it there.
    struct ExplodingPrograms;
    impl scene_script::ProgramSource for ExplodingPrograms {
        fn ops(
            &mut self,
            _element: usize,
            _src: &str,
            local_frame: u32,
            _with: &str,
            _w: f64,
            _h: f64,
        ) -> Option<scene_script::DrawList> {
            if local_frame == 3 {
                panic!("program exploded at frame 3");
            }
            let (list, _) = scene_script::DrawList::from_json(
                r##"[{"op":"rect","x":0,"y":0,"w":1,"h":1,"c":"#ff0000"}]"##,
            );
            Some(list)
        }
    }

    #[test]
    fn worker_panic_during_warmup_comes_back_as_an_error() {
        let mut scene = test_scene();
        scene.tracks[0].elements.push(ResolvedElement {
            id: None,
            kind: ElementKind::Program {
                src: "boom.js".into(),
                with: None,
            },
            timing: timing(0, 90),
            placement: None,
            anim: None,
            children: Vec::new(),
            span: Span::new(0, 0),
        });
        let timings = TimingMap::default();
        fn exploding<'a>(scene: &'a ResolvedScene, timings: &'a TimingMap) -> Renderer<'a> {
            let mut r = renderer(scene, timings);
            r.programs = Box::new(ExplodingPrograms);
            r
        }
        let err = render_frames_into(&scene, &timings, 0..30, 4, &exploding, |_| Ok(()))
            .expect_err("warm-up panic should surface as Err");
        assert!(err.contains("program exploded"), "panic preserved: {err}");
    }
}
