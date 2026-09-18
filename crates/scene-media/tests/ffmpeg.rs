//! Env-gated integration tests — run only when real ffmpeg/ffprobe are
//! available and `SCENE_MEDIA_TESTS=1` is set. Everything is generated
//! from lavfi sources, so no fixture files are needed.

use std::path::PathBuf;
use std::process::Command;

use scene_ir::Rational;
use scene_media::{Encoder, FrameStream, probe};

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

/// Generate a deterministic test clip: 1s of 160x90 testsrc at 10fps.
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
            "testsrc=size=160x90:rate=10:duration=1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());
    path
}

/// Unique dir per test — tests run in threads within one process, so a
/// pid-only name would collide (and one test's cleanup eats another's).
fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scene-media-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn probe_lavfi_clip() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("probe");
    let clip = make_clip(&dir);
    let info = probe(&clip).unwrap();
    let video = info.video.expect("video stream");
    assert_eq!((video.width, video.height), (160, 90));
    assert_eq!(
        video.frame_rate.map(|r| (r.numerator, r.denominator)),
        Some((10, 1))
    );
    assert!((info.duration_s - 1.0).abs() < 0.1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn decode_all_frames() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("decode");
    let clip = make_clip(&dir);
    let stream = FrameStream::open(&clip).unwrap();
    assert_eq!(stream.dimensions(), (160, 90));

    let mut count = 0u64;
    let mut saw_distinct = false;
    let mut first = None;
    for frame in stream {
        let frame = frame.unwrap();
        assert_eq!((frame.width, frame.height), (160, 90));
        assert_eq!(frame.pixels.len(), 160 * 90 * 4);
        assert_eq!(frame.index, count);
        if count == 0 {
            first = Some(frame.pixels.clone());
        } else if let Some(f0) = &first
            && frame.pixels != *f0
        {
            saw_distinct = true; // testsrc animates — frames differ
        }
        count += 1;
    }
    assert_eq!(count, 10, "1s @ 10fps must yield 10 frames");
    assert!(saw_distinct);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A deterministic NV12 gradient — frame index shifts the pattern so
/// frames differ (proves we're not muxing one frame N times).
fn gradient_nv12(w: u32, h: u32, index: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut buf = vec![0u8; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            buf[y * w + x] = ((x + y + index as usize * 4) % 220 + 16) as u8;
        }
    }
    // Neutral chroma — gray, keeps x264 happy and decode predictable.
    let uv = w * h;
    for i in 0..w * h / 2 {
        buf[uv + i] = 128;
    }
    buf
}

#[test]
fn encode_nv12_to_mp4_roundtrip() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("encode");
    let out = dir.join("program.mp4");
    let (w, h) = (64u32, 36u32);
    let fps = Rational {
        numerator: 30,
        denominator: 1,
    };

    let mut enc = Encoder::open(&out, w, h, &fps).unwrap();
    for i in 0..30 {
        enc.write_frame(&gradient_nv12(w, h, i)).unwrap();
    }
    enc.finish().unwrap();
    assert!(out.exists());

    // Probe: dimensions, rate, duration.
    let info = probe(&out).unwrap();
    let video = info.video.expect("video stream");
    assert_eq!((video.width, video.height), (64, 36));
    assert_eq!(
        video.frame_rate.map(|r| (r.numerator, r.denominator)),
        Some((30, 1))
    );
    assert!((info.duration_s - 1.0).abs() < 0.1, "{}s", info.duration_s);

    // Decode back: exact frame count, and content actually varies.
    let frames: Vec<_> = FrameStream::open(&out)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(frames.len(), 30, "30 frames in must decode 30 out");
    assert_ne!(frames[0].pixels, frames[29].pixels);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn encode_to_unwritable_path_is_failed_not_hung() {
    if !gated() || !have("ffmpeg") {
        return;
    }
    let dir = tempdir("encode-fail");
    let out = dir.join("no/such/dir/out.mp4");
    let mut enc = Encoder::open(
        &out,
        64,
        36,
        &Rational {
            numerator: 30,
            denominator: 1,
        },
    )
    .unwrap();
    enc.write_frame(&gradient_nv12(64, 36, 0)).ok(); // may EPIPE — fine
    let err = enc.finish().unwrap_err();
    assert!(
        err.to_string().contains("ffmpeg"),
        "expected tool error, got {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_file_is_an_error() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let missing = PathBuf::from("/definitely/not/here.mp4");
    assert!(probe(&missing).is_err());
}
