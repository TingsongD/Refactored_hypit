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
