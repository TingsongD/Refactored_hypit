//! `run` — the pipeline in order: probe → perceive → (embed) → peaks →
//! pack → jev_route → gemini → jev_package → emit. Every stage reads
//! its inputs' content hash into a cache key, so a retry reuses exactly
//! the stages whose inputs haven't moved. Absent `jev`/`gemini`
//! capabilities degrade the run, not fail it: keeps become every
//! peak-pick survivor and the package decision falls back to code.

use std::path::{Path, PathBuf};
use std::time::Instant;

use scene_cap::Registry;
use scene_media::probe;
use scene_time::{TimingMap, Word};
use serde::Serialize;

use crate::brief::Brief;
use crate::cache::Cache;
use crate::embed::{apply_embeddings, run_embed, tag_candidates};
use crate::emit::{EmitSpec, beat_spans, emit_flash_scene, materialize_beats};
use crate::error::MemeError;
use crate::gemini::{self, GeminiAnalysis, GeminiMode, KeepFile};
use crate::jev;
use crate::metrics::words_of;
use crate::pack::score_pack;
use crate::package::{self, Package, PackageOutcome};
use crate::peaks::{Candidate, pick_peaks};
use crate::perceive::{Perceive, perceive};

/// Package-driven window reruns are bounded — a connector that always
/// says rerun can't spin forever.
const MAX_RERUNS: usize = 2;

pub struct RunOpts<'a> {
    pub input: &'a Path,
    pub brief: &'a Brief,
    /// Where metrics/peaks/keeps/gemini/package/scene land, plus `.cache/`.
    pub out: &'a Path,
    /// Word lattice from `engine align` — empty map = no word snapping.
    pub timings: TimingMap,
    /// Capability registry — `None` runs the whole pipeline offline.
    pub registry: Option<&'a Registry>,
    /// Program seconds per beat.
    pub beat_sec: f64,
    /// Physically cut beats to `out/beats/` instead of `from` offsets.
    pub materialize: bool,
    pub gemini_mode: GeminiMode,
    /// Force a one-window Gemini rerun on this keep id (retry entry point).
    pub rerun_window: Option<String>,
    /// Optional bed under the montage.
    pub music_src: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StageLog {
    pub stage: &'static str,
    pub ms: u128,
    pub cache_hit: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
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

/// JSON files the run writes at `out/` (mirrors the spec's §11 layout).
fn write_json(out: &Path, name: &str, v: &impl Serialize) -> Result<(), MemeError> {
    let path = out.join(format!("{name}.json"));
    let text = serde_json::to_string_pretty(v)
        .map_err(|e| MemeError::Stage(format!("serialize {name}: {e}")))?;
    write_atomic(&path, text.as_bytes())
}

/// Temp-file + rename — a crash mid-write can't leave a half document.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), MemeError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(MemeError::io(dir))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(MemeError::io(&tmp))?;
    std::fs::rename(&tmp, path).map_err(MemeError::io(path))?;
    Ok(())
}

/// The scene's `src` must resolve under the scene's own dir at render
/// time (confinement). Copy the input into `out/` as `source.<ext>`
/// unless it's already inside — same trick `adapt` uses.
fn place_source(input: &Path, out: &Path) -> Result<String, MemeError> {
    let canon_in = input.canonicalize().unwrap_or_else(|_| input.to_path_buf());
    let canon_out = out.canonicalize().unwrap_or_else(|_| out.to_path_buf());
    if canon_in.starts_with(&canon_out) {
        return canon_in
            .strip_prefix(&canon_out)
            .map(|r| r.display().to_string())
            .map_err(|_| MemeError::Stage("source path strip".into()));
    }
    let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    let dest = out.join(format!("source.{ext}"));
    if !dest.exists() {
        std::fs::copy(input, &dest).map_err(MemeError::io(&dest))?;
    }
    Ok(format!("source.{ext}"))
}

fn hash_str(s: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(s.as_bytes()))
        .chars()
        .take(16)
        .collect()
}

fn materialize(
    mode: GeminiMode,
    video: &Path,
    keeps: &[&Candidate],
    out: &Path,
    brief: &Brief,
) -> Result<Vec<KeepFile>, MemeError> {
    match mode {
        GeminiMode::Stills => gemini::materialize_stills(video, keeps, &out.join("frames")),
        GeminiMode::Windows => {
            gemini::materialize_windows(video, keeps, &out.join("windows"), brief)
        }
    }
}

/// Everything a Gemini call needs — bundled so both the full pass and
/// single-window reruns share one shape.
struct GeminiCtx<'a> {
    reg: &'a Registry,
    video: &'a Path,
    brief: &'a Brief,
    mode: GeminiMode,
    out: &'a Path,
    cache: &'a Cache,
    model_tag: &'a str,
}

impl GeminiCtx<'_> {
    /// Gemini for one keep — the unit of a `rerun_window` retry. Its
    /// cache key covers just that keep, so a window rerun can't disturb
    /// the rest.
    fn one(&self, keep: &Candidate) -> Result<(GeminiAnalysis, bool), MemeError> {
        let key = Cache::key(
            "gemini-win",
            &[&keep.id, &self.brief.hash(), self.model_tag],
        );
        if let Some(a) = self.cache.get::<GeminiAnalysis>(&key) {
            return Ok((a, true));
        }
        let files = materialize(self.mode, self.video, &[keep], self.out, self.brief)?;
        let a = gemini::analyze(
            self.reg,
            &self.brief.gemini_cap,
            self.brief,
            self.mode,
            &files,
            &self.cache.path(&key),
        )?;
        self.cache.put(&key, &a)?;
        Ok((a, false))
    }
}

pub fn run(opts: &RunOpts) -> Result<RunReport, MemeError> {
    let brief = opts.brief;
    brief.validate()?;
    std::fs::create_dir_all(opts.out).map_err(MemeError::io(opts.out))?;
    let cache = Cache::new(opts.out);
    let mut stages = Vec::new();
    let mut warnings = Vec::new();
    let mut tick = |stage: &'static str, start: Instant, hit: bool, note: String| {
        stages.push(StageLog {
            stage,
            ms: start.elapsed().as_millis(),
            cache_hit: hit,
            note,
        });
    };

    // ---- probe + identity ------------------------------------------------
    let s0 = Instant::now();
    let info = probe(opts.input)?;
    let video_sha = Cache::file_sha256(opts.input)?;
    let brief_hash = brief.hash();
    let timings_json = serde_json::to_string(&opts.timings).unwrap_or_default();
    let timings_hash = hash_str(&timings_json);
    tick("probe", s0, false, String::new());

    // ---- perceive --------------------------------------------------------
    // Cached pre-embeddings: `change` is dHash space here; the embed stage
    // rewrites it, and the peaks key carries the encoder so the two spaces
    // can never collide downstream.
    let s = Instant::now();
    let metrics_key = Cache::key(
        "metrics",
        &[
            &video_sha,
            &brief.encoder,
            &brief.perceive_size.to_string(),
            &brief.fps.to_string(),
            &timings_hash,
        ],
    );
    let (mut p, hit) = match cache.get::<Perceive>(&metrics_key) {
        Some(p) => (p, true),
        None => {
            let p = perceive(opts.input, &info, brief, &opts.timings)?;
            cache.put(&metrics_key, &p)?;
            (p, false)
        }
    };
    tick(
        "perceive",
        s,
        hit,
        format!("{} frames, audio={}", p.frames.len(), p.has_audio),
    );

    // ---- embed (optional) -------------------------------------------------
    let s = Instant::now();
    let mut emb: Option<Vec<Vec<f32>>> = None;
    let mut vocab_emb: Vec<Vec<f32>> = Vec::new();
    if brief.encoder != "dhash" {
        let key = Cache::key("emb", &[&video_sha, &brief.encoder, &brief.fps.to_string()]);
        let e = match cache.get::<crate::embed::EmbedOut>(&key) {
            Some(e) => {
                tick("embed", s, true, String::new());
                Some(e)
            }
            // The connector writes its response to the cache path —
            // a successful miss is already cached.
            None => {
                let e = run_embed(
                    opts.registry,
                    opts.input,
                    brief,
                    p.frames.len(),
                    &cache.path(&key),
                )?;
                tick("embed", s, false, String::new());
                e
            }
        };
        if let Some(e) = e {
            vocab_emb = e.vocab_embeddings.clone();
            emb = Some(e.embeddings);
            apply_embeddings(&mut p, emb.as_deref().unwrap_or_default());
        }
    } else {
        tick("embed", s, true, "dhash mode".into());
    }

    // ---- peaks -------------------------------------------------------------
    let s = Instant::now();
    let words: Vec<Word> = words_of(&opts.timings);
    let peaks_key = Cache::key(
        "peaks",
        &[&video_sha, &brief.encoder, &brief_hash, &timings_hash],
    );
    let mut candidates: Vec<Candidate> = match cache.get(&peaks_key) {
        Some(c) => {
            tick("peaks", s, true, String::new());
            c
        }
        None => {
            let c = pick_peaks(&p, emb.as_deref(), brief, &words);
            cache.put(&peaks_key, &c)?;
            tick("peaks", s, false, format!("{} candidates", c.len()));
            c
        }
    };
    // Zero-shot tags: top-4 labels at ≥0.25 cosine — fact-sheet fodder,
    // never sent to Jev as vectors.
    if !vocab_emb.is_empty()
        && let Some(e) = &emb
    {
        tag_candidates(&mut candidates, e, &vocab_emb, &brief.tag_vocab, 4, 0.25);
    }

    // ---- pack ----------------------------------------------------------------
    let s = Instant::now();
    let pack = score_pack(&candidates, &p, emb.as_deref(), brief);
    write_json(opts.out, "metrics", &p)?;
    write_json(opts.out, "peaks", &candidates)?;
    write_json(opts.out, "pack", &pack)?;
    tick("pack", s, false, String::new());

    // ---- jev route -------------------------------------------------------------
    let s = Instant::now();
    let outcome = match opts.registry.filter(|r| r.get("jev").is_some()) {
        Some(reg) => {
            let key = Cache::key("jev_route", &[&hash_str(&pack.to_string()), &brief_hash]);
            let out = match cache.get::<jev::RouteOutcome>(&key) {
                Some(o) => {
                    tick("jev_route", s, true, String::new());
                    o
                }
                None => {
                    // The connector writes its raw reply to the cache
                    // path; overwrite it with the parsed outcome so a
                    // cached hit deserializes into the same shape.
                    let o = jev::route(reg, "jev", &pack, brief, &candidates, &cache.path(&key))?;
                    cache.put(&key, &o)?;
                    tick("jev_route", s, false, String::new());
                    o
                }
            };
            if !out.low_confidence.is_empty() {
                warnings.push(format!(
                    "jev low confidence on {} — not auto-exporting",
                    out.low_confidence.join(",")
                ));
            }
            out
        }
        None => {
            let mut answers = std::collections::BTreeMap::new();
            let mut o = jev::RouteOutcome::default();
            for c in &candidates {
                o.keeps.push(c.id.clone());
                answers.insert(
                    c.id.clone(),
                    jev::RouteAnswer {
                        route: jev::Route::Keep,
                        cut_strength: 5,
                        too_similar: 0.0,
                        confidence: 1.0,
                    },
                );
            }
            o.answers = answers;
            warnings.push("no jev capability — every candidate kept unrouted".into());
            tick("jev_route", s, true, "offline".into());
            o
        }
    };
    let mut keep_refs: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| outcome.keeps.contains(&c.id))
        .collect();
    keep_refs.sort_by(|a, b| a.t.total_cmp(&b.t));
    write_json(opts.out, "keeps", &outcome.keeps)?;

    // ---- gemini ------------------------------------------------------------------
    let s = Instant::now();
    let model_tag = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "unset".into());
    let mut analysis: Option<GeminiAnalysis> = None;
    if let Some(reg) = opts.registry.filter(|r| r.get(&brief.gemini_cap).is_some()) {
        let gctx = GeminiCtx {
            reg,
            video: opts.input,
            brief,
            mode: opts.gemini_mode,
            out: opts.out,
            cache: &cache,
            model_tag: &model_tag,
        };
        if keep_refs.is_empty() {
            warnings.push("no keeps — gemini skipped".into());
        } else {
            // Forced single-window retry: `--rerun-window f371`.
            if let Some(id) = &opts.rerun_window {
                let keep = keep_refs
                    .iter()
                    .copied()
                    .find(|k| &k.id == id)
                    .ok_or_else(|| {
                        MemeError::Stage(format!("rerun window `{id}` is not a keep"))
                    })?;
                let (a, hit) = gctx.one(keep)?;
                tick("gemini", s, hit, format!("rerun {id}"));
                analysis = Some(a);
            } else {
                let ids: Vec<String> = keep_refs.iter().map(|k| k.id.clone()).collect();
                let keeps_hash = hash_str(&ids.join(","));
                let key = Cache::key(
                    "gemini",
                    &[
                        &keeps_hash,
                        &brief_hash,
                        &model_tag,
                        opts.gemini_mode_label(),
                    ],
                );
                match cache.get::<GeminiAnalysis>(&key) {
                    Some(a) => {
                        tick("gemini", s, true, String::new());
                        analysis = Some(a);
                    }
                    None => {
                        let files =
                            materialize(opts.gemini_mode, opts.input, &keep_refs, opts.out, brief)?;
                        let a = gemini::analyze(
                            reg,
                            "gemini",
                            brief,
                            opts.gemini_mode,
                            &files,
                            &cache.path(&key),
                        )?;
                        cache.put(&key, &a)?;
                        tick(
                            "gemini",
                            s,
                            false,
                            format!("{} {}", files.len(), opts.gemini_mode_label()),
                        );
                        analysis = Some(a);
                    }
                }
            }
            if let Some(a) = &analysis {
                write_json(opts.out, "gemini", a)?;
            }
        }
    } else {
        tick("gemini", s, true, "offline".into());
    }

    // ---- jev package --------------------------------------------------------------
    let s = Instant::now();
    let mut decision = match (opts.registry.filter(|r| r.get("jev").is_some()), &analysis) {
        (Some(reg), Some(a)) => {
            let key = Cache::key(
                "jev_package",
                &[
                    &hash_str(&serde_json::to_string(a).unwrap_or_default()),
                    &brief_hash,
                ],
            );
            match cache.get::<PackageOutcome>(&key) {
                Some(d) => {
                    tick("jev_package", s, true, String::new());
                    d
                }
                None => {
                    let d = package::package(reg, "jev", brief, &keep_refs, a, &cache.path(&key))?;
                    cache.put(&key, &d)?;
                    tick("jev_package", s, false, String::new());
                    d
                }
            }
        }
        _ => {
            let d = package::fallback_package(&keep_refs);
            tick("jev_package", s, true, "offline".into());
            d
        }
    };

    // Bounded rerun_window loop — re-analyze only the named keep's window.
    for _ in 0..MAX_RERUNS {
        let Package::RerunWindow { keep_id } = &decision.package else {
            break;
        };
        let Some(reg) = opts.registry.filter(|r| r.get(&brief.gemini_cap).is_some()) else {
            warnings.push("rerun_window requested but no gemini capability".into());
            break;
        };
        let Some(keep) = keep_refs.iter().copied().find(|k| &k.id == keep_id) else {
            warnings.push(format!("rerun_window named unknown keep `{keep_id}`"));
            break;
        };
        let gctx = GeminiCtx {
            reg,
            video: opts.input,
            brief,
            mode: opts.gemini_mode,
            out: opts.out,
            cache: &cache,
            model_tag: &model_tag,
        };
        let s = Instant::now();
        let (a, hit) = gctx.one(keep)?;
        tick("gemini_rerun", s, hit, keep_id.clone());
        analysis = Some(a.clone());
        write_json(opts.out, "gemini", &a)?;
        // Re-package against the fresh window analysis.
        if let Some(reg) = opts.registry.filter(|r| r.get("jev").is_some()) {
            let key = Cache::key(
                "jev_package",
                &[
                    &hash_str(&serde_json::to_string(&a).unwrap_or_default()),
                    &brief_hash,
                ],
            );
            decision = package::package(reg, "jev", brief, &keep_refs, &a, &cache.path(&key))?;
            cache.put(&key, &decision)?;
        } else {
            decision = package::fallback_package(&keep_refs);
        }
    }

    // Low-confidence route answers never auto-export.
    if !outcome.low_confidence.is_empty() && decision.package == Package::Export {
        decision = PackageOutcome {
            package: Package::NeedMorePeaks,
            note: "low-confidence route answers — needs a human".into(),
            ..decision
        };
    }
    write_json(opts.out, "package", &decision)?;

    // ---- emit ----------------------------------------------------------------------
    let s = Instant::now();
    let scene_path = opts.out.join("meme.scene");
    let mut spans = beat_spans(&keep_refs, opts.beat_sec, info.duration_s);
    let scene_src = if opts.materialize {
        materialize_beats(opts.input, &mut spans, &opts.out.join("beats"))?;
        String::new() // beats carry their own srcs
    } else {
        place_source(opts.input, opts.out)?
    };
    let (n, d) = info
        .video
        .as_ref()
        .and_then(|v| v.frame_rate)
        .map(|r| (u64::from(r.numerator), u64::from(r.denominator)))
        .unwrap_or((30, 1));
    let spec = EmitSpec {
        src: &scene_src,
        canvas: info
            .video
            .as_ref()
            .map(|v| (v.width, v.height))
            .unwrap_or((1080, 1920)),
        fps: Some((n, d)),
        has_audio: info.audio.is_some(),
        music_src: opts.music_src.as_deref(),
        render_target: "meme.mp4",
    };
    let markup = emit_flash_scene(&spans, &spec);
    write_atomic(&scene_path, markup.as_bytes())?;
    tick("emit", s, false, format!("{} beats", spans.len()));

    Ok(RunReport {
        video_sha256: video_sha,
        frames_seen: p.frames.len(),
        candidates: candidates.len(),
        keeps: outcome.keeps.clone(),
        low_confidence: outcome.low_confidence.clone(),
        decision,
        gemini: analysis,
        scene: scene_path,
        warnings,
        stages,
    })
}

impl RunOpts<'_> {
    /// Short label for logging/cache keys.
    fn gemini_mode_label(&self) -> &'static str {
        match self.gemini_mode {
            GeminiMode::Stills => "stills",
            GeminiMode::Windows => "windows",
        }
    }
}
