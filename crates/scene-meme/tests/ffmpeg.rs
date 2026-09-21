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
        representative_t: i as f64 / 30.0,
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

#[test]
fn custom_connector_preflight_usage_and_cache_invalidation() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    use scene_meme::run::{RunOpts, run_with_progress};
    let dir = tempdir("mock-provider");
    let clip = make_av_clip(&dir);
    let script = dir.join("mock.py");
    let marker = dir.join("preflight");
    std::fs::write(&script, format!(r#"
import json,sys,pathlib
req=json.load(sys.stdin)
assert pathlib.Path({marker:?}).exists(), 'preflight must arrive before provider execution'
p=req['params']
assert p['model']=='offline-model'
assert all('representative_t' in k and 'word' in k and 'onset' in k for k in p['keeps'])
pathlib.Path(req['out']).write_text(json.dumps({{'beats':[{{'id':k['id'],'t':k['t'],'note':p['mode']}} for k in p['keeps']], 'usage':{{'input_tokens':100,'output_tokens':20}}}}))
"#, marker=marker.to_string_lossy())).unwrap();
    let python = if cfg!(windows) { "python" } else { "python3" };
    let config = format!(
        "[capabilities.custom]\ncommand = [{}, {}]\n",
        serde_json::to_string(python).unwrap(),
        serde_json::to_string(&script).unwrap()
    );
    let registry = scene_cap::Registry::from_toml(&config).unwrap();
    let mut brief = Brief {
        gemini_cap: "custom".into(),
        gemini_model: Some("offline-model".into()),
        change_keep: 0.000001,
        change_skip: 0.0,
        min_sharpness: 0.0,
        dup_cosine: 1.0,
        snap_to_sharp: false,
        max_candidates_to_jev: 2,
        input_price_per_million: Some(1.0),
        output_price_per_million: Some(2.0),
        ..Brief::default()
    };
    let out = dir.join("out");
    let execute = |brief: &Brief, retry: Option<String>| {
        let mut phases = Vec::new();
        let result = run_with_progress(
            &RunOpts {
                input: &clip,
                brief,
                out: &out,
                timings: TimingMap::default(),
                registry: Some(&registry),
                beat_sec: 0.2,
                materialize: false,
                gemini_mode: scene_meme::gemini::GeminiMode::Stills,
                rerun_window: retry,
                music_src: None,
            },
            &mut |event| {
                if event.phase == "preflight" {
                    std::fs::write(&marker, b"ready").unwrap();
                }
                phases.push((event.stage, event.phase, event.cache_hit));
            },
        )
        .unwrap();
        (result, phases)
    };
    let (first, phases) = execute(&brief, None);
    assert!(first.keeps.len() >= 2);
    let events: Vec<_> = phases.iter().filter(|e| e.0 == "gemini").collect();
    assert_eq!(events[0].1, "preflight");
    assert_eq!(events[1].1, "complete");
    let usage = first
        .stages
        .iter()
        .find(|e| e.stage == "gemini" && e.phase == "complete")
        .unwrap();
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.output_tokens, Some(20));
    assert!((usage.estimated_cost_usd.unwrap() - 0.00014).abs() < 1e-9);
    let (_, phases) = execute(&brief, None);
    assert!(phases.iter().any(|e| e.0 == "gemini" && e.2));
    let id = first.keeps[0].clone();
    let (retried, _) = execute(&brief, Some(id.clone()));
    let analysis = retried.gemini.unwrap();
    assert_eq!(
        analysis.beats.iter().find(|b| b.id == id).unwrap().note,
        "windows"
    );
    assert!(
        analysis
            .beats
            .iter()
            .filter(|b| b.id != id)
            .all(|b| b.note == "stills")
    );
    let (cached, _) = execute(&brief, None);
    assert_eq!(
        cached
            .gemini
            .unwrap()
            .beats
            .iter()
            .find(|b| b.id == id)
            .unwrap()
            .note,
        "windows"
    );
    brief.gemini_fps = 12.0;
    let (_, phases) = execute(&brief, None);
    assert!(phases.iter().any(|e| e.1 == "preflight"));
    std::fs::write(
        &script,
        std::fs::read_to_string(&script).unwrap() + "\n# changed adapter\n",
    )
    .unwrap();
    let (_, phases) = execute(&brief, None);
    assert!(phases.iter().any(|e| e.1 == "preflight"));
    brief.description = "cold targeted retry".into();
    let (partial, _) = execute(&brief, Some(id));
    assert_eq!(partial.gemini.unwrap().beats.len(), 1);
    assert_eq!(
        partial.decision.package,
        scene_meme::package::Package::NeedMorePeaks
    );
    let (complete, phases) = execute(&brief, None);
    assert_eq!(complete.gemini.unwrap().beats.len(), complete.keeps.len());
    assert!(
        phases.iter().any(|e| e.1 == "preflight"),
        "partial retry must not masquerade as a complete cache hit"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn still_uses_representative_not_beat_timestamp() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("sharp-still");
    let clip = make_av_clip(&dir);
    let mut candidate = keep(0);
    candidate.representative_t = 0.5;
    let files = materialize_stills(&clip, &[&candidate], &dir.join("stills")).unwrap();
    let expected = dir.join("expected.png");
    assert!(
        Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-ss", "0.5", "-i"])
            .arg(&clip)
            .args(["-frames:v", "1", "-f", "image2"])
            .arg(&expected)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        std::fs::read(&files[0].file).unwrap(),
        std::fs::read(expected).unwrap()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn embedding_cache_tracks_vocabulary_resolution_model_checkpoint_and_grid() {
    if !gated() || !have("ffmpeg") || !have("ffprobe") {
        return;
    }
    let dir = tempdir("embedding-inputs");
    let clip = make_av_clip(&dir);
    let script = dir.join("embed.py");
    std::fs::write(&script,r#"
import json,sys,math,pathlib
req=json.load(sys.stdin); p=req['params']
rows=[[math.cos(i),math.sin(i)] for i in range(round(p['fps']))]
pathlib.Path(req['out']).write_text(json.dumps({'embeddings':rows,'vocab_embeddings':[[1,0] for _ in p['vocab']]}))
"#).unwrap();
    let python = if cfg!(windows) { "python" } else { "python3" };
    let registry = scene_cap::Registry::from_toml(&format!(
        "[capabilities.embed]\ncommand=[{},{}]",
        serde_json::to_string(python).unwrap(),
        serde_json::to_string(&script).unwrap()
    ))
    .unwrap();
    let base = Brief {
        encoder: "embed".into(),
        ..Brief::default()
    };
    let out = dir.join("out");
    let run = |brief: &Brief| {
        scene_meme::run::run(&scene_meme::run::RunOpts {
            input: &clip,
            brief,
            out: &out,
            timings: TimingMap::default(),
            registry: Some(&registry),
            beat_sec: 0.2,
            materialize: false,
            gemini_mode: scene_meme::gemini::GeminiMode::Stills,
            rerun_window: None,
            music_src: None,
        })
        .unwrap()
    };
    let hit = |report: &scene_meme::run::RunReport| {
        report
            .stages
            .iter()
            .find(|s| s.stage == "embed")
            .unwrap()
            .cache_hit
    };
    assert!(!hit(&run(&base)));
    assert!(hit(&run(&base)));
    for changed in [
        Brief {
            tag_vocab: vec!["new label".into()],
            ..base.clone()
        },
        Brief {
            perceive_size: 128,
            ..base.clone()
        },
        Brief {
            embedding_model: Some("mock-alternative".into()),
            ..base.clone()
        },
        Brief {
            weights_id: Some("mock-checkpoint".into()),
            ..base.clone()
        },
        Brief {
            fps: 15.0,
            ..base.clone()
        },
    ] {
        assert!(!hit(&run(&changed)));
        assert!(hit(&run(&changed)));
    }
    std::fs::write(
        &script,
        std::fs::read_to_string(&script).unwrap() + "\n# implementation changed",
    )
    .unwrap();
    assert!(!hit(&run(&base)));
    let _ = std::fs::remove_dir_all(dir);
}
