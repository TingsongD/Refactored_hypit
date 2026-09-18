//! scene-time — realization: symbolic anchors + measured audio → frames.
//!
//! Hermetic: no I/O, no rendering, no connectors. Input is a `Scene`
//! (from scene-ir) plus a `TimingMap` (what an alignment connector would
//! produce); output is a `ResolvedScene` where every element's timing is
//! a concrete `FrameRange` and `SampleRange`.

mod alignment;
mod domain;
mod resolve;

pub use alignment::{AlignedLine, TimingMap, TimingSource, Word};
pub use domain::{
    FrameRange, ResolvedElement, ResolvedScene, ResolvedTiming, ResolvedTrack, SampleRange,
};
pub use resolve::{SAMPLE_RATE, realize};

#[cfg(test)]
mod tests {
    use super::*;
    use scene_ir::*;

    fn w(text: &str, start_s: f64, end_s: f64) -> Word {
        Word {
            text: text.to_string(),
            start_s,
            end_s,
        }
    }

    fn line(cue: &str, words: Vec<Word>) -> AlignedLine {
        AlignedLine {
            cue: cue.to_string(),
            words,
        }
    }

    /// hook:  "Nobody talks about the"   @ 0.0–1.0 (4 words)
    /// payoff: "Compound interest is a treadmill" @ 1.5–3.0 (5 words)
    /// Lattice: [0.0, 0.3, 0.5, 0.8, 1.5, 1.9, 2.2, 2.4, 2.5, 3.0]
    fn voice_timing() -> TimingMap {
        let mut map = TimingMap::default();
        map.insert(
            "voice",
            TimingSource {
                lines: vec![
                    line(
                        "hook",
                        vec![
                            w("Nobody", 0.0, 0.3),
                            w("talks", 0.3, 0.5),
                            w("about", 0.5, 0.8),
                            w("the", 0.8, 1.0),
                        ],
                    ),
                    line(
                        "payoff",
                        vec![
                            w("Compound", 1.5, 1.9),
                            w("interest", 1.9, 2.2),
                            w("is", 2.2, 2.4),
                            w("a", 2.4, 2.5),
                            w("treadmill", 2.5, 3.0),
                        ],
                    ),
                ],
            },
        );
        map
    }

    /// A minimal scene: one script on `voice`, one visual track holding a
    /// single board element timed by `during` ("" = untimed).
    fn parse_scene(during: &str) -> Scene {
        let timing = if during.is_empty() {
            None
        } else {
            Some(AnchorRange::parse(during).unwrap())
        };
        Scene {
            canvas: Canvas {
                width: 1920,
                height: 1080,
            },
            frame_rate: Rational {
                numerator: 30,
                denominator: 1,
            },
            clear: Color::TRANSPARENT,
            script: Some(Script {
                track: "voice".to_string(),
                voice: None,
                lines: Vec::new(),
                span: Span::new(0, 0),
            }),
            tracks: vec![Track {
                id: "t".to_string(),
                kind: TrackKind::Visual,
                anchor: None,
                elements: vec![Element {
                    id: Some("el".to_string()),
                    kind: ElementKind::Board,
                    timing,
                    placement: None,
                    anim: None,
                    children: Vec::new(),
                    span: Span::new(0, 1),
                }],
                span: Span::new(0, 0),
            }],
            render: None,
        }
    }

    fn realize_one(during: &str) -> (ResolvedTiming, Vec<Diagnostic>) {
        let scene = parse_scene(during);
        let (resolved, diags) = realize(&scene, &voice_timing());
        let timing = resolved
            .and_then(|s| s.tracks.into_iter().next())
            .and_then(|t| t.elements.into_iter().next())
            .map(|e| e.timing)
            .unwrap_or(ResolvedTiming {
                start_s: 0.0,
                end_s: 0.0,
                frames: FrameRange { start: 0, end: 0 },
                samples: SampleRange { start: 0, end: 0 },
            });
        (timing, diags)
    }

    // --- cue edges ---------------------------------------------------------

    #[test]
    fn cue_start_and_end() {
        let (t, d) = realize_one("hook");
        assert!(d.is_empty());
        assert_eq!((t.start_s, t.end_s), (0.0, 1.0));
        assert_eq!(t.frames, FrameRange { start: 0, end: 30 });

        let (t, _) = realize_one("payoff");
        assert_eq!((t.start_s, t.end_s), (1.5, 3.0));
        assert_eq!(t.frames, FrameRange { start: 45, end: 90 });
    }

    #[test]
    fn cue_range() {
        let (t, d) = realize_one("hook..payoff");
        assert!(d.is_empty());
        assert_eq!((t.start_s, t.end_s), (0.0, 3.0));
        assert_eq!(t.frames, FrameRange { start: 0, end: 90 });
    }

    // --- offsets ------------------------------------------------------------

    #[test]
    fn second_offsets() {
        // A lone `hook+0.5s` is the hook's whole span shifted +0.5s.
        let (t, _) = realize_one("hook+0.5s");
        assert_eq!((t.start_s, t.end_s), (0.5, 1.5));

        let (t, _) = realize_one("payoff-0.5s..payoff");
        assert_eq!((t.start_s, t.end_s), (1.0, 3.0));
    }

    #[test]
    fn frame_offsets() {
        // +15f at 30fps = +0.5s
        let (t, _) = realize_one("hook+15f");
        assert_eq!(t.start_s, 0.5);

        // at 24fps the same +15f = 0.625s — the offset is frames, not seconds
        let mut scene = parse_scene("hook+15f");
        scene.frame_rate = Rational {
            numerator: 24,
            denominator: 1,
        };
        let (resolved, _) = realize(&scene, &voice_timing());
        let el = &resolved.unwrap().tracks[0].elements[0];
        assert!((el.timing.start_s - 0.625).abs() < 1e-9);
    }

    #[test]
    fn word_offsets_walk_the_lattice() {
        // hook starts at word 0 (0.0s); +2w lands on word 2's start (0.5s)
        let (t, _) = realize_one("hook+2w");
        assert_eq!(t.start_s, 0.5);
        // +4w walks past hook's end into payoff's first word (1.5s)
        let (t, _) = realize_one("hook+4w");
        assert_eq!(t.start_s, 1.5);
        // -2w from payoff's start (word 4) lands on word 2 (0.5s)
        let (t, _) = realize_one("payoff-2w");
        assert_eq!(t.start_s, 0.5);
    }

    #[test]
    fn word_offsets_clamp_at_stream_ends() {
        // +99w clamps to the final boundary (3.0s); end edge explicit so
        // the shifted-pair semantics don't collapse the range.
        let (t, _) = realize_one("hook+99w..5s");
        assert_eq!(t.start_s, 3.0);
        // -99w clamps to the first boundary (0.0s)
        let (t, _) = realize_one("payoff-99w..payoff");
        assert_eq!(t.start_s, 0.0);
    }

    // --- literals ------------------------------------------------------------

    #[test]
    fn literal_seconds_and_frames() {
        let (t, _) = realize_one("1.5s..4s");
        assert_eq!((t.start_s, t.end_s), (1.5, 4.0));
        assert_eq!(
            t.frames,
            FrameRange {
                start: 45,
                end: 120
            }
        );

        let (t, _) = realize_one("30f..90f");
        assert_eq!((t.start_s, t.end_s), (1.0, 3.0));
        assert_eq!(t.frames, FrameRange { start: 30, end: 90 });
    }

    // --- inheritance ---------------------------------------------------------

    #[test]
    fn untimed_element_inherits_program_span() {
        let (t, d) = realize_one("");
        assert!(d.is_empty());
        // program span = end of the last measured word (3.0s)
        assert_eq!((t.start_s, t.end_s), (0.0, 3.0));
        assert_eq!(t.frames, FrameRange { start: 0, end: 90 });
    }

    #[test]
    fn untimed_child_inherits_parent_span() {
        let mut scene = parse_scene("hook");
        scene.tracks[0].elements[0].children.push(Element {
            id: None,
            kind: ElementKind::Board,
            timing: None,
            placement: None,
            anim: None,
            children: Vec::new(),
            span: Span::new(0, 0),
        });
        let (resolved, d) = realize(&scene, &voice_timing());
        assert!(d.is_empty());
        let child = &resolved.unwrap().tracks[0].elements[0].children[0];
        assert_eq!((child.timing.start_s, child.timing.end_s), (0.0, 1.0));
    }

    // --- errors ---------------------------------------------------------------

    #[test]
    fn unknown_cue_errors() {
        let (_, d) = realize_one("ghost");
        assert!(d.iter().any(|x| x.severity == Severity::Error
            && x.message.contains("no timing data for cue `ghost`")));
    }

    #[test]
    fn cue_without_words_errors() {
        let mut map = TimingMap::default();
        map.insert(
            "voice",
            TimingSource {
                lines: vec![line("empty", Vec::new())],
            },
        );
        let scene = parse_scene("empty");
        let (_, d) = realize(&scene, &map);
        assert!(d.iter().any(|x| x.severity == Severity::Error
            && x.message.contains("cue `empty` has no aligned words")));
    }

    #[test]
    fn empty_range_errors() {
        let (_, d) = realize_one("payoff..hook");
        assert!(
            d.iter()
                .any(|x| x.severity == Severity::Error && x.message.contains("empty span"))
        );
    }

    #[test]
    fn missing_timing_source_errors() {
        let scene = parse_scene("hook");
        let (_, d) = realize(&scene, &TimingMap::default());
        assert!(d.iter().any(|x| {
            x.severity == Severity::Error
                && x.message
                    .contains("no timing data for script track `voice`")
        }));
    }

    #[test]
    fn errors_report_once() {
        // pass 1 reports; pass 2 must not re-report the same range
        let (_, d) = realize_one("ghost..also-ghost");
        assert_eq!(
            d.iter().filter(|x| x.severity == Severity::Error).count(),
            1
        );
    }

    // --- quantization ---------------------------------------------------------

    #[test]
    fn frame_quantization_covers_partial_frames() {
        // 0.0–0.51s at 30fps touches frames 0..=15 (0.51×30=15.3) → end 16
        let mut scene = parse_scene("0s..0.51s");
        scene.script = None;
        let (resolved, _) = realize(&scene, &TimingMap::default());
        let el = &resolved.unwrap().tracks[0].elements[0];
        assert_eq!(el.timing.frames, FrameRange { start: 0, end: 16 });
    }

    #[test]
    fn boundary_snapping_avoids_jitter() {
        // 1.0s at 30fps lands exactly on frame 30 — no off-by-one
        let (t, _) = realize_one("hook");
        assert_eq!(t.frames, FrameRange { start: 0, end: 30 });
        // and a literal end edge does the same
        let (t, _) = realize_one("0s..1s");
        assert_eq!(t.frames.end, 30);
    }

    #[test]
    fn sample_range_uses_48k() {
        let (t, _) = realize_one("hook");
        assert_eq!(
            t.samples,
            SampleRange {
                start: 0,
                end: 48_000
            }
        );
        let (t, _) = realize_one("payoff");
        assert_eq!(
            t.samples,
            SampleRange {
                start: 72_000,
                end: 144_000
            }
        );
    }

    #[test]
    fn non_integer_fps() {
        // 30000/1001 ≈ 29.97fps: 3.0s → 89.91 frames → ceil = 90
        let mut scene = parse_scene("hook..payoff");
        scene.frame_rate = Rational {
            numerator: 30000,
            denominator: 1001,
        };
        let (resolved, _) = realize(&scene, &voice_timing());
        let el = &resolved.unwrap().tracks[0].elements[0];
        assert_eq!(el.timing.frames.end, 90);
    }

    // --- program span ---------------------------------------------------------

    #[test]
    fn program_covers_timing_source_and_elements() {
        // element ends at 4s, past the voice track's 3.0s → program = 4s
        let scene = parse_scene("0s..4s");
        let (resolved, _) = realize(&scene, &voice_timing());
        assert_eq!(resolved.unwrap().program.end_s, 4.0);

        // nothing timed past 3.0s → program = voice end (3.0s)
        let scene = parse_scene("hook");
        let (resolved, _) = realize(&scene, &voice_timing());
        assert_eq!(resolved.unwrap().program.end_s, 3.0);
    }

    #[test]
    fn timeless_scene_errors() {
        // no script, no timings, no during → nothing resolves past t=0
        let mut scene = parse_scene("");
        scene.script = None;
        let (resolved, d) = realize(&scene, &TimingMap::default());
        assert!(resolved.is_none());
        assert!(
            d.iter()
                .any(|x| x.severity == Severity::Error && x.message.contains("no timing"))
        );
    }

    // --- determinism ----------------------------------------------------------

    #[test]
    fn realization_is_deterministic() {
        let scene = parse_scene("hook+2w..payoff-1w");
        let (a, da) = realize(&scene, &voice_timing());
        let (b, db) = realize(&scene, &voice_timing());
        let ta = a.unwrap().tracks[0].elements[0].timing;
        let tb = b.unwrap().tracks[0].elements[0].timing;
        assert_eq!(ta, tb);
        assert_eq!(da.len(), db.len());
    }
}
