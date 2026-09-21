//! Env-gated integration tests — run only when real ffmpeg/ffprobe are
//! available and `SCENE_MEDIA_TESTS=1` is set. Everything is generated
//! from lavfi sources, so no fixture files are needed.

use std::path::PathBuf;
use std::process::Command;

use scene_ir::Rational;
use scene_media::{Encoder, FrameStream, PcmStream, probe};

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

/// PCM sine WAV — exact sample counts (no codec priming/padding, so
/// hop boundaries are deterministic).
fn make_wav(dir: &std::path::Path, name: &str, duration_s: &str) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate=15360:duration={duration_s}"),
        ])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());
    path
}

#[test]
fn pcm_decodes_exact_wav() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("pcm-exact");
    let wav = make_wav(&dir, "tone.wav", "0.5");
    let info = probe(&wav).unwrap();
    // 0.5s at 15360 Hz = 7680 samples = 15 hops of 512.
    let stream = PcmStream::open(&wav, &info, 15360, 512).unwrap().unwrap();
    let chunks: Vec<_> = stream.collect::<Result<_, _>>().unwrap();
    assert_eq!(chunks.len(), 15, "0.5s must yield exactly 15 hops");
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c.index, i as u64);
        assert_eq!(c.samples.len(), 512);
    }
    // ffmpeg's `sine` defaults to amplitude 1/8 — RMS ≈ 0.088. Loose
    // bound: prove the hop carries energy, not silence.
    let rms: f32 = (chunks[0].samples.iter().map(|s| s * s).sum::<f32>() / 512.0).sqrt();
    assert!(rms > 0.05, "sine hop should carry energy, rms={rms}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pcm_tail_pads_to_hop() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("pcm-tail");
    // 0.503s ≈ 7726±1 samples — 15 full hops plus a ~46-sample tail.
    let wav = make_wav(&dir, "tail.wav", "0.503");
    let info = probe(&wav).unwrap();
    let stream = PcmStream::open(&wav, &info, 15360, 512).unwrap().unwrap();
    let chunks: Vec<_> = stream.collect::<Result<_, _>>().unwrap();
    assert_eq!(chunks.len(), 16, "partial tail must surface as a hop");
    // The tail is zero-padded past its real samples — the pad keeps the
    // hop grid aligned with the video frames it indexes.
    assert!(
        chunks[15].samples[100..].iter().all(|&s| s == 0.0),
        "tail beyond ~46 real samples must be zero-padded"
    );
    assert!(chunks[15].samples[..32].iter().any(|&s| s != 0.0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pcm_no_audio_is_none() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("pcm-none");
    let clip = make_clip(&dir); // testsrc — video only
    let info = probe(&clip).unwrap();
    assert!(info.audio.is_none());
    let stream = PcmStream::open(&clip, &info, 15360, 512).unwrap();
    assert!(stream.is_none(), "no audio stream → Ok(None), not an error");
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

#[test]
fn scaled_windows_match_sequential_cfr() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("scaled-window");
    let path = dir.join("long.mp4");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x90:rate=10:duration=3",
                "-pix_fmt",
                "yuv420p",
                "-output_ts_offset",
                "5"
            ])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let info = probe(&path).unwrap();
    let all: Vec<_> = FrameStream::open_scaled(&path, &info, 80, 46, 10.0)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (start_s, end_s, range) in [
        (2.3, 2.7, 23..27),
        (2.3, 2.71, 23..28),
        (2.31, 2.69, 24..27),
        (0.0, 0.01, 0..1),
    ] {
        let selected: Vec<_> = FrameStream::open_scaled_window(
            &path,
            &info,
            80,
            46,
            10.0,
            Some(scene_media::DecodeWindow { start_s, end_s }),
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
        assert_eq!(selected.len(), range.len(), "window {start_s}..{end_s}");
        for (i, source) in range.enumerate() {
            assert_eq!(selected[i].index, i as u64);
            assert_eq!(selected[i].pixels, all[source].pixels);
        }
    }
    for (start_s, end_s) in [(1.0, 0.0), (0.0, f64::INFINITY), (-1.0, 1.0)] {
        assert!(
            FrameStream::open_scaled_window(
                &path,
                &info,
                80,
                46,
                10.0,
                Some(scene_media::DecodeWindow { start_s, end_s })
            )
            .is_err()
        );
    }
    assert!(FrameStream::open_scaled(&path, &info, 0, 46, f64::NAN).is_err());
    let _ = std::fs::remove_dir_all(dir);
}
