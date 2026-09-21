//! scene-layout — resolved elements → positioned boxes for one frame.
//!
//! Pure geometry, no graphics. `Measure` is the seam where the renderer
//! injects real content metrics (text extents, media sizes); tests inject
//! a stub. What comes out is the draw list: every element live at that
//! frame, in document order, with its canvas-space rect, entrance
//! transform, and opacity.
//!
//! Sizing rules:
//! - `clip`/`image` — cover the whole canvas; `at` is the crop focal.
//! - `text`/`captions`/`board` — content-sized via `Measure`, placed at
//!   `at` (default: captions bottom, others centre).
//! - `music`/`sound` — no visual; never placed.
//! - board children — vertical stack inside the board rect, centred
//!   horizontally; their `at` does not apply inside a stack.

mod anim;
mod ease;
mod geom;

use scene_ir::{ElementKind, NamedPlacement, Placement, TrackKind};
use scene_time::{ResolvedElement, ResolvedScene};

pub use geom::{Fit, Rect, Size, Transform};

/// Margin between a named-edge placement and the canvas edge.
pub const EDGE_MARGIN: f64 = 48.0;
/// Board inner padding and the gap between stacked children.
const BOARD_PAD: f64 = 24.0;
const BOARD_GAP: f64 = 16.0;

/// The seam for content metrics. The renderer supplies real measurement;
/// returning `None` means "cannot place this" — the element is dropped
/// from the draw list rather than guessed at. `local_s` is the element's
/// position inside its own span, in seconds — caption-style elements
/// whose content changes over time measure the content live at `local_s`.
pub trait Measure {
    fn measure(&mut self, element: &ResolvedElement, local_s: f64) -> Option<Size>;
}

/// One drawable element for one frame.
#[derive(Debug)]
pub struct PlacedElement<'a> {
    pub element: &'a ResolvedElement,
    /// Canvas-space box before `transform`.
    pub rect: Rect,
    pub fit: Fit,
    /// Crop focal for `Fit::Cover`; the authored `at` otherwise applied.
    pub focal: Placement,
    pub transform: Transform,
    /// 0..1 — multiplies the element's own opacity (entrance anims).
    pub opacity: f64,
    /// Frames into the element's own span (anim/progress input).
    pub local_frame: u32,
    /// Emission order across the whole draw list.
    pub z: usize,
    /// Board children — rects relative to this element's rect origin.
    pub children: Vec<PlacedElement<'a>>,
}

/// Default placement when an element carries no `at`.
fn default_placement(kind: &ElementKind) -> Placement {
    match kind {
        ElementKind::Captions { .. } => Placement::Named(NamedPlacement::Bottom),
        _ => Placement::Named(NamedPlacement::Center),
    }
}

/// The box a `size` occupies under `placement` on a `w`×`h` canvas.
fn place(size: Size, placement: Placement, canvas_w: f64, canvas_h: f64) -> Rect {
    match placement {
        Placement::Point { x, y } => Rect {
            x: x - size.w / 2.0,
            y: y - size.h / 2.0,
            w: size.w,
            h: size.h,
        },
        Placement::Named(NamedPlacement::Center) => Rect {
            x: (canvas_w - size.w) / 2.0,
            y: (canvas_h - size.h) / 2.0,
            w: size.w,
            h: size.h,
        },
        Placement::Named(NamedPlacement::Top) => Rect {
            x: (canvas_w - size.w) / 2.0,
            y: EDGE_MARGIN,
            w: size.w,
            h: size.h,
        },
        Placement::Named(NamedPlacement::Bottom) => Rect {
            x: (canvas_w - size.w) / 2.0,
            y: canvas_h - EDGE_MARGIN - size.h,
            w: size.w,
            h: size.h,
        },
        Placement::Named(NamedPlacement::Left) => Rect {
            x: EDGE_MARGIN,
            y: (canvas_h - size.h) / 2.0,
            w: size.w,
            h: size.h,
        },
        Placement::Named(NamedPlacement::Right) => Rect {
            x: canvas_w - EDGE_MARGIN - size.w,
            y: (canvas_h - size.h) / 2.0,
            w: size.w,
            h: size.h,
        },
    }
}

struct Placer<'m> {
    measure: &'m mut dyn Measure,
    canvas_w: f64,
    canvas_h: f64,
    fps: f64,
    z: usize,
}

impl<'m> Placer<'m> {
    fn next_z(&mut self) -> usize {
        let z = self.z;
        self.z += 1;
        z
    }

    /// Place every element of `elements` live at `frame`, appending to `out`.
    fn place_all<'a>(
        &mut self,
        elements: &'a [ResolvedElement],
        frame: u32,
        out: &mut Vec<PlacedElement<'a>>,
    ) {
        for element in elements {
            let range = element.timing.frames;
            if frame < range.start || frame >= range.end {
                continue;
            }
            let local_frame = frame - range.start;
            if let Some(placed) = self.place_one(element, local_frame, frame) {
                out.push(placed);
            }
        }
    }

    /// Content size of a stack member. Boards size themselves from their
    /// *own* children's stack — `Measure` only knows leaf content
    /// (text/captions); a nested board measured through it would return
    /// `None` and silently drop out of the layout.
    fn stack_size(&mut self, element: &ResolvedElement, frame: u32) -> Option<Size> {
        let range = element.timing.frames;
        let local_s = f64::from(frame - range.start) / self.fps;
        if matches!(element.kind, ElementKind::Board) {
            // Children extent first; `Measure` is the empty-board fallback
            // (a styled panel), matching `place_one`'s board arm.
            self.board_size(element, frame)
                .or_else(|| self.measure.measure(element, local_s))
        } else {
            self.measure.measure(element, local_s)
        }
    }

    /// A board's content size: the stacked extent of its live, measurable
    /// children. `None` when nothing counts — the caller falls back to
    /// `Measure` for an empty styled panel.
    fn board_size(&mut self, element: &ResolvedElement, frame: u32) -> Option<Size> {
        let mut w = 0.0_f64;
        let mut h = 0.0_f64;
        let mut counted = 0usize;
        for c in &element.children {
            let range = c.timing.frames;
            if frame < range.start || frame >= range.end {
                continue;
            }
            if let Some(size) = self.stack_size(c, frame).filter(|s| !s.is_empty()) {
                w = w.max(size.w);
                h += size.h;
                counted += 1;
            }
        }
        (counted > 0).then(|| Size {
            w: w + BOARD_PAD * 2.0,
            h: h + BOARD_PAD * 2.0 + BOARD_GAP * (counted - 1) as f64,
        })
    }

    /// Place one board child inside the stack: centred horizontally at
    /// `cursor_y`, rect relative to the board origin. Nested boards
    /// recurse — their children stack inside the nested rect.
    fn place_stack_child<'a>(
        &mut self,
        child: &'a ResolvedElement,
        frame: u32,
        board_w: f64,
        cursor_y: f64,
        size: Size,
    ) -> PlacedElement<'a> {
        let range = child.timing.frames;
        let child_local = frame - range.start;
        let (opacity, transform) = child.anim.map_or((1.0, Transform::IDENTITY), |kind| {
            anim::evaluate(kind, child_local, range.len())
        });
        let rect = Rect {
            x: (board_w - size.w) / 2.0,
            y: cursor_y,
            w: size.w,
            h: size.h,
        };
        let mut children = Vec::new();
        if matches!(child.kind, ElementKind::Board) {
            let mut cy = BOARD_PAD;
            for gc in &child.children {
                let grange = gc.timing.frames;
                if frame < grange.start || frame >= grange.end {
                    continue;
                }
                let Some(gsize) = self.stack_size(gc, frame).filter(|s| !s.is_empty()) else {
                    continue;
                };
                children.push(self.place_stack_child(gc, frame, rect.w, cy, gsize));
                cy += gsize.h + BOARD_GAP;
            }
        }
        PlacedElement {
            element: child,
            rect,
            fit: Fit::Content,
            focal: child
                .placement
                .unwrap_or(Placement::Named(NamedPlacement::Center)),
            transform,
            opacity,
            local_frame: child_local,
            z: self.next_z(),
            children,
        }
    }

    fn place_one<'a>(
        &mut self,
        element: &'a ResolvedElement,
        local_frame: u32,
        frame: u32,
    ) -> Option<PlacedElement<'a>> {
        // Measure children once — the board's own size and each child's
        // rect both derive from the same measurements. Only children live
        // at this frame count toward the stack. Nested boards measure
        // through `stack_size`, not the flat `Measure`.
        let child_sizes: Vec<Option<Size>> = if matches!(element.kind, ElementKind::Board) {
            element
                .children
                .iter()
                .map(|c| {
                    let range = c.timing.frames;
                    if frame >= range.start && frame < range.end {
                        self.stack_size(c, frame)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        let (rect, fit) = match &element.kind {
            // Footage covers the canvas; `at` is the crop focal.
            ElementKind::Clip { .. } | ElementKind::Image { .. } => (
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: self.canvas_w,
                    h: self.canvas_h,
                },
                Fit::Cover,
            ),
            // Audio elements draw nothing.
            ElementKind::Music { .. } | ElementKind::Sound { .. } => return None,
            // Programs draw in canvas space: ops are element-local and
            // `ctx.w`/`ctx.h` report this rect. No intrinsic content to
            // measure — the box is the canvas, `at` centers it on a point.
            ElementKind::Program { .. } => (
                place(
                    Size {
                        w: self.canvas_w,
                        h: self.canvas_h,
                    },
                    element
                        .placement
                        .unwrap_or(Placement::Named(NamedPlacement::Center)),
                    self.canvas_w,
                    self.canvas_h,
                ),
                Fit::Content,
            ),
            ElementKind::Board => {
                // Board size = its children's stack extent; falls back to
                // `Measure` for an empty board (a styled panel the
                // renderer knows how to size).
                let local_s = f64::from(local_frame) / self.fps;
                let size = if child_sizes.is_empty() {
                    self.measure
                        .measure(element, local_s)
                        .filter(|s| !s.is_empty())
                } else {
                    let mut w = 0.0_f64;
                    let mut h = 0.0_f64;
                    let mut counted = 0usize;
                    for size in child_sizes.iter().flatten() {
                        w = w.max(size.w);
                        h += size.h;
                        counted += 1;
                    }
                    (counted > 0).then(|| Size {
                        w: w + BOARD_PAD * 2.0,
                        h: h + BOARD_PAD * 2.0 + BOARD_GAP * (counted - 1) as f64,
                    })
                }?;
                (
                    place(
                        size,
                        element
                            .placement
                            .unwrap_or(Placement::Named(NamedPlacement::Center)),
                        self.canvas_w,
                        self.canvas_h,
                    ),
                    Fit::Content,
                )
            }
            _ => {
                let size = self
                    .measure
                    .measure(element, f64::from(local_frame) / self.fps)?;
                if size.is_empty() {
                    return None;
                }
                (
                    place(
                        size,
                        element
                            .placement
                            .unwrap_or_else(|| default_placement(&element.kind)),
                        self.canvas_w,
                        self.canvas_h,
                    ),
                    Fit::Content,
                )
            }
        };

        let (opacity, transform) = element.anim.map_or((1.0, Transform::IDENTITY), |kind| {
            anim::evaluate(kind, local_frame, element.timing.frames.len())
        });

        // Children live inside the board's rect: vertical stack, centred,
        // rects relative to the board origin. Their own anims still apply.
        // Nested boards recurse — grandchildren are placed, not dropped.
        let mut children = Vec::new();
        if matches!(element.kind, ElementKind::Board) {
            let mut cursor_y = BOARD_PAD;
            for (child, size) in element.children.iter().zip(&child_sizes) {
                // Non-live and unmeasurable children carry `None` already.
                let Some(size) = size.filter(|s| !s.is_empty()) else {
                    continue;
                };
                children.push(self.place_stack_child(child, frame, rect.w, cursor_y, size));
                cursor_y += size.h + BOARD_GAP;
            }
        }

        Some(PlacedElement {
            element,
            rect,
            fit,
            focal: element
                .placement
                .unwrap_or_else(|| default_placement(&element.kind)),
            transform,
            opacity,
            local_frame,
            z: self.next_z(),
            children,
        })
    }
}

/// The draw list for one frame, in document order: track order, then
/// element order, children nested under their board.
pub fn layout_frame<'a>(
    scene: &'a ResolvedScene,
    frame: u32,
    measure: &mut dyn Measure,
) -> Vec<PlacedElement<'a>> {
    let mut placer = Placer {
        measure,
        canvas_w: f64::from(scene.canvas.width),
        canvas_h: f64::from(scene.canvas.height),
        fps: scene.frame_rate.to_f64(),
        z: 0,
    };
    let mut out = Vec::new();
    for track in &scene.tracks {
        if track.kind == TrackKind::Audio {
            continue;
        }
        placer.place_all(&track.elements, frame, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use scene_ir::*;
    use scene_time::*;

    /// Stub measurer: text/captions → 200×50, boards → 100×100, rest → None.
    struct StubMeasure;
    impl Measure for StubMeasure {
        fn measure(&mut self, element: &ResolvedElement, _local_s: f64) -> Option<Size> {
            match &element.kind {
                ElementKind::Text { .. } | ElementKind::Captions { .. } => {
                    Some(Size { w: 200.0, h: 50.0 })
                }
                ElementKind::Board => Some(Size { w: 100.0, h: 100.0 }),
                _ => None,
            }
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

    fn element(
        kind: ElementKind,
        at: Option<Placement>,
        anim: Option<AnimKind>,
    ) -> ResolvedElement {
        ResolvedElement {
            id: None,
            kind,
            timing: timing(0, 90),
            placement: at,
            anim,
            children: Vec::new(),
            span: Span::new(0, 0),
        }
    }

    fn scene(elements: Vec<ResolvedElement>) -> ResolvedScene {
        ResolvedScene {
            canvas: Canvas {
                width: 1920,
                height: 1080,
            },
            frame_rate: Rational {
                numerator: 30,
                denominator: 1,
            },
            clear: Color::TRANSPARENT,
            script: None,
            program: timing(0, 90),
            tracks: vec![ResolvedTrack {
                id: "v".to_string(),
                kind: TrackKind::Visual,
                anchor: None,
                elements,
            }],
        }
    }

    #[test]
    fn clip_covers_canvas() {
        let s = scene(vec![element(
            ElementKind::Clip {
                src: "a.mp4".into(),
                from_s: 0.0,
            },
            Some(Placement::Named(NamedPlacement::Top)),
            None,
        )]);
        let list = layout_frame(&s, 10, &mut StubMeasure);
        assert_eq!(list.len(), 1);
        let placed = &list[0];
        assert_eq!(
            placed.rect,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0
            }
        );
        assert_eq!(placed.fit, Fit::Cover);
        assert_eq!(placed.focal, Placement::Named(NamedPlacement::Top));
    }

    #[test]
    fn named_placements() {
        let cases = [
            (
                NamedPlacement::Center,
                Rect {
                    x: 860.0,
                    y: 515.0,
                    w: 200.0,
                    h: 50.0,
                },
            ),
            (
                NamedPlacement::Top,
                Rect {
                    x: 860.0,
                    y: 48.0,
                    w: 200.0,
                    h: 50.0,
                },
            ),
            (
                NamedPlacement::Bottom,
                Rect {
                    x: 860.0,
                    y: 982.0,
                    w: 200.0,
                    h: 50.0,
                },
            ),
            (
                NamedPlacement::Left,
                Rect {
                    x: 48.0,
                    y: 515.0,
                    w: 200.0,
                    h: 50.0,
                },
            ),
            (
                NamedPlacement::Right,
                Rect {
                    x: 1672.0,
                    y: 515.0,
                    w: 200.0,
                    h: 50.0,
                },
            ),
        ];
        for (named, want) in cases {
            let s = scene(vec![element(
                ElementKind::Text {
                    content: TextContent::Literal("x".into()),
                },
                Some(Placement::Named(named)),
                None,
            )]);
            let list = layout_frame(&s, 0, &mut StubMeasure);
            assert_eq!(list[0].rect, want, "{named:?}");
        }
    }

    #[test]
    fn point_placement_centres_the_box() {
        let s = scene(vec![element(
            ElementKind::Text {
                content: TextContent::Literal("x".into()),
            },
            Some(Placement::Point { x: 400.0, y: 300.0 }),
            None,
        )]);
        let list = layout_frame(&s, 0, &mut StubMeasure);
        assert_eq!(
            list[0].rect,
            Rect {
                x: 300.0,
                y: 275.0,
                w: 200.0,
                h: 50.0
            }
        );
    }

    #[test]
    fn captions_default_to_bottom() {
        let s = scene(vec![element(
            ElementKind::Captions {
                style: CaptionStyle::Karaoke,
                source: AnchorRef {
                    source: "voice".into(),
                    granularity: Granularity::Word,
                },
            },
            None,
            None,
        )]);
        let list = layout_frame(&s, 0, &mut StubMeasure);
        assert_eq!(list[0].rect.y, 1080.0 - EDGE_MARGIN - 50.0);
    }

    #[test]
    fn element_outside_its_frames_is_absent() {
        let mut el = element(
            ElementKind::Text {
                content: TextContent::Literal("x".into()),
            },
            None,
            None,
        );
        el.timing = timing(30, 60);
        let s = scene(vec![el]);
        assert!(layout_frame(&s, 10, &mut StubMeasure).is_empty());
        assert_eq!(layout_frame(&s, 30, &mut StubMeasure).len(), 1);
        // end is exclusive
        assert!(layout_frame(&s, 60, &mut StubMeasure).is_empty());
    }

    #[test]
    fn audio_elements_are_never_placed() {
        let s = scene(vec![
            element(
                ElementKind::Music {
                    src: "bed.mp3".into(),
                    gain_db: -14.0,
                    duck: None,
                    from_s: 0.0,
                },
                None,
                None,
            ),
            element(
                ElementKind::Text {
                    content: TextContent::Literal("x".into()),
                },
                None,
                None,
            ),
        ]);
        let list = layout_frame(&s, 0, &mut StubMeasure);
        assert_eq!(list.len(), 1);
        assert!(matches!(list[0].element.kind, ElementKind::Text { .. }));
    }

    #[test]
    fn z_follows_document_order() {
        let s = scene(vec![
            element(
                ElementKind::Clip {
                    src: "a.mp4".into(),
                    from_s: 0.0,
                },
                None,
                None,
            ),
            element(
                ElementKind::Text {
                    content: TextContent::Literal("x".into()),
                },
                None,
                None,
            ),
        ]);
        let list = layout_frame(&s, 0, &mut StubMeasure);
        assert!(list[0].z < list[1].z);
    }

    #[test]
    fn board_stacks_children() {
        let mut board = element(ElementKind::Board, None, None);
        for _ in 0..2 {
            board.children.push(element(
                ElementKind::Text {
                    content: TextContent::Literal("x".into()),
                },
                None,
                None,
            ));
        }
        let s = scene(vec![board]);
        let list = layout_frame(&s, 0, &mut StubMeasure);
        let board = &list[0];
        // size = 2*pad + widest child; h = 2*pad + 2*50 + gap
        assert_eq!(board.rect.w, 24.0 * 2.0 + 200.0);
        assert_eq!(board.rect.h, 24.0 * 2.0 + 100.0 + 16.0);
        assert_eq!(board.children.len(), 2);
        // children relative to board origin, centred horizontally
        assert_eq!(board.children[0].rect.y, 24.0);
        assert_eq!(board.children[1].rect.y, 24.0 + 50.0 + 16.0);
        assert_eq!(board.children[0].rect.x, (board.rect.w - 200.0) / 2.0);
    }

    #[test]
    fn nested_boards_recurse() {
        // board > board > text — the inner board sizes from its own
        // stack (not Measure), and grandchildren are placed, not dropped.
        let mut inner = element(ElementKind::Board, None, None);
        inner.children.push(element(
            ElementKind::Text {
                content: TextContent::Literal("deep".into()),
            },
            None,
            None,
        ));
        let mut outer = element(ElementKind::Board, None, None);
        outer.children.push(inner);
        let s = scene(vec![outer]);

        let list = layout_frame(&s, 0, &mut StubMeasure);
        let outer = &list[0];
        // inner = 200+2*pad × 50+2*pad; outer = inner + 2*pad.
        let inner_w = 200.0 + 24.0 * 2.0;
        let inner_h = 50.0 + 24.0 * 2.0;
        assert_eq!(outer.rect.w, inner_w + 24.0 * 2.0);
        assert_eq!(outer.rect.h, inner_h + 24.0 * 2.0);
        assert_eq!(outer.children.len(), 1);
        let inner = &outer.children[0];
        assert!(matches!(inner.element.kind, ElementKind::Board));
        assert_eq!(inner.rect.w, inner_w);
        assert_eq!(inner.children.len(), 1, "grandchildren are placed");
        let text = &inner.children[0];
        assert!(matches!(text.element.kind, ElementKind::Text { .. }));
        // Grandchild sits at the inner board's pad offset, centred.
        assert_eq!(text.rect.y, 24.0);
        assert_eq!(text.rect.x, (inner_w - 200.0) / 2.0);
    }

    #[test]
    fn anim_entrance_then_identity() {
        let s = scene(vec![element(
            ElementKind::Text {
                content: TextContent::Literal("x".into()),
            },
            None,
            Some(AnimKind::Rise),
        )]);
        let early = layout_frame(&s, 0, &mut StubMeasure);
        assert_eq!(early[0].opacity, 0.0);
        assert!(early[0].transform.dy > 0.0);
        let settled = layout_frame(&s, 60, &mut StubMeasure);
        assert_eq!(settled[0].opacity, 1.0);
        assert!(settled[0].transform.is_identity());
    }

    #[test]
    fn unmeasurable_content_is_dropped() {
        struct NoMeasure;
        impl Measure for NoMeasure {
            fn measure(&mut self, _: &ResolvedElement, _: f64) -> Option<Size> {
                None
            }
        }
        let s = scene(vec![element(
            ElementKind::Text {
                content: TextContent::Literal("x".into()),
            },
            None,
            None,
        )]);
        assert!(layout_frame(&s, 0, &mut NoMeasure).is_empty());
        // ...but a clip still covers — it needs no measurement
        let s = scene(vec![element(
            ElementKind::Clip {
                src: "a.mp4".into(),
                from_s: 0.0,
            },
            None,
            None,
        )]);
        assert_eq!(layout_frame(&s, 0, &mut NoMeasure).len(), 1);
    }

    #[test]
    fn deterministic() {
        let mut board = element(
            ElementKind::Board,
            Some(Placement::Point { x: 300.0, y: 300.0 }),
            Some(AnimKind::Pop),
        );
        board.children.push(element(
            ElementKind::Text {
                content: TextContent::Literal("x".into()),
            },
            None,
            Some(AnimKind::Fade),
        ));
        let s = scene(vec![
            element(
                ElementKind::Clip {
                    src: "a.mp4".into(),
                    from_s: 0.0,
                },
                None,
                None,
            ),
            board,
        ]);
        for frame in [0, 7, 45, 89] {
            let a = layout_frame(&s, frame, &mut StubMeasure);
            let b = layout_frame(&s, frame, &mut StubMeasure);
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(&b) {
                assert_eq!(x.rect, y.rect);
                assert_eq!(x.opacity, y.opacity);
                assert_eq!(x.transform, y.transform);
            }
        }
    }
}
