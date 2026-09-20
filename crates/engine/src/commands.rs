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
    CosmicText, RenderMeasure, Renderer, SeqFrameSource, StillFrameSource, render_frames_into,
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

/// Removes its file on drop — a program wav left behind by a failed
/// render (mix error, encode error, even a partial mix) is just litter.
struct TempWav(PathBuf);
impl Drop for TempWav {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A unique sibling path for intermediate output: `name.tag-PID-SEQ.ext`
/// next to `target`. Same-dir keeps the final `fs::rename` atomic; the
/// pid+seq pair keeps concurrent in-process renders from colliding, so a
/// temp path can never name (and `-y`-clobber) a file the user owns.
fn unique_sibling(target: &Path, tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let stem = target.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    target.with_file_name(format!("{stem}.{tag}-{pid}-{seq}.{ext}"))
}

/// Publish `tmp` as `target`, replacing any previous output. Runs only
/// after a fully successful render — a failed render leaves the prior
/// output file untouched. The temp must exist before `target` is
/// touched; unix `rename` replaces atomically, Windows needs a
/// remove-then-rename fallback.
fn publish(tmp: &Path, target: &Path) -> Result<(), String> {
    if !tmp.exists() {
        return Err(format!("render output {} is missing", tmp.display()));
    }
    match fs::rename(tmp, target) {
        Ok(()) => Ok(()),
        Err(first) => {
            if target.exists() {
                fs::remove_file(target)
                    .map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
                fs::rename(tmp, target).map_err(|e| {
                    format!("cannot move {} to {}: {e}", tmp.display(), target.display())
                })
            } else {
                Err(format!(
                    "cannot move {} to {}: {first}",
                    tmp.display(),
                    target.display()
                ))
            }
        }
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
    // Missing assets are non-fatal (placeholders draw), but they must be
    // loud — a silent hole reads as a successful render of broken input.
    bundles.push(DiagBundle {
        name: name.clone(),
        source: source.clone(),
        diags: asset_warnings(&scene, file.parent().unwrap_or(Path::new("."))),
    });

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

    // Stale-timing guard: `engine align` stamps the script fingerprint
    // into the timing document — if it doesn't match the live script,
    // the words were aligned against different text and captions would
    // show the old words without a word of complaint. Same bundle carries
    // word-time validation — a hand-edited or connector-produced file can
    // carry negative or reversed spans that parse fine but caption wrong.
    let mut timing_diags: Vec<Diagnostic> =
        stale_timing_warning(&scene, &timings).into_iter().collect();
    for source in timings.sources.values() {
        timing_diags.extend(
            source
                .validate()
                .into_iter()
                .map(|m| Diagnostic::warning(m, None)),
        );
    }
    if !timing_diags.is_empty() {
        bundles.push(DiagBundle {
            name: name.clone(),
            source: source.clone(),
            diags: timing_diags,
        });
    }

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

    let project_root = file.parent().unwrap_or(Path::new(".")).to_path_buf();

    // `<render target>` is authored markup, so it resolves against the
    // project root and may not escape it — an `../`/absolute target would
    // let a scene create dirs and `-y`-truncate files anywhere ffmpeg can
    // reach. `--out` is the operator's own argument and is used verbatim.
    let target = match out {
        Some(t) => t.to_path_buf(),
        None => match resolved_tracks_target(&scene) {
            Some(t) => match confine_target(&project_root, t) {
                Ok(t) => t,
                Err(e) => return Err(stage(e, bundles)),
            },
            None => {
                return Err(stage(
                    "no output target — pass --out or set <render target>".to_string(),
                    bundles,
                ));
            }
        },
    };
    if let Some(parent) = target.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Err(stage(
            format!("cannot create {}: {e}", parent.display()),
            bundles,
        ));
    }

    // Program audio: collect music/sound → 48kHz mix → muxed into the
    // same ffmpeg process as the video pipe. No audio elements = silent.
    // `--frames` selects a window of the program timeline — the mix must
    // be rebased onto it or a partial render plays the opening audio
    // under later frames.
    let (graph, audio_diags) = AudioGraph::from_scene(&resolved, &project_root);
    bundles.push(DiagBundle {
        name: name.clone(),
        source: source.clone(),
        diags: audio_diags,
    });
    let fps = resolved.frame_rate.to_f64();
    let sample_of = |frame: u32| (frame as f64 / fps * 48_000.0 + 1e-6).floor().max(0.0) as u64;
    let graph = graph.window(sample_of(range.start), sample_of(range.end));
    let program_wav = if graph.clips.is_empty() {
        None
    } else {
        // `.wav` — the mixer's ffmpeg picks its muxer by extension.
        let wav = unique_sibling(&target, "program").with_extension("wav");
        let guard = TempWav(wav);
        if let Err(e) = mix_program(&graph, &guard.0) {
            return Err(stage(format!("audio mix failed: {e}"), bundles));
        }
        Some(guard)
    };

    // Render into a unique sibling temp, publish on success — `-y` may
    // truncate the temp all it wants; the previous good output survives
    // any failed render.
    let tmp_target = unique_sibling(&target, "tmp");
    let mut encoder = match Encoder::open_muxed_opt(
        &tmp_target,
        resolved.canvas.width,
        resolved.canvas.height,
        &resolved.frame_rate,
        program_wav.as_ref().map(|t| t.0.as_path()),
    ) {
        Ok(e) => e,
        Err(e) => return Err(stage(e.to_string(), bundles)),
    };
    // Decode-time asset failures — a file that exists but won't open, a
    // corrupt png — are recorded by each worker's frame sources into one
    // shared sink, then reported once per asset.
    let warn_sink: scene_render::WarnSink = Default::default();
    let make = renderer_factory(project_root.clone(), warn_sink.clone());

    // Frames stream straight into the encoder as workers finish them —
    // no whole-video frame buffer. On any failure the pool cancels its
    // workers and joins them before returning Err; Encoder::drop kills
    // ffmpeg, and the truncated temp is removed — the real target is
    // only ever written by a successful publish.
    let (cw, ch) = (resolved.canvas.width, resolved.canvas.height);
    let frames_done =
        match render_frames_into(&resolved, &timings, range, workers.max(1), &make, |f| {
            encoder
                .write_frame(&rgba_to_nv12(&f.pixels, cw, ch))
                .map_err(|e| format!("encode failed at frame {}: {e}", f.index))
        }) {
            Ok(n) => n,
            Err(e) => {
                let _ = fs::remove_file(&tmp_target);
                return Err(stage(e, bundles));
            }
        };
    if let Ok(w) = warn_sink.lock()
        && !w.is_empty()
    {
        bundles.push(DiagBundle {
            name: name.clone(),
            source: source.clone(),
            diags: w
                .iter()
                .map(|m| Diagnostic::warning(m.clone(), None))
                .collect(),
        });
    }
    if let Err(e) = encoder.finish() {
        let _ = fs::remove_file(&tmp_target);
        return Err(stage(e.to_string(), bundles));
    }
    if let Err(e) = publish(&tmp_target, &target) {
        let _ = fs::remove_file(&tmp_target);
        return Err(stage(e, bundles));
    }
    Ok(RenderReport {
        frames: frames_done,
        target,
        bundles,
    })
}

/// Some when the timing file carries an align-time script fingerprint
/// that doesn't match the live script — same cue ids, different words.
/// Files with no fingerprint (hand-written, older) can't be checked.
fn stale_timing_warning(scene: &scene_ir::Scene, timings: &TimingMap) -> Option<Diagnostic> {
    let script = scene.script.as_ref()?;
    let hash = timings.script_hash.as_deref()?;
    (hash != scene_ir::script_fingerprint(script)).then(|| {
        Diagnostic::warning(
            "timings were aligned to a different script — re-run `engine align` \
             or captions will render stale words"
                .to_string(),
            None,
        )
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
        let n: u32 = spec.trim().parse().ok()?;
        n.checked_add(1).map(|end| n..end)
    }
}

/// Pins the factory signature so `scene` and `timings` share the lifetime
/// `render_frames` unifies them under — a bare closure can't express that.
fn renderer_factory<'a>(
    project_root: PathBuf,
    warnings: scene_render::WarnSink,
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
        clips: Box::new(SeqFrameSource::new(project_root.clone()).with_warnings(warnings.clone())),
        images: Box::new(
            StillFrameSource::new(project_root.clone()).with_warnings(warnings.clone()),
        ),
        programs: Box::new(
            SandboxPrograms::new(project_root.clone()).with_warnings(warnings.clone()),
        ),
    }
}

fn resolved_tracks_target(scene: &scene_ir::Scene) -> Option<&Path> {
    scene.render.as_ref().map(|r| Path::new(r.target.as_str()))
}

/// Resolve an authored `<render target>` against the project root and
/// refuse escapes — absolute targets, `..` components, and symlinks
/// inside the root pointing out are all markup trying to write files
/// ffmpeg shouldn't touch. Shared with `src` confinement in
/// `scene_media::confine_under_root`.
fn confine_target(root: &Path, target: &Path) -> Result<PathBuf, String> {
    scene_media::confine_under_root(root, target).map_err(|e| {
        format!(
            "render target `{}` escapes the project root (must land under {})",
            e.rel.display(),
            e.root.display()
        )
    })
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

/// Each tool's version flag — `yt-dlp`/`uv` take GNU-style `--version`;
/// the ffmpeg family takes its own `-version`. The wrong flag exits
/// nonzero, which must read as missing, not "ok".
fn version_arg(tool: &str) -> &'static str {
    match tool {
        "ffmpeg" | "ffprobe" => "-version",
        _ => "--version",
    }
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
        // A version probe exits instantly on a healthy binary; ten seconds
        // catches a wedged shim/wrapper instead of hanging doctor. The
        // status must be a success — a binary that errors on its own
        // version flag is present but unusable.
        match scene_media::output_timeout(
            Command::new(tool).arg(version_arg(tool)),
            tool,
            std::time::Duration::from_secs(10),
        ) {
            Ok(out) if out.status.success() => {
                let first = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                println!("  ok      {tool:<8} {first}");
            }
            Ok(out) => {
                missing += 1;
                println!(
                    "  MISSING {tool:<8} ({role}) — exits {status}",
                    status = out.status
                );
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

/// `path` split into components with `.`/`..` collapsed lexically —
/// made absolute first so the result is canonical without touching the
/// filesystem (a `..` over a symlink boundary stays the caller's
/// problem, same as any lexical resolver).
fn normalized_components(path: &Path) -> Vec<std::ffi::OsString> {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    // Leading RootDir/Prefix components — a `..` must never pop the anchor.
    let mut anchored = 0usize;
    for c in abs.components() {
        match c {
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                out.push(c.as_os_str().to_os_string());
                anchored = out.len();
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if out.len() > anchored && out.last().is_some_and(|p| p != "..") {
                    out.pop();
                } else {
                    out.push(c.as_os_str().to_os_string());
                }
            }
            std::path::Component::Normal(name) => out.push(name.to_os_string()),
        }
    }
    out
}

/// The src written into a draft printed to *stdout* — the scene's root
/// is unknown (the user decides where to save it), so the best we can
/// do is express the path against the cwd: relative when both endpoints
/// sit inside it, absolute otherwise. `engine adapt --out file.scene`
/// takes [`place_in_scene`] instead, which imports the footage so the
/// draft always renders under source confinement.
fn adapt_src(path: &Path, scene_dir: &Path) -> String {
    let a = normalized_components(scene_dir);
    let b = normalized_components(path);
    let inside = std::env::current_dir()
        .ok()
        .map(|cwd| normalized_components(&cwd))
        .is_some_and(|cwd| a.starts_with(&cwd) && b.starts_with(&cwd));
    if inside {
        let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        let mut rel = PathBuf::new();
        for _ in common..a.len() {
            rel.push("..");
        }
        rel.extend(b[common..].iter().map(|c| c.as_os_str()));
        rel.to_string_lossy().into_owned()
    } else {
        b.iter().collect::<PathBuf>().to_string_lossy().into_owned()
    }
}

/// Express `path` as a src usable inside `scene_dir`'s project root.
/// Source confinement refuses anything outside the root, so footage
/// that isn't already inside gets *imported* into `scene_dir/assets/`
/// — a draft written next to its own assets always renders. `relocate`
/// moves the file instead of copying, for media the engine itself just
/// produced (a yt-dlp download the draft can claim outright).
fn place_in_scene(path: &Path, scene_dir: &Path, relocate: bool) -> std::io::Result<PathBuf> {
    let root = scene_dir.canonicalize()?;
    let src_abs = path.canonicalize()?;
    let placed = if src_abs.starts_with(&root) {
        src_abs
    } else {
        import_media(&src_abs, scene_dir, relocate)?.canonicalize()?
    };
    Ok(placed.strip_prefix(&root).unwrap_or(&placed).to_path_buf())
}

/// Copy (or move) `path` into `<scene_dir>/assets/`, never overwriting
/// an unrelated file — `name-2.ext`, `name-3.ext`, … until a free slot.
/// A candidate that *is* the source (same file, already imported) is
/// reused as-is. Returns the absolute path the file landed at.
fn import_media(path: &Path, scene_dir: &Path, relocate: bool) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind};
    let assets = scene_dir.join("assets");
    std::fs::create_dir_all(&assets)?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "source has no file name"))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "media".into());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 0..100u32 {
        let candidate = assets.join(if n == 0 {
            name.to_os_string()
        } else {
            match &ext {
                Some(e) => format!("{stem}-{n}.{e}").into(),
                None => format!("{stem}-{n}").into(),
            }
        });
        if candidate.exists() {
            // Same file already sitting there? Then it's imported
            // already — reuse it rather than duplicating.
            if candidate.canonicalize().ok().as_deref() == Some(path) {
                return Ok(candidate);
            }
            continue;
        }
        if relocate {
            match std::fs::rename(path, &candidate) {
                Ok(()) => return Ok(candidate),
                // Cross-device — copy, then drop the original download.
                Err(_) => {
                    std::fs::copy(path, &candidate)?;
                    let _ = std::fs::remove_file(path);
                    return Ok(candidate);
                }
            }
        }
        std::fs::copy(path, &candidate)?;
        return Ok(candidate);
    }
    Err(Error::new(
        ErrorKind::AlreadyExists,
        "assets/ has no free name for the import",
    ))
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
    // The draft resolves assets against its own directory. With `--out`
    // the footage is imported under that root so the emitted src never
    // escapes it; a stdout draft has no root yet — express the source
    // against the cwd instead.
    let src = match out
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
    {
        Some(scene_dir) => {
            if let Err(e) = std::fs::create_dir_all(scene_dir) {
                eprintln!("adapt: mkdir {}: {e}", scene_dir.display());
                return 1;
            }
            match place_in_scene(&ingested.path, scene_dir, ingested.fetched) {
                Ok(rel) => {
                    let rel = rel.to_string_lossy().into_owned();
                    if rel != ingested.path.to_string_lossy() {
                        eprintln!("adapt: imported footage at {rel}");
                    }
                    rel
                }
                Err(e) => {
                    eprintln!("adapt: import {}: {e}", ingested.path.display());
                    return 1;
                }
            }
        }
        None => adapt_src(&ingested.path, Path::new(".")),
    };
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
        // u32::MAX must not wrap the implicit `n..n+1`.
        assert_eq!(parse_frame_range("4294967295"), None);
    }

    #[test]
    fn confine_target_keeps_writes_inside_the_project() {
        let root = Path::new(".");
        // confine_target returns canonical paths — compare against the
        // canonical cwd so a symlinked cwd can't break the assert.
        let cwd = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        // Relative target resolves under the project root.
        assert_eq!(
            confine_target(root, Path::new("out/final.mp4")).unwrap(),
            cwd.join("out/final.mp4")
        );
        // `..` and absolute-outside targets are refused, not clamped.
        assert!(confine_target(root, Path::new("../escape.mp4")).is_err());
        assert!(confine_target(root, Path::new("/tmp/escape.mp4")).is_err());
        assert!(confine_target(root, Path::new("out/../../escape.mp4")).is_err());
        // An absolute path *inside* the root is still allowed.
        let inside = cwd.join("sub").join("x.mp4");
        assert_eq!(confine_target(root, &inside).unwrap(), inside);
        // A symlink inside the root pointing out is refused too.
        #[cfg(unix)]
        {
            let tmp = std::env::temp_dir().join(format!("confine-sym-{}", std::process::id()));
            fs::create_dir_all(&tmp).unwrap();
            let link = cwd.join(format!("confine-link-{}", std::process::id()));
            std::os::unix::fs::symlink(&tmp, &link).unwrap();
            let t = format!("confine-link-{}/x.mp4", std::process::id());
            // Clean up before asserting so a failure leaves no stray
            // symlink inside the project root.
            let result = confine_target(root, Path::new(&t));
            let _ = fs::remove_file(&link);
            let _ = fs::remove_dir_all(&tmp);
            assert!(result.is_err());
        }
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
    fn unique_sibling_is_unique_and_keeps_extension() {
        let target = Path::new("out/final.mp4");
        let a = unique_sibling(target, "tmp");
        let b = unique_sibling(target, "tmp");
        assert_ne!(a, b);
        assert_eq!(a.extension().unwrap(), "mp4");
        assert_eq!(a.parent().unwrap(), Path::new("out"));
        assert!(
            a.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("final.tmp-")
        );
    }

    #[test]
    fn publish_replaces_only_on_success() {
        let dir = std::env::temp_dir().join(format!("publish-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("final.mp4");
        let tmp = unique_sibling(&target, "tmp");
        fs::write(&target, b"old-good").unwrap();
        fs::write(&tmp, b"new-good").unwrap();
        publish(&tmp, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new-good");
        assert!(!tmp.exists());
        // A publish failure (missing temp) leaves the old output alone.
        let gone = dir.join("gone.mp4");
        assert!(publish(&gone, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"new-good");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn adapt_src_resolves_against_the_cwd_for_stdout() {
        let cwd = std::env::current_dir().unwrap();
        // Same-dir case: source beside the scene stays a bare name.
        assert_eq!(
            adapt_src(Path::new("assets/v.mp4"), Path::new("assets")),
            "v.mp4"
        );
        // A nested --out climbs back: `adapt source.mp4 --out
        // nested/draft.scene` must emit `../source.mp4`, not a path that
        // only resolves from the cwd.
        assert_eq!(
            adapt_src(Path::new("source.mp4"), Path::new("nested")),
            "../source.mp4"
        );
        // Sibling trees climb then descend.
        assert_eq!(
            adapt_src(Path::new("assets/v.mp4"), Path::new("nested/deep")),
            "../../assets/v.mp4"
        );
        // Outside the project tree → absolute, which resolves anywhere.
        let outside = Path::new("/definitely/not/here.mp4");
        assert_eq!(
            adapt_src(outside, Path::new(".")),
            "/definitely/not/here.mp4"
        );
        // Absolute source inside the project still relativizes.
        let under = cwd.join("assets").join("v.mp4");
        assert_eq!(adapt_src(&under, Path::new(".")), "assets/v.mp4");
    }

    /// The review repro: `adapt clip.mp4 --out nested/draft.scene` used
    /// to emit `src="../clip.mp4"` — which source confinement refuses at
    /// render. Now footage outside the scene root is *imported* into the
    /// project's assets/, and the src stays inside.
    #[test]
    fn place_in_scene_imports_outside_footage() {
        let dir = std::env::temp_dir().join(format!("adapt-import-{}", std::process::id()));
        let scene_dir = dir.join("nested");
        let src = dir.join("clip.mp4");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&src, b"footage").unwrap();
        fs::create_dir_all(&scene_dir).unwrap();
        let rel = place_in_scene(&src, &scene_dir, false).unwrap();
        assert_eq!(rel, Path::new("assets").join("clip.mp4"));
        // Copied, not moved — the user's original stays put.
        assert_eq!(fs::read(&src).unwrap(), b"footage");
        assert_eq!(
            fs::read(scene_dir.join("assets/clip.mp4")).unwrap(),
            b"footage"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn place_in_scene_reuses_inside_footage() {
        let dir = std::env::temp_dir().join(format!("adapt-inside-{}", std::process::id()));
        let scene_dir = dir.join("proj");
        let src = scene_dir.join("media/clip.mp4");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, b"footage").unwrap();
        let rel = place_in_scene(&src, &scene_dir, false).unwrap();
        assert_eq!(rel, Path::new("media").join("clip.mp4"));
        // Nothing copied — no assets/ dir created for inside sources.
        assert!(!scene_dir.join("assets").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_media_never_clobbers_an_unrelated_file() {
        let dir = std::env::temp_dir().join(format!("adapt-collide-{}", std::process::id()));
        let scene_dir = dir.join("proj");
        let existing = scene_dir.join("assets/clip.mp4");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, b"someone-else").unwrap();
        let src = dir.join("clip.mp4");
        fs::write(&src, b"footage").unwrap();
        let placed = import_media(&src, &scene_dir, false).unwrap();
        assert!(placed.ends_with("clip-1.mp4"), "{}", placed.display());
        // The unrelated file is untouched.
        assert_eq!(fs::read(&existing).unwrap(), b"someone-else");
        assert_eq!(fs::read(&placed).unwrap(), b"footage");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn place_in_scene_relocates_fetched_footage() {
        let dir = std::env::temp_dir().join(format!("adapt-move-{}", std::process::id()));
        let scene_dir = dir.join("proj");
        let download = dir.join("dl/adapt-source.mp4");
        fs::create_dir_all(download.parent().unwrap()).unwrap();
        fs::create_dir_all(&scene_dir).unwrap();
        fs::write(&download, b"fetched").unwrap();
        let rel = place_in_scene(&download, &scene_dir, true).unwrap();
        assert_eq!(rel, Path::new("assets").join("adapt-source.mp4"));
        // Moved — the download dir is empty of the file.
        assert!(!download.exists());
        assert_eq!(
            fs::read(scene_dir.join("assets/adapt-source.mp4")).unwrap(),
            b"fetched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn doctor_uses_each_tools_own_version_flag() {
        assert_eq!(version_arg("ffmpeg"), "-version");
        assert_eq!(version_arg("ffprobe"), "-version");
        assert_eq!(version_arg("yt-dlp"), "--version");
        assert_eq!(version_arg("uv"), "--version");
    }

    fn scene_with_script(text: &str) -> scene_ir::Scene {
        let src = format!(
            r##"<scene canvas="1x1" fps="30">
  <track id="voice" kind="audio"><sound src="n.wav" during="hook"/></track>
  <script track="voice"><line id="hook">{text}</line></script>
</scene>"##
        );
        let doc = scene_markup::parse_document(&src).unwrap();
        let (scene, diags) = scene_markup::lower(&doc);
        assert!(!scene_ir::has_errors(&diags), "{diags:?}");
        scene.unwrap()
    }

    #[test]
    fn stale_timing_fingerprint_warns() {
        // Timings aligned to "Hello." must not silently serve a scene
        // whose hook now says different words.
        let scene = scene_with_script("Hello.");
        let mut timings = TimingMap {
            script_hash: Some(scene_ir::script_fingerprint(scene.script.as_ref().unwrap())),
            ..Default::default()
        };
        assert!(stale_timing_warning(&scene, &timings).is_none());

        let edited = scene_with_script("Totally different words.");
        assert!(
            stale_timing_warning(&edited, &timings).is_some(),
            "edited script must trip the fingerprint check"
        );
        // No fingerprint at all → nothing to compare, no warning.
        timings.script_hash = None;
        assert!(stale_timing_warning(&edited, &timings).is_none());
        // No script → nothing stale can exist.
        let mut bare = scene_with_script("x");
        bare.script = None;
        assert!(stale_timing_warning(&bare, &timings).is_none());
    }
}
