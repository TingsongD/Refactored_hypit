//! Env-gated integration tests — run only when real ffmpeg/ffprobe are
//! available and `SCENE_MEDIA_TESTS=1` is set. All media is generated
//! from lavfi sources, so no fixture files are needed.

use std::path::{Path, PathBuf};
use std::process::Command;

use scene_media::probe;
use scene_meme::emit::{beat_spans, materialize_beats};
use scene_meme::gemini::{materialize_stills, materialize_windows};
use scene_meme::peaks::{Candidate, PeakSource};
use scene_meme::{Brief, perceive};
use scene_time::TimingMap;

fn keep(i: u64) -> Candidate {
    Candidate {
        id: format!("f{i}"),
        frame: i,
        t: i as f64 / 30.0,
        change: 0.5,
        sharpness: 0.9,
        motion: 0.5,
        brightness: 0.5,
        contrast: 0.5,
        importance: 0.5,
        same_shot: false,
        cut_on_beat: false,
        onset: 0.0,
        word: None,
        source: PeakSource::Visual,
        sim_to_kept: 0.0,
        tags: Vec::new(),
    }
}

fn gated() -> bool {
    std::env::var("SCENE_MEDIA_TESTS").ok().as_deref() == Some("1")
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scene-meme-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 1s of testsrc video + a 440 Hz sine bed.
fn make_av_clip(dir: &Path) -> PathBuf {
    let path = dir.join("av.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=30:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-pix_fmt",
            "yuv420p",
            "-shortest",
        ])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());
    path
}

#[test]
fn perceive_aligns_frames_and_hops() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("perceive");
    let clip = make_av_clip(&dir);
    let info = probe(&clip).unwrap();
    let brief = Brief::default();
    let p = perceive(&clip, &info, &brief, &TimingMap::default()).unwrap();

    // 1s at 30 fps → ~30 frames and 30 aligned hops (container rounding
    // can shave a tail — the assertion is loose, the index alignment is not).
    assert!(p.frames.len() >= 28, "{} frames", p.frames.len());
    assert_eq!(p.frames.len(), p.audio.len(), "one hop per frame");
    assert!(p.has_audio);
    for (i, (f, a)) in p.frames.iter().zip(&p.audio).enumerate() {
        assert_eq!(f.frame, i as u64);
        assert_eq!(a.frame, i as u64);
        assert!((f.t - i as f64 / 30.0).abs() < 1e-9);
        // The sine bed is loud everywhere — no hop should read silent.
        assert!(!a.silence, "hop {i} silent over a sine bed");
        assert!(a.loud_db > -40.0, "hop {i} loud_db {}", a.loud_db);
    }
    // testsrc animates — some inter-frame change and nonzero motion.
    assert!(p.frames.iter().any(|f| f.motion > 0.01));
    assert!(p.frames.iter().skip(1).any(|f| f.change > 0.0));
    // Sharpness got normalized into 0..1.
    assert!(p.frames.iter().all(|f| (0.0..=1.0).contains(&f.sharpness)));
    assert!(p.frames.iter().any(|f| f.sharpness > 0.0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn perceive_without_audio_marks_silence() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("perceive-mute");
    // Video only — no audio stream.
    let path = dir.join("mute.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=30:duration=1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());
    let info = probe(&path).unwrap();
    let p = perceive(&path, &info, &Brief::default(), &TimingMap::default()).unwrap();
    assert!(!p.has_audio);
    assert!(p.audio.iter().all(|h| h.silence && h.loud_db <= -100.0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn materialize_stills_writes_one_png_per_keep() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("stills");
    let clip = make_av_clip(&dir);
    let keeps = [keep(5), keep(20)];
    let refs: Vec<&Candidate> = keeps.iter().collect();
    let files = materialize_stills(&clip, &refs, &dir.join("frames")).unwrap();
    assert_eq!(files.len(), 2);
    for f in &files {
        let bytes = std::fs::read(&f.file).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{} not a PNG", f.id);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn materialize_windows_writes_short_mp4s() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("windows");
    let clip = make_av_clip(&dir);
    let keeps = [keep(15)];
    let refs: Vec<&Candidate> = keeps.iter().collect();
    let brief = Brief {
        gemini_window_sec: 0.4,
        gemini_fps: 8.0,
        ..Default::default()
    };
    let files = materialize_windows(&clip, &refs, &dir.join("win"), &brief).unwrap();
    assert_eq!(files.len(), 1);
    let info = probe(&files[0].file).unwrap();
    // ~0.4s window ± container slack; resampled to 8 fps.
    let v = info.video.expect("video stream");
    assert!(
        info.duration_s > 0.2 && info.duration_s < 0.8,
        "{}s",
        info.duration_s
    );
    let fps = v.frame_rate.expect("frame rate").to_f64();
    assert!((fps - 8.0).abs() < 0.5, "{fps}fps");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn materialize_beats_cuts_real_clips_with_audio() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("beats");
    let clip = make_av_clip(&dir);
    let keeps = [keep(10), keep(25)];
    let refs: Vec<&Candidate> = keeps.iter().collect();
    let mut spans = beat_spans(&refs, 0.4, 1.0);
    materialize_beats(&clip, &mut spans, &dir.join("beats")).unwrap();
    for s in &spans {
        let file = s.file.as_ref().expect("materialized");
        let info = probe(file).unwrap();
        assert!(info.video.is_some() && info.audio.is_some());
        assert!(info.duration_s > 0.2 && info.duration_s < 0.8);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
