//! Content-addressed flash-cut stages. Provider responses and generated assets
//! publish only after validation; progress is observable before external calls.

use crate::brief::Brief;
use crate::cache::{Cache, write_atomic};
use crate::embed::{EmbedOut, apply_embeddings, run_embed, tag_candidates};
use crate::emit::{EmitSpec, beat_spans, emit_flash_scene, materialize_beats};
use crate::error::MemeError;
use crate::gemini::{self, GeminiAnalysis, GeminiMode};
use crate::metrics::words_of;
use crate::pack::score_pack;
use crate::package::{Package, PackageOutcome};
use crate::peaks::{Candidate, pick_peaks};
use crate::perceive::{Perceive, perceive};
use crate::{jev, package};
use scene_cap::{Connector, Registry};
use scene_media::{StagedOutput, probe};
use scene_time::TimingMap;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Instant;

const MAX_RERUNS: usize = 2;

pub struct RunOpts<'a> {
    pub input: &'a Path,
    pub brief: &'a Brief,
    pub out: &'a Path,
    pub timings: TimingMap,
    pub registry: Option<&'a Registry>,
    pub beat_sec: f64,
    pub materialize: bool,
    pub gemini_mode: GeminiMode,
    pub rerun_window: Option<String>,
    pub music_src: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageLog {
    pub stage: &'static str,
    pub phase: &'static str,
    pub ms: u128,
    pub cache_hit: bool,
    pub note: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub video_sha256: String,
    pub frames_seen: usize,
    pub candidates: usize,
    pub keeps: Vec<String>,
    pub low_confidence: Vec<String>,
    pub decision: PackageOutcome,
    pub gemini: Option<GeminiAnalysis>,
    pub scene: PathBuf,
    pub warnings: Vec<String>,
    pub stages: Vec<StageLog>,
}

struct Recorder<'a> {
    stages: Vec<StageLog>,
    progress: &'a mut dyn FnMut(&StageLog),
}
impl Recorder<'_> {
    fn record(&mut self, stage: &'static str, start: Instant, hit: bool, note: String) {
        self.emit(StageLog {
            stage,
            phase: "complete",
            ms: start.elapsed().as_millis(),
            cache_hit: hit,
            note,
            input_tokens: None,
            output_tokens: None,
            estimated_cost_usd: None,
        });
    }
    fn emit(&mut self, event: StageLog) {
        (self.progress)(&event);
        self.stages.push(event);
    }
}

fn write_json(out: &Path, name: &str, value: &impl Serialize) -> Result<(), MemeError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| MemeError::Stage(e.to_string()))?;
    write_atomic(&out.join(format!("{name}.json")), &bytes)
}
fn key(stage: &str, value: &impl Serialize) -> String {
    Cache::key(
        stage,
        &[&serde_json::to_string(value).expect("validated stage inputs serialize")],
    )
}

/// The request contract and connector implementation are inputs; auth values are not.
fn connector_identity(registry: Option<&Registry>, name: &str) -> Value {
    match registry.and_then(|r| r.get(name)).map(|c| &c.connector) {
        Some(Connector::Http { endpoint }) => json!({"endpoint": endpoint.split('?').next()}),
        Some(Connector::Subprocess { argv }) => {
            let files: Vec<_> = argv
                .iter()
                .flat_map(|arg| [PathBuf::from(arg), PathBuf::from(arg).with_extension("py")])
                .filter(|arg| arg.is_file())
                .map(|arg| json!({"path":arg,"sha256":Cache::file_sha256(&arg).ok()}))
                .collect();
            json!({"argv":argv,"files":files,"gateway":std::env::var("JEV_ENDPOINT").ok()})
        }
        None => Value::Null,
    }
}

/// Import by content, never by a reused basename. Old scenes retain their assets.
fn place_source(input: &Path, out: &Path) -> Result<String, MemeError> {
    let hash = Cache::file_sha256(input)?;
    let ext = input.extension().and_then(|x| x.to_str()).unwrap_or("bin");
    let relative = format!("assets/{hash}.{ext}");
    let dest = out.join(&relative);
    if !dest.is_file() || Cache::file_sha256(&dest)? != hash {
        let staged = StagedOutput::new(&dest).map_err(MemeError::io(&dest))?;
        std::fs::copy(input, staged.path()).map_err(MemeError::io(input))?;
        staged.publish().map_err(MemeError::io(&dest))?;
    }
    Ok(relative)
}

fn cached<T: Serialize + DeserializeOwned>(
    cache: &Cache,
    key: &str,
    make: impl FnOnce(&Path) -> Result<T, MemeError>,
) -> Result<(T, bool), MemeError> {
    if let Some(value) = cache.get(key) {
        return Ok((value, true));
    }
    // Raw connector replies never occupy the normalized cache entry.
    let target = cache.path(key);
    let stage = StagedOutput::new(&target).map_err(MemeError::io(&target))?;
    let value = make(stage.path())?;
    cache.put(key, &value)?;
    Ok((value, false))
}

struct GeminiCtx<'a> {
    reg: &'a Registry,
    video: &'a Path,
    video_sha: &'a str,
    brief: &'a Brief,
    mode: GeminiMode,
    out: &'a Path,
    cache: &'a Cache,
}
impl GeminiCtx<'_> {
    fn analyze(
        &self,
        keeps: &[&Candidate],
        force: bool,
        log: &mut Recorder<'_>,
    ) -> Result<GeminiAnalysis, MemeError> {
        let mode = match self.mode {
            GeminiMode::Stills => "stills",
            GeminiMode::Windows => "windows",
        };
        let cache_key = key(
            "gemini",
            &json!({"video":self.video_sha,"keeps":keeps,"brief":self.brief,
            "mode":mode,"prompt_version":2,"connector":connector_identity(Some(self.reg), &self.brief.gemini_cap)}),
        );
        let started = Instant::now();
        if !force && let Some(a) = self.cache.get::<GeminiAnalysis>(&cache_key) {
            log.record("gemini", started, true, format!("{} {mode}", keeps.len()));
            return Ok(a);
        }
        let dir = self.out.join("analysis").join(&cache_key);
        let files = match self.mode {
            GeminiMode::Stills => gemini::materialize_stills(self.video, keeps, &dir)?,
            GeminiMode::Windows => {
                gemini::materialize_windows(self.video, keeps, &dir, self.brief)?
            }
        };
        let request = gemini::gemini_doc(self.brief, self.mode, &files);
        let image_count = match self.mode {
            GeminiMode::Stills => keeps.len() as f64,
            GeminiMode::Windows => {
                keeps.len() as f64 * self.brief.gemini_window_sec * self.brief.gemini_fps
            }
        };
        let input_tokens = (request["prompt"].as_str().unwrap_or("").len() as u64).div_ceil(4)
            + (image_count * 258.0).ceil() as u64;
        let output_tokens = u64::from(self.brief.max_output_tokens);
        let cost = |input: u64, output: u64| {
            self.brief
                .input_price_per_million
                .zip(self.brief.output_price_per_million)
                .map(|(a, b)| (a * input as f64 + b * output as f64) / 1_000_000.0)
        };
        log.emit(StageLog { stage:"gemini",phase:"preflight",ms:0,cache_hit:false,
            note:"approximate input tokens; output is configured cap; monetary estimate unavailable unless prices are configured".into(),
            input_tokens:Some(input_tokens),output_tokens:Some(output_tokens),estimated_cost_usd:cost(input_tokens,output_tokens) });
        let target = self.cache.path(&cache_key);
        let staging = StagedOutput::new(&target).map_err(MemeError::io(&target))?;
        let result = gemini::analyze(
            self.reg,
            &self.brief.gemini_cap,
            self.brief,
            self.mode,
            &files,
            staging.path(),
        )?;
        if result.beats.is_empty()
            || result
                .beats
                .iter()
                .any(|b| !b.t.is_finite() || !keeps.iter().any(|k| k.id == b.id))
        {
            return Err(MemeError::Stage(
                "gemini returned empty or unknown beats; retry this analysis".into(),
            ));
        }
        self.cache.put(&cache_key, &result)?;
        log.emit(StageLog {
            stage: "gemini",
            phase: "complete",
            ms: started.elapsed().as_millis(),
            cache_hit: false,
            note: format!("{} {mode}", keeps.len()),
            input_tokens: result.usage.as_ref().map(|u| u.input_tokens),
            output_tokens: result.usage.as_ref().map(|u| u.output_tokens),
            estimated_cost_usd: result
                .usage
                .as_ref()
                .and_then(|u| cost(u.input_tokens, u.output_tokens)),
        });
        Ok(result)
    }
}

pub fn run(opts: &RunOpts) -> Result<RunReport, MemeError> {
    run_with_progress(opts, &mut |_| {})
}

pub fn run_with_progress(
    opts: &RunOpts,
    progress: &mut dyn FnMut(&StageLog),
) -> Result<RunReport, MemeError> {
    let effective = opts.brief.effective()?;
    let brief = &effective;
    if !opts.beat_sec.is_finite() || opts.beat_sec < 1e-9 {
        return Err(MemeError::Brief(
            "beat-sec must be finite and positive".into(),
        ));
    }
    for timing in opts.timings.sources.values() {
        if !timing.validate().is_empty() {
            return Err(MemeError::Stage("invalid word timings".into()));
        }
    }
    let gemini_reg = opts.registry.filter(|r| r.get(&brief.gemini_cap).is_some());
    if gemini_reg.is_some() && brief.gemini_model.is_none() {
        return Err(MemeError::Brief(
            "configured Gemini requires gemini_model or GEMINI_MODEL".into(),
        ));
    }
    std::fs::create_dir_all(opts.out).map_err(MemeError::io(opts.out))?;
    let cache = Cache::new(opts.out);
    let mut log = Recorder {
        stages: Vec::new(),
        progress,
    };
    let mut warnings = Vec::new();
    let started = Instant::now();
    let info = probe(opts.input)?;
    if info.video.is_none() || !info.duration_s.is_finite() || info.duration_s <= 0.0 {
        return Err(MemeError::Stage(
            "meme requires a video with a positive finite duration".into(),
        ));
    }
    let video_sha = Cache::file_sha256(opts.input)?;
    log.record("probe", started, false, String::new());
    let started = Instant::now();
    let metrics_key = key(
        "metrics",
        &json!({"video":video_sha,"size":brief.perceive_size,"fps":brief.fps,"timings":opts.timings}),
    );
    let (mut p, hit): (Perceive, _) = cached(&cache, &metrics_key, |_| {
        perceive(opts.input, &info, brief, &opts.timings)
    })?;
    log.record(
        "perceive",
        started,
        hit,
        format!("{} frames, audio={}", p.frames.len(), p.has_audio),
    );
    let started = Instant::now();
    let mut embeddings = None;
    let mut vocab = Vec::new();
    let mut embedding_key = "dhash".to_string();
    if brief.encoder != "dhash" {
        embedding_key = key(
            "embed",
            &json!({"video":video_sha,"size":brief.perceive_size,"fps":brief.fps,
            "vocab":brief.tag_vocab,"model":brief.embedding_model,"weights":brief.weights_id,
            "connector":connector_identity(opts.registry,&brief.encoder)}),
        );
        let valid_cached = cache
            .get::<EmbedOut>(&embedding_key)
            .filter(|e| e.validate(p.frames.len(), brief.tag_vocab.len()).is_ok());
        let (e, hit) = match valid_cached {
            Some(e) => (e, true),
            None => {
                let target = cache.path(&embedding_key);
                let staging = StagedOutput::new(&target).map_err(MemeError::io(&target))?;
                let e = run_embed(
                    opts.registry,
                    opts.input,
                    brief,
                    p.frames.len(),
                    staging.path(),
                )?
                .ok_or_else(|| MemeError::Stage("embedding connector missing".into()))?;
                cache.put(&embedding_key, &e)?;
                (e, false)
            }
        };
        apply_embeddings(&mut p, &e.embeddings);
        vocab = e.vocab_embeddings;
        embeddings = Some(e.embeddings);
        log.record("embed", started, hit, String::new());
    } else {
        log.record("embed", started, true, "dhash mode".into());
    }
    let started = Instant::now();
    let peaks_key = key(
        "peaks",
        &json!({"metrics":metrics_key,"embeddings":embedding_key,"brief":brief}),
    );
    let (mut candidates, hit): (Vec<Candidate>, _) = cached(&cache, &peaks_key, |_| {
        Ok(pick_peaks(
            &p,
            embeddings.as_deref(),
            brief,
            &words_of(&opts.timings),
        ))
    })?;
    if let Some(e) = &embeddings {
        tag_candidates(&mut candidates, e, &vocab, &brief.tag_vocab, 4, 0.25);
    }
    log.record(
        "peaks",
        started,
        hit,
        format!("{} candidates", candidates.len()),
    );
    let started = Instant::now();
    let pack = score_pack(&candidates, &p, embeddings.as_deref(), brief);
    write_json(opts.out, "metrics", &p)?;
    write_json(opts.out, "peaks", &candidates)?;
    write_json(opts.out, "pack", &pack)?;
    log.record("pack", started, false, String::new());
    let started = Instant::now();
    let outcome = if let Some(reg) = opts.registry.filter(|r| r.get("jev").is_some()) {
        let route_key = key(
            "jev-route",
            &json!({"request":jev::route_doc(&pack,brief),"brief":brief,
            "connector":connector_identity(Some(reg),"jev")}),
        );
        let (outcome, hit) = cached(&cache, &route_key, |path| {
            jev::route(reg, "jev", &pack, brief, &candidates, path)
        })?;
        log.record("jev_route", started, hit, String::new());
        outcome
    } else {
        warnings.push("no jev capability — every candidate kept unrouted".into());
        log.record("jev_route", started, true, "offline".into());
        jev::RouteOutcome {
            keeps: candidates.iter().map(|c| c.id.clone()).collect(),
            ..Default::default()
        }
    };
    let keeps: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| outcome.keeps.contains(&c.id))
        .collect();
    write_json(opts.out, "keeps", &outcome.keeps)?;
    let ctx = gemini_reg.map(|reg| GeminiCtx {
        reg,
        video: opts.input,
        video_sha: &video_sha,
        brief,
        mode: opts.gemini_mode,
        out: opts.out,
        cache: &cache,
    });
    let mut analysis = match &ctx {
        Some(ctx) if !keeps.is_empty() => Some(ctx.analyze(&keeps, false, &mut log)?),
        _ => {
            log.record("gemini", Instant::now(), true, "offline or no keeps".into());
            None
        }
    };
    if let Some(id) = &opts.rerun_window {
        let ctx = ctx
            .as_ref()
            .ok_or_else(|| MemeError::Stage("rerun-window requires Gemini".into()))?;
        let keep = keeps
            .iter()
            .copied()
            .find(|k| &k.id == id)
            .ok_or_else(|| MemeError::Stage(format!("rerun window `{id}` is not a keep")))?;
        let update = ctx.analyze(&[keep], true, &mut log)?;
        analysis
            .get_or_insert_with(Default::default)
            .merge_window(id, update);
    }
    let decide = |analysis: &Option<GeminiAnalysis>,
                  log: &mut Recorder<'_>|
     -> Result<PackageOutcome, MemeError> {
        let started = Instant::now();
        if let (Some(reg), Some(a)) = (opts.registry.filter(|r| r.get("jev").is_some()), analysis) {
            let package_key = key(
                "jev-package",
                &json!({"request":package::package_doc(brief,&keeps,a),"brief":brief,
                "connector":connector_identity(Some(reg),"jev")}),
            );
            let (decision, hit) = cached(&cache, &package_key, |path| {
                package::package(reg, "jev", brief, &keeps, a, path)
            })?;
            log.record("jev_package", started, hit, String::new());
            Ok(decision)
        } else {
            log.record("jev_package", started, true, "offline".into());
            Ok(package::fallback_package(&keeps))
        }
    };
    let mut decision = decide(&analysis, &mut log)?;
    for _ in 0..MAX_RERUNS {
        let Package::RerunWindow { keep_id } = &decision.package else {
            break;
        };
        let Some(ctx) = &ctx else { break };
        let Some(keep) = keeps.iter().copied().find(|k| &k.id == keep_id) else {
            return Err(MemeError::Stage(format!(
                "rerun_window named unknown keep `{keep_id}`"
            )));
        };
        let update = ctx.analyze(&[keep], true, &mut log)?;
        analysis
            .get_or_insert_with(Default::default)
            .merge_window(keep_id, update);
        decision = decide(&analysis, &mut log)?;
    }
    if !outcome.low_confidence.is_empty() && decision.package == Package::Export {
        decision.package = Package::NeedMorePeaks;
        decision.note = "low-confidence routing needs human review".into();
    }
    if let Some(a) = &analysis {
        write_json(opts.out, "gemini", a)?;
    }
    write_json(opts.out, "package", &decision)?;
    let started = Instant::now();
    let scene_path = opts.out.join("meme.scene");
    let mut spans = beat_spans(&keeps, opts.beat_sec, info.duration_s);
    let scene_src = if opts.materialize {
        let beat_key = key("beats", &json!({"video":video_sha,"spans":spans}));
        let relative = Path::new("beats").join(beat_key);
        materialize_beats(opts.input, &mut spans, &opts.out.join(&relative))?;
        for span in &mut spans {
            if let Some(file) = &span.file {
                span.file = Some(relative.join(file.file_name().expect("beat filename")));
            }
        }
        String::new()
    } else {
        place_source(opts.input, opts.out)?
    };
    let music_src = opts
        .music_src
        .as_ref()
        .map(|path| place_source(Path::new(path), opts.out))
        .transpose()?;
    let video = info.video.as_ref().expect("validated video");
    let rate = video
        .frame_rate
        .map(|r| (u64::from(r.numerator), u64::from(r.denominator)));
    let spec = EmitSpec {
        src: &scene_src,
        canvas: (video.width, video.height),
        fps: rate,
        has_audio: info.audio.is_some(),
        music_src: music_src.as_deref(),
        render_target: "meme.mp4",
    };
    // Empty detections remain an explicit need-more-peaks report, not a false export.
    if spans.is_empty() {
        decision.package = Package::NeedMorePeaks;
        write_json(opts.out, "package", &decision)?;
    }
    write_atomic(&scene_path, emit_flash_scene(&spans, &spec).as_bytes())?;
    log.record("emit", started, false, format!("{} beats", spans.len()));
    let report = RunReport {
        video_sha256: video_sha,
        frames_seen: p.frames.len(),
        candidates: candidates.len(),
        keeps: outcome.keeps,
        low_confidence: outcome.low_confidence,
        decision,
        gemini: analysis,
        scene: scene_path,
        warnings,
        stages: log.stages,
    };
    write_json(opts.out, "report", &report)?;
    Ok(report)
}
