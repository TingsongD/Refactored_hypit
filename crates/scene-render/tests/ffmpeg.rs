//! Env-gated integration tests — run only when real ffmpeg/ffprobe are
//! available and `SCENE_MEDIA_TESTS=1` is set. Clips are generated from
//! lavfi sources, so no fixture files are needed.

use std::path::PathBuf;
use std::process::Command;

use scene_media::FrameStream;
use scene_render::{FrameSource, SeqFrameSource};

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
    let dir = std::env::temp_dir().join(format!("scene-render-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 4s of 160x90 testsrc at 30fps — 120 distinct frames.
fn make_clip(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("testsrc.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=30:duration=4",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());
    path
}

/// A first sample deep into a clip must return the *same pixels* as a
/// full sequential decode of that frame — the `-ss` seek and the `base`
/// index offset are only worth anything if the mapping is exact.
#[test]
fn seeked_sample_matches_unseeked_decode() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("seek");
    let clip = make_clip(&dir);
    let root = clip.parent().unwrap().to_path_buf();
    let name = clip.file_name().unwrap().to_str().unwrap().to_string();

    // First sample at program frame 90 (≥ SEEK_THRESHOLD) — opens with
    // `-ss` instead of decoding frames 0..90.
    let mut source = SeqFrameSource::new(root);
    let seeked = source.sample(&name, 90, 30.0).expect("sampled frame");
    assert!(
        seeked.index >= 90,
        "effective index ≥ target: {}",
        seeked.index
    );

    // Reference: plain sequential decode of source frame 90.
    let reference = FrameStream::open(&clip)
        .unwrap()
        .nth(90)
        .expect("has frame 90")
        .unwrap();
    assert_eq!(
        seeked.pixels, reference.pixels,
        "seeked decode returned different pixels for source frame 90"
    );

    // Subsequent samples still walk forward correctly.
    let next = source.sample(&name, 95, 30.0).expect("next frame");
    assert!(next.index >= 95);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two elements sharing one source: the second restarts at frame 0.
/// Without a reopen, `sample` would keep serving the first element's
/// last decoded frame — the clip appears frozen on its tail.
#[test]
fn reusing_a_source_restarts_the_clip() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("restart");
    let clip = make_clip(&dir);
    let root = clip.parent().unwrap().to_path_buf();
    let name = clip.file_name().unwrap().to_str().unwrap().to_string();

    let mut source = SeqFrameSource::new(root);
    // First element plays deep into the clip.
    let _ = source.sample(&name, 100, 30.0).expect("deep sample");
    // Second element restarts at its own frame 0 → source frame 0.
    let restarted = source.sample(&name, 0, 30.0).expect("restarted frame");
    assert_eq!(restarted.index, 0);
    let reference = FrameStream::open(&clip)
        .unwrap()
        .next()
        .expect("has frame 0")
        .unwrap();
    assert_eq!(
        restarted.pixels, reference.pixels,
        "restarted clip must show frame 0, not the previous tail"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Below the threshold the stream opens from 0 — no seek, index equals
/// the plain decoded position.
#[test]
fn small_offsets_still_decode_from_zero() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("noseek");
    let clip = make_clip(&dir);
    let root = clip.parent().unwrap().to_path_buf();
    let name = clip.file_name().unwrap().to_str().unwrap().to_string();

    let mut source = SeqFrameSource::new(root);
    let frame = source.sample(&name, 10, 30.0).expect("sampled frame");
    assert_eq!(frame.index, 10);
    let _ = std::fs::remove_dir_all(&dir);
}
