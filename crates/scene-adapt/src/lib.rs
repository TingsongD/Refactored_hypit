//! `scene-adapt` — turn an external media file into a draft `.scene`.
//!
//! Pipeline: [`ingest`] resolves a path or URL to a local file →
//! [`analyze`] probes + shot-detects → [`emit_scene`] writes markup the
//! rest of the engine accepts. Each stage is separable so the pure
//! parts (`detect_cuts`, `emit_scene`) stay hermetic and unit-tested.

mod analyze;
mod emit;
mod ingest;

pub use analyze::{Analysis, analyze, detect_cuts, frame_diff};
pub use emit::emit_scene;
pub use ingest::{Ingested, ingest};

/// Anything adapt can fail with.
#[derive(Debug, thiserror::Error)]
pub enum AdaptError {
    #[error(transparent)]
    Media(#[from] scene_media::MediaError),
    #[error("ingest failed: {0}")]
    Ingest(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis(cuts: &[f64], has_audio: bool) -> Analysis {
        Analysis {
            path: std::path::PathBuf::from("clip.mp4"),
            duration_s: 9.0,
            width: 1920,
            height: 1080,
            fps: Some((30, 1)),
            has_audio,
            cuts: cuts.to_vec(),
        }
    }

    // --- frame_diff --------------------------------------------------------

    #[test]
    fn diff_identical_is_zero_opposite_is_max() {
        let a = vec![0u8; 64];
        assert_eq!(frame_diff(&a, &a), Some(0.0));
        let b = vec![255u8; 64];
        assert_eq!(frame_diff(&a, &b), Some(255.0));
        // Half the bytes differ by 2 → mean 1.0.
        let mut c = a.clone();
        for x in &mut c[..32] {
            *x = 2;
        }
        assert_eq!(frame_diff(&a, &c), Some(1.0));
        assert_eq!(frame_diff(&a, &[]), None);
        assert_eq!(frame_diff(&a, &a[..32]), None);
    }

    // --- detect_cuts -------------------------------------------------------

    #[test]
    fn flat_scores_never_cut() {
        let scores = vec![1.0; 40];
        assert!(detect_cuts(&scores, 4.0, 0.5).is_empty());
    }

    #[test]
    fn a_spike_is_a_cut_at_its_frame_time() {
        let mut scores = vec![1.0; 40];
        scores[7] = 60.0; // compares frame 7 → 8
        scores[23] = 55.0; // frame 24
        let cuts = detect_cuts(&scores, 4.0, 0.5);
        assert_eq!(cuts, vec![2.0, 6.0]);
    }

    #[test]
    fn cuts_respect_min_shot_spacing() {
        let mut scores = vec![1.0; 40];
        scores[7] = 60.0; // t=2.0
        scores[8] = 50.0; // t=2.25 — 1 frame later, inside 0.5s gap
        scores[20] = 55.0; // t=5.25, well past
        let cuts = detect_cuts(&scores, 4.0, 0.5);
        // Second spike merges into the first; stronger one wins (60 > 50
        // keeps t=2.0), then 5.25 stands alone.
        assert_eq!(cuts, vec![2.0, 5.25]);
    }

    #[test]
    fn weaker_first_spike_yields_to_stronger_neighbor() {
        let mut scores = vec![1.0; 40];
        scores[7] = 50.0; // t=2.0
        scores[8] = 70.0; // t=2.25, stronger — should replace
        let cuts = detect_cuts(&scores, 4.0, 0.5);
        assert_eq!(cuts, vec![2.25]);
    }

    #[test]
    fn adaptive_threshold_tolerates_bumpy_footage() {
        // Noisy baseline (mean ~5, some variance) but no real spike.
        let scores: Vec<f64> = (0..40).map(|i| 4.0 + (i % 5) as f64).collect();
        assert!(detect_cuts(&scores, 4.0, 0.5).is_empty());
    }

    // --- emit_scene --------------------------------------------------------

    /// The real gate: whatever emit produces must survive our own
    /// parser+lowerer with zero errors.
    fn lower_emitted(markup: &str) -> (Option<scene_ir::Scene>, Vec<String>) {
        let doc = match scene_markup::parse_document(markup) {
            Ok(doc) => doc,
            Err(d) => return (None, vec![d.message]),
        };
        let (scene, lower_diags) = scene_markup::lower(&doc);
        let msgs = lower_diags
            .iter()
            .filter(|d| d.severity == scene_ir::Severity::Error)
            .map(|d| d.message.clone())
            .collect();
        (scene, msgs)
    }

    #[test]
    fn emitted_scene_lowers_clean() {
        let a = analysis(&[2.0, 5.25], true);
        let (scene, errors) = lower_emitted(&emit_scene("assets/clip.mp4", &a));
        assert!(errors.is_empty(), "errors: {errors:?}");
        let scene = scene.expect("lowered");
        // clip + 3 shot boards on the visual track; music on audio track.
        let visual = &scene.tracks[0].elements;
        assert_eq!(visual.len(), 4);
        assert!(
            matches!(visual[0].kind, scene_ir::ElementKind::Clip { .. }),
            "first element is the clip"
        );
        assert!(
            scene.tracks[1]
                .elements
                .iter()
                .any(|e| matches!(e.kind, scene_ir::ElementKind::Music { .. }))
        );
    }

    #[test]
    fn emitted_no_audio_no_cuts_still_lowers() {
        let a = analysis(&[], false);
        let (scene, errors) = lower_emitted(&emit_scene("v.mp4", &a));
        assert!(errors.is_empty(), "errors: {errors:?}");
        let scene = scene.unwrap();
        assert_eq!(scene.tracks.len(), 1); // visual only
    }

    #[test]
    fn src_path_is_escaped() {
        let a = analysis(&[], false);
        let markup = emit_scene("a&b\"c.mp4", &a);
        assert!(markup.contains("a&amp;b&quot;c.mp4"));
        let (_, errors) = lower_emitted(&markup);
        assert!(errors.is_empty());
    }

    #[test]
    fn source_fps_carries_through_to_the_draft() {
        // NTSC 29.97 stays an exact rational.
        let mut a = analysis(&[], false);
        a.fps = Some((30000, 1001));
        let markup = emit_scene("v.mp4", &a);
        assert!(markup.contains("fps=\"30000/1001\""), "{markup}");
        let (scene, errors) = lower_emitted(&markup);
        assert!(errors.is_empty(), "{errors:?}");
        let rate = scene.unwrap().frame_rate;
        assert_eq!((rate.numerator, rate.denominator), (30000, 1001));

        // 24fps source → 24fps scene.
        a.fps = Some((24, 1));
        assert!(emit_scene("v.mp4", &a).contains("fps=\"24/1\""));

        // Unknown rate falls back to 30.
        a.fps = None;
        assert!(emit_scene("v.mp4", &a).contains("fps=\"30\""));
    }

    #[test]
    fn unsorted_and_out_of_range_cuts_are_normalized() {
        // Analysis contract violations must not silently drop boards.
        let a = analysis(&[7.0, 2.0, 2.0, -1.0, 9.0, 12.0, f64::NAN], true);
        let (scene, errors) = lower_emitted(&emit_scene("v.mp4", &a));
        assert!(errors.is_empty(), "errors: {errors:?}");
        let scene = scene.unwrap();
        // clip + boards for shots [0..2], [2..7], [7..9] — the -1/9/12/NaN
        // cuts normalize away (9 == dur duplicates the pushed boundary).
        let boards = scene.tracks[0]
            .elements
            .iter()
            .filter(|e| matches!(e.kind, scene_ir::ElementKind::Board))
            .count();
        assert_eq!(boards, 3);
    }

    // --- gated: real ffmpeg ------------------------------------------------

    fn gated() -> bool {
        std::env::var("SCENE_MEDIA_TESTS").is_ok()
    }

    /// Build a clip with two unmistakable hard cuts: red → green → blue.
    #[test]
    fn analyze_finds_real_cuts() {
        if !gated() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("adapt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("three-shots.mp4");
        // Three solid colors, 2s each — a cut detector can't miss these.
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=160x90:rate=8:duration=2",
                "-f",
                "lavfi",
                "-i",
                "color=c=green:size=160x90:rate=8:duration=2",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:size=160x90:rate=8:duration=2",
                "-filter_complex",
                "[0:v][1:v][2:v]concat=n=3:v=1[out]",
                "-map",
                "[out]",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(ok.success());

        let a = analyze(&path).unwrap();
        assert!(
            (a.duration_s - 6.0).abs() < 0.6,
            "duration {}",
            a.duration_s
        );
        assert_eq!(a.cuts.len(), 2, "cuts {:?}", a.cuts);
        assert!((a.cuts[0] - 2.0).abs() < 0.5, "cut0 {}", a.cuts[0]);
        assert!((a.cuts[1] - 4.0).abs() < 0.5, "cut1 {}", a.cuts[1]);

        // …and the emitted draft survives our own checker.
        let (scene, errors) = lower_emitted(&emit_scene("three-shots.mp4", &a));
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert!(scene.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
