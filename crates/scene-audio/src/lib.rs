//! scene-audio — the 48 kHz program mix.
//!
//! - [`AudioGraph::from_scene`] — resolved scene → clip graph (pure)
//! - [`filter_complex`] / [`mix_args`] — graph → ffmpeg argv (pure)
//! - [`mix_program`] — argv → program WAV (the only I/O in the crate)
//!
//! Sample math lives on one clock: [`PROGRAM_RATE`]. Visual timing comes
//! in already quantized to it (`ResolvedTiming.samples`), so this crate
//! never sees seconds-as-anchors — only positions.

mod emit;
mod graph;
mod mix;

pub use emit::{PROGRAM_RATE, filter_complex, gain_linear, mix_args, samples_to_ms};
pub use graph::{
    AudioClip, AudioGraph, DUCK_ATTACK_MS, DUCK_RATIO, DUCK_RELEASE_MS, DUCK_THRESHOLD, DuckLink,
    Fade,
};
pub use mix::mix_program;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use scene_ir::*;
    use scene_time::*;

    use super::*;

    fn timing(start: u32, end: u32) -> ResolvedTiming {
        // `start`/`end` are program *seconds* — tests stay readable.
        ResolvedTiming {
            start_s: start as f64,
            end_s: end as f64,
            frames: FrameRange {
                start: start * 30,
                end: end * 30,
            },
            samples: SampleRange {
                start: u64::from(start) * PROGRAM_RATE,
                end: u64::from(end) * PROGRAM_RATE,
            },
        }
    }

    fn audio_el(kind: ElementKind, timing: ResolvedTiming) -> ResolvedElement {
        ResolvedElement {
            id: None,
            kind,
            timing,
            placement: None,
            anim: None,
            children: Vec::new(),
            span: Span::new(0, 0),
        }
    }

    fn scene(tracks: Vec<ResolvedTrack>, program_secs: u32) -> ResolvedScene {
        ResolvedScene {
            canvas: Canvas {
                width: 320,
                height: 180,
            },
            frame_rate: Rational {
                numerator: 30,
                denominator: 1,
            },
            clear: Color::BLACK,
            script: None,
            program: timing(0, program_secs),
            tracks,
        }
    }

    fn track(id: &str, elements: Vec<ResolvedElement>) -> ResolvedTrack {
        ResolvedTrack {
            id: id.into(),
            kind: TrackKind::Audio,
            anchor: None,
            elements,
        }
    }

    #[test]
    fn from_scene_collects_audio_and_resolves_duck() {
        let scene = scene(
            vec![
                track(
                    "music",
                    vec![audio_el(
                        ElementKind::Music {
                            src: "bed.mp3".into(),
                            gain_db: -14.0,
                            duck: Some("voice".into()),
                        },
                        timing(0, 10),
                    )],
                ),
                track(
                    "voice",
                    vec![audio_el(
                        ElementKind::Sound {
                            src: "vo.wav".into(),
                            gain_db: 0.0,
                        },
                        timing(2, 8),
                    )],
                ),
            ],
            10,
        );
        let (graph, diags) = AudioGraph::from_scene(&scene, Path::new("/proj"));
        assert!(diags.is_empty());
        assert_eq!(graph.clips.len(), 2);
        assert_eq!(graph.program_samples, 10 * PROGRAM_RATE);
        let music = &graph.clips[0];
        assert_eq!(music.src, PathBuf::from("/proj/bed.mp3"));
        assert_eq!(music.gain_db, -14.0);
        assert_eq!(music.target.start, 0);
        // duck="voice" → keyed by the clip living on the voice track
        assert_eq!(
            graph.duck,
            vec![DuckLink {
                clip: 0,
                key: vec![1]
            }]
        );
    }

    #[test]
    fn duck_without_key_warns_and_degrades() {
        let scene = scene(
            vec![track(
                "music",
                vec![audio_el(
                    ElementKind::Music {
                        src: "bed.mp3".into(),
                        gain_db: 0.0,
                        duck: Some("narration".into()),
                    },
                    timing(0, 5),
                )],
            )],
            5,
        );
        let (graph, diags) = AudioGraph::from_scene(&scene, Path::new("/p"));
        assert!(graph.duck.is_empty());
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("narration"));
    }

    #[test]
    fn visual_elements_are_skipped_but_children_walked() {
        let mut board = audio_el(ElementKind::Board, timing(0, 4));
        board.children = vec![audio_el(
            ElementKind::Sound {
                src: "hit.wav".into(),
                gain_db: -3.0,
            },
            timing(1, 2),
        )];
        let scene = scene(vec![track("v", vec![board])], 4);
        let (graph, _) = AudioGraph::from_scene(&scene, Path::new("/p"));
        assert_eq!(graph.clips.len(), 1);
        assert_eq!(graph.clips[0].target.start, PROGRAM_RATE);
    }

    #[test]
    fn gain_and_delay_math() {
        assert!((gain_linear(0.0) - 1.0).abs() < 1e-9);
        assert!((gain_linear(-6.0) - 0.501187).abs() < 1e-5);
        assert!((gain_linear(-14.0) - 0.199526).abs() < 1e-5);
        assert_eq!(samples_to_ms(PROGRAM_RATE), 1000);
        assert_eq!(samples_to_ms(PROGRAM_RATE / 2), 500);
        assert_eq!(samples_to_ms(1), 0, "sub-ms lands at 0, never late");
    }

    #[test]
    fn emitted_chain_orders_ops_correctly() {
        let graph = AudioGraph {
            clips: vec![AudioClip {
                src: PathBuf::from("/p/bed.mp3"),
                target: SampleRange {
                    start: PROGRAM_RATE,
                    end: 4 * PROGRAM_RATE,
                },
                src_start_s: 0.0,
                gain_db: -6.0,
                fade: Fade {
                    in_s: 0.25,
                    out_s: 0.5,
                },
                track_id: "music".into(),
                span: Span::new(0, 0),
            }],
            duck: Vec::new(),
            program_samples: 10 * PROGRAM_RATE,
        };
        let fc = filter_complex(&graph);
        // trim window covers exactly the 3s target; delay = 1000ms|1000ms
        assert!(fc.contains("atrim=start=0.000000:end=3.000000"), "{fc}");
        assert!(fc.contains("adelay=1000|1000"), "{fc}");
        assert!(fc.contains("aresample=48000"), "{fc}");
        assert!(fc.contains("afade=t=in:st=0:d=0.250000"), "{fc}");
        assert!(fc.contains("afade=t=out:st=2.500000:d=0.500000"), "{fc}");
        assert!(fc.contains("amix=inputs=1:normalize=0[aout]"), "{fc}");
        assert!(!fc.contains("sidechaincompress"));
    }

    #[test]
    fn window_rebases_a_partial_mix() {
        // Program 0..6s; a tone plays 1s..4s. A `--frames` window of
        // 2s..3s must hear the *middle* of that tone — the source read
        // shifts by the cut front, the delay rebases to the window.
        let graph = AudioGraph {
            clips: vec![
                AudioClip {
                    src: PathBuf::from("/p/early.wav"),
                    target: SampleRange {
                        start: 0,
                        end: PROGRAM_RATE / 2,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "t".into(),
                    span: Span::new(0, 0),
                },
                AudioClip {
                    src: PathBuf::from("/p/tone.wav"),
                    target: SampleRange {
                        start: PROGRAM_RATE,
                        end: 4 * PROGRAM_RATE,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "t".into(),
                    span: Span::new(0, 0),
                },
            ],
            duck: Vec::new(),
            program_samples: 6 * PROGRAM_RATE,
        };
        let w = graph.window(2 * PROGRAM_RATE, 3 * PROGRAM_RATE);
        // The fully-before clip drops; the tone survives rebased.
        assert_eq!(w.clips.len(), 1);
        let tone = &w.clips[0];
        assert_eq!(tone.target.start, 0);
        assert_eq!(tone.target.end, PROGRAM_RATE);
        assert!(
            (tone.src_start_s - 1.0).abs() < 1e-9,
            "front cut → +1s source read"
        );
        assert_eq!(w.program_samples, PROGRAM_RATE);
        let fc = filter_complex(&w);
        assert!(fc.contains("atrim=start=1.000000:end=2.000000"), "{fc}");
        assert!(fc.contains("adelay=0|0"), "{fc}");
        // Identity window keeps everything; empty window yields silence.
        let full = graph.window(0, 6 * PROGRAM_RATE);
        assert_eq!(full.clips.len(), 2);
        assert!(
            graph
                .window(4 * PROGRAM_RATE, 2 * PROGRAM_RATE)
                .clips
                .is_empty()
        );
    }

    #[test]
    fn window_remaps_duck_links() {
        let graph = AudioGraph {
            clips: vec![
                AudioClip {
                    src: PathBuf::from("/p/bed.mp3"),
                    target: SampleRange {
                        start: 0,
                        end: 6 * PROGRAM_RATE,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "music".into(),
                    span: Span::new(0, 0),
                },
                AudioClip {
                    src: PathBuf::from("/p/drop.wav"),
                    target: SampleRange {
                        start: 0,
                        end: PROGRAM_RATE / 2,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "voice".into(),
                    span: Span::new(0, 0),
                },
                AudioClip {
                    src: PathBuf::from("/p/vo.wav"),
                    target: SampleRange {
                        start: PROGRAM_RATE,
                        end: 3 * PROGRAM_RATE,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "voice".into(),
                    span: Span::new(0, 0),
                },
            ],
            duck: vec![DuckLink {
                clip: 0,
                key: vec![1, 2],
            }],
            program_samples: 6 * PROGRAM_RATE,
        };
        // Window 2s..4s: clip 1 drops out, key remaps to the survivor.
        let w = graph.window(2 * PROGRAM_RATE, 4 * PROGRAM_RATE);
        assert_eq!(w.duck.len(), 1);
        assert_eq!(w.duck[0].clip, 0);
        assert_eq!(
            w.duck[0].key,
            vec![1],
            "dropped key is removed, kept key remapped"
        );
    }

    #[test]
    fn duck_emits_sidechain_with_split_key() {
        let graph = AudioGraph {
            clips: vec![
                AudioClip {
                    src: PathBuf::from("/p/bed.mp3"),
                    target: SampleRange {
                        start: 0,
                        end: 6 * PROGRAM_RATE,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "music".into(),
                    span: Span::new(0, 0),
                },
                AudioClip {
                    src: PathBuf::from("/p/vo.wav"),
                    target: SampleRange {
                        start: PROGRAM_RATE,
                        end: 3 * PROGRAM_RATE,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "voice".into(),
                    span: Span::new(0, 0),
                },
            ],
            duck: vec![DuckLink {
                clip: 0,
                key: vec![1],
            }],
            program_samples: 6 * PROGRAM_RATE,
        };
        let fc = filter_complex(&graph);
        // Key clip splits: one branch to mix, one to the key submix.
        assert!(fc.contains("asplit=2[a1m][a1k]"), "{fc}");
        assert!(fc.contains("[a1k]amix=inputs=1:normalize=0[key0]"), "{fc}");
        assert!(
            fc.contains("[a0][key0]sidechaincompress=threshold=0.020000:ratio=8.000000"),
            "{fc}"
        );
        // Final mix takes the ducked music + the key's mix branch.
        assert!(
            fc.contains("[duck0][a1m]amix=inputs=2:normalize=0[aout]"),
            "{fc}"
        );
    }

    #[test]
    fn args_feed_every_clip_and_cut_to_program() {
        let graph = AudioGraph {
            clips: vec![
                AudioClip {
                    src: PathBuf::from("/p/a.wav"),
                    target: SampleRange {
                        start: 0,
                        end: PROGRAM_RATE,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "t".into(),
                    span: Span::new(0, 0),
                },
                AudioClip {
                    src: PathBuf::from("/p/b.wav"),
                    target: SampleRange {
                        start: 0,
                        end: PROGRAM_RATE,
                    },
                    src_start_s: 0.0,
                    gain_db: 0.0,
                    fade: Fade::default(),
                    track_id: "t".into(),
                    span: Span::new(0, 0),
                },
            ],
            duck: Vec::new(),
            program_samples: 2 * PROGRAM_RATE,
        };
        let args = mix_args(&graph, Path::new("/out.wav"));
        let inputs = args.iter().filter(|a| *a == "-i").count();
        assert_eq!(inputs, 2);
        assert!(args.iter().any(|a| a == "-filter_complex"));
        assert!(args.windows(2).any(|w| w == ["-t", "2.000000"]));
        assert!(args.windows(2).any(|w| w == ["-ar", "48000"]));
        assert!(args.windows(2).any(|w| w == ["-map", "[aout]"]));
    }
}
