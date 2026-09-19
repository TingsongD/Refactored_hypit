//! Command implementations. Each returns a process exit code.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use scene_align::{markers_to_timing, parse_markers, timing_map_for, whisperx_to_timing};
use scene_audio::{AudioGraph, mix_program};
use scene_ir::{Diagnostic, IR_FORMAT};
use scene_markup::compile;
use scene_media::Encoder;
use scene_render::{
    CosmicText, RenderMeasure, Renderer, SeqFrameSource, StillFrameSource, render_frames,
    rgba_to_nv12,
};
use scene_script::SandboxPrograms;
use scene_time::{TimingMap, realize};

use crate::report;

/// Read a `.scene` source: a file path, or `-` for stdin.
fn read_source(file: &Path) -> Result<(String, String), String> {
    if file == Path::new("-") {
        let mut source = String::new();
        std::io::stdin()
            .read_to_string(&mut source)
            .map_err(|e| e.to_string())?;
        Ok(("<stdin>".to_string(), source))
    } else {
        let source =
            fs::read_to_string(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
        Ok((file.display().to_string(), source))
    }
}

/// Parse + lower one source and print every diagnostic. Returns the scene
/// when it is clean enough to use.
fn compile_and_report(name: &str, source: &str) -> Option<scene_ir::Scene> {
    let outcome = compile(source);
    report::emit(name, source, &outcome.diagnostics);
    outcome.scene
}

fn has_error_diagnostics(source: &str) -> bool {
    compile(source).diagnostics.iter().any(|d| d.is_error())
}

/// File-existence warnings for a scene's asset refs — a CLI/UI concern
/// (needs fs access), so the IR itself stays pure. Shared by `check` and
/// `/api/check` so both produce the same diagnostics.
pub fn asset_warnings(scene: &scene_ir::Scene, base: &Path) -> Vec<Diagnostic> {
    scene
        .asset_refs()
        .into_iter()
        .filter(|(src, _)| !src.contains("://"))
        .filter(|(src, _)| !base.join(src).exists())
        .map(|(src, span)| {
            Diagnostic::warning(format!("referenced file not found: {src}"), Some(span))
        })
        .collect()
}

pub fn check(file: &Path) -> i32 {
    let (name, source) = match read_source(file) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let outcome = compile(&source);
    let mut diagnostics = outcome.diagnostics;

    // File-existence warnings are a CLI concern (they need fs access);
    // the IR itself stays pure.
    if let Some(scene) = &outcome.scene {
        diagnostics.extend(asset_warnings(
            scene,
            file.parent().unwrap_or(Path::new(".")),
        ));
    }

    report::emit(&name, &source, &diagnostics);
    if diagnostics.iter().any(|d| d.is_error()) {
        1
    } else {
        0
    }
}

pub fn parse(file: &Path) -> i32 {
    let (name, source) = match read_source(file) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let failed = has_error_diagnostics(&source);
    match compile_and_report(&name, &source) {
        Some(scene) if !failed => {
            let document = serde_json::json!({
                "format": IR_FORMAT,
                "scene": scene,
            });
            println!("{}", serde_json::to_string_pretty(&document).unwrap());
            0
        }
        _ => 1,
    }
}

const SCENE_TEMPLATE: &str = r##"<scene canvas="1080x1920" fps="30" clear="#0e0e12">
  <script track="voice" voice="narrator">
    <line id="hook">Your opening line goes here.</line>
    <line id="payoff">The payoff lands here.</line>
  </script>

  <track id="voice" kind="audio">
    <sound src="assets/narration.wav" during="hook..payoff"/>
  </track>

  <track kind="visual" anchor="voice">
    <image src="assets/backdrop.png" during="hook..payoff"/>
    <board during="payoff" at="center" anim="rise">
      <text bind="payoff.text"/>
    </board>
    <captions style="karaoke" anchor="voice.words"/>
  </track>

  <track kind="audio">
    <music src="assets/bed.mp3" gain="-14dB" duck="voice" during="hook..payoff"/>
  </track>

  <render target="out/final.mp4"/>
</scene>
"##;

const PROJECT_TOML: &str = r#"[project]
name = "scene"

[scene]
source = "main.scene"

[render]
target = "out/final.mp4"

# Capabilities — external services the scene may call. The engine sends
# the connector a JSON request; the connector writes the asset. Secrets
# resolve via env or the OS keychain and arrive as SCENE_CAP_AUTH.
#
# [capabilities.tts]
# command = ["sh", "connectors/elevenlabs-tts.sh"]
# auth = { env = "ELEVENLABS_API_KEY" }
#
# [capabilities.image]
# command = ["sh", "connectors/openai-image.sh"]
# auth = { env = "OPENAI_API_KEY" }
"#;

const PROJECT_GITIGNORE: &str = "cache/\nout/\n";

pub fn init(dir: &Path) -> i32 {
    let scene_path = dir.join("main.scene");
    if scene_path.exists() {
        eprintln!("error: {} already exists", scene_path.display());
        return 1;
    }
    let write = |path: PathBuf, contents: &str| -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(&path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
    };
    for sub in ["assets", "out", "cache"] {
        if let Err(e) = fs::create_dir_all(dir.join(sub)) {
            eprintln!("error: cannot create {}: {e}", dir.join(sub).display());
            return 1;
        }
    }
    let files = [
        (dir.join("scene.toml"), PROJECT_TOML),
        (scene_path, SCENE_TEMPLATE),
        (dir.join(".gitignore"), PROJECT_GITIGNORE),
    ];
    for (path, contents) in files {
        if let Err(e) = write(path, contents) {
            eprintln!("error: {e}");
            return 1;
        }
    }
    println!("initialized scene project in {}", dir.display());
    println!("  next: engine check {}", dir.join("main.scene").display());
    0
}

/// Produce a `--timings` JSON for a scene: markers file or WhisperX
/// output in, `TimingMap` document out (stdout unless `--out`).
pub fn align(
    file: &Path,
    markers_path: Option<&Path>,
    whisperx_path: Option<&Path>,
    out: Option<&Path>,
) -> i32 {
    let (name, source) = match read_source(file) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let Some(scene) = compile_and_report(&name, &source) else {
        return 1;
    };
    let Some(script) = scene.script.clone() else {
        eprintln!("error: {name} has no <script> — nothing to align");
        return 1;
    };

    // Two diagnostic sources: parse problems carry spans into the
    // markers file; timing problems carry script-line spans into the
    // scene (or none). Each renders against its own text.
    let timing = match (markers_path, whisperx_path) {
        (Some(m), None) => match fs::read_to_string(m) {
            Ok(text) => {
                let (markers, parse_diags) = parse_markers(&text);
                report::emit(&m.display().to_string(), &text, &parse_diags);
                if parse_diags.iter().any(Diagnostic::is_error) {
                    return 1;
                }
                let (timing, diags) = markers_to_timing(&markers, &script);
                report::emit(&name, &source, &diags);
                timing
            }
            Err(e) => {
                eprintln!("error: cannot read {}: {e}", m.display());
                return 1;
            }
        },
        (None, Some(w)) => match fs::read_to_string(w)
            .map_err(|e| e.to_string())
            .and_then(|json| whisperx_to_timing(&json, &script))
        {
            Ok((timing, diags)) => {
                report::emit(&name, &source, &diags);
                timing
            }
            Err(e) => {
                eprintln!("error: bad whisperx file {}: {e}", w.display());
                return 1;
            }
        },
        _ => {
            eprintln!("error: pass exactly one of --markers or --whisperx");
            return 1;
        }
    };

    let map = timing_map_for(&script, timing);
    let json = serde_json::to_string_pretty(&map).expect("TimingMap serializes");
    match out {
        Some(path) => {
            if let Err(e) = fs::write(path, &json) {
                eprintln!("error: cannot write {}: {e}", path.display());
                return 1;
            }
            println!("wrote {}", path.display());
        }
        None => println!("{json}"),
    }
    0
}

/// Render a scene to mp4: compile → realize → raster → NV12 → ffmpeg.
///
/// Diagnostics grouped with the source they point into — one bundle
/// per file that produced them (scene, markers, etc).
pub struct DiagBundle {
    pub name: String,
    pub source: String,
    pub diags: Vec<Diagnostic>,
}

/// What a successful render produced, plus every diagnostic raised
/// along the way (warnings included — the UI shows them).
pub struct RenderReport {
    pub frames: usize,
    pub target: PathBuf,
    pub bundles: Vec<DiagBundle>,
}

/// Why a render failed: a message, plus any diagnostic bundles gathered
/// before the failure (a `check`-stage failure carries them all).
pub struct RenderError {
    pub message: String,
    pub bundles: Vec<DiagBundle>,
}

/// Removes its file on drop — a program.wav left behind by a failed
/// render (mix error, encode error, even a partial mix) is just litter.
struct TempWav(PathBuf);
impl Drop for TempWav {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The render pipeline with reporting abstracted: diagnostics and
/// failures come back as data instead of prints, so the CLI can print
/// them and the UI can ship them as JSON.
pub fn render_inner(
    file: &Path,
    timings_path: Option<&Path>,
    out: Option<&Path>,
    frames: Option<&str>,
    workers: usize,
) -> Result<RenderReport, RenderError> {
    let stage = |msg: String, bundles: Vec<DiagBundle>| RenderError {
        message: msg,
        bundles,
    };
    let (name, source) = read_source(file).map_err(|e| stage(e.to_string(), Vec::new()))?;
    let mut bundles = Vec::new();

    let doc = match scene_markup::parse_document(&source) {
        Ok(d) => d,
        Err(d) => {
            return Err(stage(
                "parse error".to_string(),
                vec![DiagBundle {
                    name,
                    source,
                    diags: vec![d],
                }],
            ));
        }
    };
    let (scene, diags) = scene_markup::lower(&doc);
    let failed = scene_ir::has_errors(&diags);
    bundles.push(DiagBundle {
        name: name.clone(),
        source: source.clone(),
        diags,
    });
    let Some(scene) = scene.filter(|_| !failed) else {
        return Err(stage("scene has errors".to_string(), bundles));
    };

    let timings = match timings_path {
        Some(path) => {
            let loaded = fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|json| {
                    serde_json::from_str::<TimingMap>(&json).map_err(|e| e.to_string())
                });
            match loaded {
                Ok(t) => t,
                Err(e) => {
                    return Err(stage(
                        format!("cannot load timings {}: {e}", path.display()),
                        bundles,
                    ));
                }
            }
        }
        None => TimingMap::default(),
    };

    let (resolved, diags) = realize(&scene, &timings);
    let failed = scene_ir::has_errors(&diags);
    bundles.push(DiagBundle {
        name: name.clone(),
        source: source.clone(),
        diags,
    });
    let Some(resolved) = resolved.filter(|_| !failed) else {
        return Err(stage("timing resolution failed".to_string(), bundles));
    };

    let range = match frames {
        Some(spec) => match parse_frame_range(spec) {
            Some(r) => r,
            None => {
                return Err(stage(
                    format!("invalid --frames `{spec}` (use `start:end`, e.g. `0:90`)"),
                    bundles,
                ));
            }
        },
        None => resolved.program.frames.start..resolved.program.frames.end,
    };

    let target = match out.or_else(|| resolved_tracks_target(&scene)) {
        Some(t) => t.to_path_buf(),
        None => {
            return Err(stage(
                "no output target — pass --out or set <render target>".to_string(),
                bundles,
            ));
        }
    };
    if let Some(parent) = target.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Err(stage(
            format!("cannot create {}: {e}", parent.display()),
            bundles,
        ));
    }

    let project_root = file.parent().unwrap_or(Path::new(".")).to_path_buf();

    // Program audio: collect music/sound → 48kHz mix → muxed into the
    // same ffmpeg process as the video pipe. No audio elements = silent.
    let (graph, audio_diags) = AudioGraph::from_scene(&resolved, &project_root);
    bundles.push(DiagBundle {
        name,
        source,
        diags: audio_diags,
    });
    let program_wav = if graph.clips.is_empty() {
        None
    } else {
        let wav = target.with_extension("program.wav");
        let guard = TempWav(wav);
        if let Err(e) = mix_program(&graph, &guard.0) {
            return Err(stage(format!("audio mix failed: {e}"), bundles));
        }
        Some(guard)
    };

    let mut encoder = match Encoder::open_muxed_opt(
        &target,
        resolved.canvas.width,
        resolved.canvas.height,
        &resolved.frame_rate,
        program_wav.as_ref().map(|t| t.0.as_path()),
    ) {
        Ok(e) => e,
        Err(e) => return Err(stage(e.to_string(), bundles)),
    };
    let make = renderer_factory(project_root.clone());

    let rendered = match render_frames(&resolved, &timings, range, workers.max(1), &make) {
        Ok(r) => r,
        Err(e) => return Err(stage(format!("render worker failed: {e}"), bundles)),
    };
    let (w, h) = (resolved.canvas.width, resolved.canvas.height);
    for frame in &rendered {
        if let Err(e) = encoder.write_frame(&rgba_to_nv12(&frame.pixels, w, h)) {
            // The encoder opened with -y: the target is already truncated
            // — remove the corpse so nothing serves a partial video.
            let _ = fs::remove_file(&target);
            return Err(stage(
                format!("encode failed at frame {}: {e}", frame.index),
                bundles,
            ));
        }
    }
    if let Err(e) = encoder.finish() {
        let _ = fs::remove_file(&target);
        return Err(stage(e.to_string(), bundles));
    }
    Ok(RenderReport {
        frames: rendered.len(),
        target,
        bundles,
    })
}

/// `engine render` — the CLI view over [`render_inner`]: diagnostics
/// rendered with spans, failures as exit codes.
pub fn render(
    file: &Path,
    timings_path: Option<&Path>,
    out: Option<&Path>,
    frames: Option<&str>,
    workers: usize,
) -> i32 {
    match render_inner(file, timings_path, out, frames, workers) {
        Ok(report) => {
            for b in &report.bundles {
                report::emit(&b.name, &b.source, &b.diags);
            }
            println!(
                "rendered {} frame(s) → {}",
                report.frames,
                report.target.display()
            );
            0
        }
        Err(err) => {
            for b in &err.bundles {
                report::emit(&b.name, &b.source, &b.diags);
            }
            eprintln!("error: {}", err.message);
            1
        }
    }
}

fn parse_frame_range(spec: &str) -> Option<std::ops::Range<u32>> {
    // `N` alone is the single frame N; `n:m` is the half-open range.
    if let Some((start, end)) = spec.split_once(':') {
        let (start, end) = (start.trim().parse().ok()?, end.trim().parse().ok()?);
        (end > start).then_some(start..end)
    } else {
        let n = spec.trim().parse().ok()?;
        Some(n..n + 1)
    }
}

/// Pins the factory signature so `scene` and `timings` share the lifetime
/// `render_frames` unifies them under — a bare closure can't express that.
fn renderer_factory<'a>(
    project_root: PathBuf,
) -> impl Fn(&'a scene_time::ResolvedScene, &'a TimingMap) -> Renderer<'a> + Send + Sync + 'static {
    move |scene, timings| Renderer {
        scene,
        timings,
        measure: Box::new(RenderMeasure {
            scene,
            timings,
            text: Box::new(CosmicText::new()),
        }),
        text: Box::new(CosmicText::new()),
        clips: Box::new(SeqFrameSource::new(project_root.clone())),
        images: Box::new(StillFrameSource::new(project_root.clone())),
        programs: Box::new(SandboxPrograms::new(project_root.clone())),
    }
}

fn resolved_tracks_target(scene: &scene_ir::Scene) -> Option<&Path> {
    scene.render.as_ref().map(|r| Path::new(r.target.as_str()))
}

/// List the capabilities a scene.toml registers.
pub fn cap_list(config: &Path) -> i32 {
    match load_registry(config) {
        Ok(reg) => {
            let mut found = false;
            for name in reg.names() {
                found = true;
                let cap = reg.get(name).expect("listed");
                let kind = match &cap.connector {
                    scene_cap::Connector::Subprocess { argv } => {
                        format!("command: {}", argv.join(" "))
                    }
                    scene_cap::Connector::Http { endpoint } => {
                        format!("http: {endpoint}")
                    }
                };
                let auth = match &cap.auth {
                    Some(scene_cap::AuthRef::Env(v)) => format!("auth: env {v}"),
                    Some(scene_cap::AuthRef::Keychain { service, account }) => {
                        format!("auth: keychain {service}/{account}")
                    }
                    None => "auth: none".to_string(),
                };
                println!("{name}\t{kind}\t{auth}");
            }
            if !found {
                println!("no capabilities declared in {}", config.display());
            }
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// Invoke one capability through the real connector path.
pub fn cap_call(config: &Path, name: &str, params: &str, out: &Path) -> i32 {
    let reg = match load_registry(config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let params: serde_json::Value = match serde_json::from_str(params) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: --params is not valid JSON: {e}");
            return 1;
        }
    };
    match scene_cap::fulfill(
        &reg,
        &scene_cap::CapRequest {
            capability: name,
            params,
            out,
        },
    ) {
        Ok(path) => {
            println!("wrote {}", path.display());
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn load_registry(config: &Path) -> Result<scene_cap::Registry, String> {
    let text =
        fs::read_to_string(config).map_err(|e| format!("cannot read {}: {e}", config.display()))?;
    scene_cap::Registry::from_toml(&text)
}

/// Probe the external tools the engine will shell out to. Everything here
/// is optional at parse time but needed for real renders.
pub fn doctor() -> i32 {
    let tools = [
        ("ffmpeg", "video decode, audio mix, final encode"),
        ("ffprobe", "media inspection"),
        ("yt-dlp", "reference video download (adapt mode)"),
        ("uv", "python services: transcription, image ops"),
    ];
    let mut missing = 0;
    for (tool, role) in tools {
        match Command::new(tool).arg("-version").output() {
            Ok(out) => {
                let first = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                println!("  ok      {tool:<8} {first}");
            }
            Err(_) => {
                missing += 1;
                println!("  MISSING {tool:<8} ({role})");
            }
        }
    }
    if missing > 0 {
        println!("{missing} tool(s) missing — parsing still works; rendering needs them");
        return 1;
    }
    println!("all tools present");
    0
}

/// The src written into the draft scene: already-relative paths stay
/// (the draft resolves assets against the scene's own directory);
/// absolute paths shrink to relative when they sit under the cwd.
fn adapt_src(path: &Path) -> String {
    if path.is_relative() {
        return path.to_string_lossy().into_owned();
    }
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// `engine adapt <source>` — ingest, analyze, emit a draft .scene.
pub fn adapt(source: &str, out_dir: &Path, out: Option<&Path>) -> i32 {
    let ingested = match scene_adapt::ingest(source, out_dir) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("adapt: {e}");
            return 1;
        }
    };
    let analysis = match scene_adapt::analyze(&ingested.path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("adapt: analyze {}: {e}", ingested.path.display());
            return 1;
        }
    };
    let src = adapt_src(&ingested.path);
    let markup = scene_adapt::emit_scene(&src, &analysis);

    eprintln!(
        "adapt: {} — {:.2}s {}x{}, {} cut(s){}",
        ingested.path.display(),
        analysis.duration_s,
        analysis.width,
        analysis.height,
        analysis.cuts.len(),
        if analysis.has_audio {
            ", has audio"
        } else {
            ""
        }
    );
    match out {
        Some(path) => match std::fs::write(path, &markup) {
            Ok(()) => {
                println!("wrote {}", path.display());
                0
            }
            Err(e) => {
                eprintln!("adapt: write {}: {e}", path.display());
                1
            }
        },
        None => {
            print!("{markup}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_accepts_range_and_single() {
        assert_eq!(parse_frame_range("0:60"), Some(0..60));
        assert_eq!(parse_frame_range("5"), Some(5..6));
        assert_eq!(parse_frame_range(" 10 "), Some(10..11));
        assert_eq!(parse_frame_range("5:5"), None); // empty range still rejected
        assert_eq!(parse_frame_range("9:5"), None);
        assert_eq!(parse_frame_range("x"), None);
        assert_eq!(parse_frame_range("1:x"), None);
    }

    #[test]
    fn temp_wav_removes_its_file_on_drop() {
        let path = std::env::temp_dir().join(format!("tempwav-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        {
            let _guard = TempWav(path.clone());
        }
        assert!(!path.exists());
    }

    #[test]
    fn adapt_src_prefers_relative() {
        // Relative paths pass through unchanged.
        assert_eq!(adapt_src(Path::new("assets/v.mp4")), "assets/v.mp4");
        // Absolute paths under the cwd shrink to relative.
        let under = std::env::current_dir()
            .unwrap()
            .join("assets")
            .join("v.mp4");
        assert_eq!(adapt_src(&under), "assets/v.mp4");
        // Outside the cwd stays absolute.
        let outside = Path::new("/definitely/not/here.mp4");
        assert_eq!(adapt_src(outside), "/definitely/not/here.mp4");
    }
}
