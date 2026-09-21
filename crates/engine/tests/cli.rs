use assert_cmd::Command;
use predicates::str::contains;

fn example(name: &str) -> String {
    format!("{}/../../examples/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn check_passes_on_the_minimal_example() {
    // Missing referenced assets are warnings, not errors.
    Command::cargo_bin("engine")
        .unwrap()
        .args(["check", &example("minimal.scene")])
        .assert()
        .success();
}

#[test]
fn check_fails_with_span_diagnostics() {
    Command::cargo_bin("engine")
        .unwrap()
        .args(["check", &fixture("bad.scene")])
        .assert()
        .failure()
        .stderr(contains("unknown cue `missing`"))
        .stderr(contains("unknown element <florp>"));
}

#[test]
fn parse_emits_versioned_ir() {
    Command::cargo_bin("engine")
        .unwrap()
        .args(["parse", &example("minimal.scene")])
        .assert()
        .success()
        .stdout(contains("\"format\": \"scene.ir@1\""))
        .stdout(contains("\"karaoke\""));
}

#[test]
fn check_reads_stdin() {
    Command::cargo_bin("engine")
        .unwrap()
        .args(["check", "-"])
        .write_stdin("<scene canvas=\"1x1\" fps=\"30\"/>")
        .assert()
        .success();
}

#[test]
fn init_scaffolds_a_project() {
    let dir = std::env::temp_dir().join(format!("engine-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Command::cargo_bin("engine")
        .unwrap()
        .args(["init", dir.to_str().unwrap()])
        .assert()
        .success();
    assert!(dir.join("main.scene").exists());
    // Re-init refuses to overwrite.
    Command::cargo_bin("engine")
        .unwrap()
        .args(["init", dir.to_str().unwrap()])
        .assert()
        .failure();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Offline end-to-end: a cut-heavy lavfi clip in, keeps + .scene out,
/// and the emitted scene passes `engine check`. Gated on real ffmpeg —
/// the same `SCENE_MEDIA_TESTS` env as the scene-media suite.
#[test]
fn meme_offline_run_emits_a_valid_scene() {
    if std::env::var("SCENE_MEDIA_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("engine-meme-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join("cuts.mp4");
    // 4 shots, hard cuts every half second + a sine that swells once.
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=red:size=160x90:rate=30:duration=0.5[c0];\
             color=blue:size=160x90:rate=30:duration=0.5[c1];\
             color=green:size=160x90:rate=30:duration=0.5[c2];\
             color=white:size=160x90:rate=30:duration=0.5[c3];\
             [c0][c1][c2][c3]concat=n=4:v=1:a=0",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=2,volume='0.1+0.9*gt(t,1)':eval=frame",
            "-pix_fmt",
            "yuv420p",
            "-shortest",
        ])
        .arg(&clip)
        .status()
        .expect("spawn ffmpeg");
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return; // ffmpeg present but can't build the fixture — skip
    }
    let out = dir.join("out");
    Command::cargo_bin("engine")
        .unwrap()
        .args([
            "meme",
            clip.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--config",
            dir.join("no-such.toml").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("keeps"));
    // The emitted scene compiles clean.
    let scene = out.join("meme.scene");
    assert!(scene.exists());
    Command::cargo_bin("engine")
        .unwrap()
        .args(["check", scene.to_str().unwrap()])
        .assert()
        .success();
    // Spec artifacts exist.
    for name in ["metrics", "peaks", "pack", "keeps", "package"] {
        assert!(out.join(format!("{name}.json")).exists(), "{name}.json");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
