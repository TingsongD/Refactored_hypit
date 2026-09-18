//! Env-gated integration: real ffmpeg mixes. `SCENE_MEDIA_TESTS=1` to run.
//! Fixtures are lavfi sines — deterministic, no binary assets.

use std::path::{Path, PathBuf};
use std::process::Command;

use scene_audio::{AudioClip, AudioGraph, DuckLink, Fade, PROGRAM_RATE, mix_program};
use scene_ir::Span;
use scene_media::probe;
use scene_time::SampleRange;

fn gated() -> bool {
    std::env::var("SCENE_MEDIA_TESTS").ok().as_deref() == Some("1")
        && Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scene-audio-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `sine` source → stereo wav. `freq` distinguishes music from "voice".
fn sine(dir: &Path, name: &str, freq: u32, secs: u32) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency={freq}:duration={secs}:sample_rate=48000"),
            "-ac",
            "2",
        ])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());
    path
}

fn clip(src: PathBuf, start_s: u64, end_s: u64, gain_db: f64, track: &str) -> AudioClip {
    AudioClip {
        src,
        target: SampleRange {
            start: start_s * PROGRAM_RATE,
            end: end_s * PROGRAM_RATE,
        },
        src_start_s: 0.0,
        gain_db,
        fade: Fade::default(),
        track_id: track.into(),
        span: Span::new(0, 0),
    }
}

/// `mean_volume` dB of a segment, via volumedetect on stderr. `band`
/// optionally isolates the 440 Hz music tone from the 880 Hz "voice".
fn segment_level(file: &Path, from_s: f64, to_s: f64, band: bool) -> f64 {
    let af = if band {
        "highpass=f=300,lowpass=f=600,volumedetect"
    } else {
        "volumedetect"
    };
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "info",
            "-ss",
            &from_s.to_string(),
            "-to",
            &to_s.to_string(),
            "-i",
            &file.display().to_string(),
            "-af",
            af,
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("volumedetect");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let marker = "mean_volume:";
    let at = stderr.find(marker).expect("mean_volume in stderr");
    stderr[at + marker.len()..]
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .expect("dB float")
}

#[test]
fn two_sines_mix_to_program_length() {
    if !gated() {
        return;
    }
    let dir = tempdir("mix");
    let a = sine(&dir, "a.wav", 440, 3);
    let b = sine(&dir, "b.wav", 880, 1);
    let out = dir.join("mix.wav");
    let graph = AudioGraph {
        clips: vec![
            clip(a, 0, 3, 0.0, "music"),
            clip(b, 1, 2, 0.0, "voice"), // delayed 1s
        ],
        duck: Vec::new(),
        program_samples: 3 * PROGRAM_RATE,
    };
    mix_program(&graph, &out).unwrap();
    let info = probe(&out).unwrap();
    assert!((info.duration_s - 3.0).abs() < 0.05, "{}s", info.duration_s);
    // 440Hz only in solo region, both tones inside the window.
    let solo = segment_level(&out, 0.2, 0.8, false);
    let both = segment_level(&out, 1.2, 1.8, false);
    assert!(both > solo, "overlap region louder: {both} vs {solo}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ducking_actually_ducks() {
    if !gated() {
        return;
    }
    let dir = tempdir("duck");
    let music = sine(&dir, "bed.wav", 440, 3);
    let voice = sine(&dir, "vo.wav", 880, 1);
    // Voice at authored level — it has to cross the compressor threshold
    // to key. Measurements isolate the 440 Hz music tone via bandpass.
    let base = |duck: Vec<DuckLink>| AudioGraph {
        clips: vec![
            clip(music.clone(), 0, 3, 0.0, "music"),
            clip(voice.clone(), 1, 2, 0.0, "voice"),
        ],
        duck,
        program_samples: 3 * PROGRAM_RATE,
    };
    let unducked = dir.join("noduck.wav");
    let ducked = dir.join("duck.wav");
    mix_program(&base(Vec::new()), &unducked).unwrap();
    mix_program(
        &base(vec![DuckLink {
            clip: 0,
            key: vec![1],
        }]),
        &ducked,
    )
    .unwrap();

    let free = segment_level(&ducked, 0.2, 0.8, true); // before voice: untouched
    let solo = segment_level(&unducked, 1.2, 1.8, true); // window, no duck
    let squashed = segment_level(&ducked, 1.2, 1.8, true); // window, ducked
    assert!(
        squashed < solo - 3.0,
        "ducked window should drop ≥3dB: {squashed} vs {solo}"
    );
    assert!(
        (free - solo).abs() < 3.0,
        "outside the window ducking shouldn't move level: {free} vs {solo}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
