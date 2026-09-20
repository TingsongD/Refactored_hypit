//! The QuickJS sandbox. One `Program` = one runtime+context, used
//! per-render-worker (QuickJS contexts are cheap but not thread-safe —
//! the pool gives each worker its own).
//!
//! Hardening is structural, not policy:
//! - the crate builds `rquickjs` with `default-features = false`, so the
//!   `std`/`os` modules that reach the filesystem *do not exist* — a
//!   script can't ask for what was never registered
//! - memory cap + instruction budget come from the runtime, not the
//!   honor system
//! - everything crossing the boundary is JSON (params in, ops out)

use std::cell::Cell;
use std::rc::Rc;

use rquickjs::{Context, Runtime};

use crate::ops::DrawList;

/// Hard caps — a program is a component, not a tenant.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024; // 64 MiB
const INSTRUCTION_BUDGET: u64 = 20_000; // ~0.5s of runaway before abort

#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("could not start script runtime: {0}")]
    Runtime(String),
    #[error("script failed to load: {0}")]
    Load(String),
    #[error("render call failed: {0}")]
    Eval(String),
}

/// The JS-side prelude: a tiny canvas-shaped `ctx` that buffers ops and
/// the harness that runs setup once, render every call.
const PRELUDE: &str = r#"
var __ops = [];
var __setup_done = false;
var __data = null;
var ctx = {
  w: 0, h: 0,
  fill: '#ffffffff',
  size: 32,
  setFill(c) { this.fill = c; },
  setFont(s) { this.size = s; },
  rect(x, y, w, h) { __ops.push({op:'rect', x, y, w, h, c: this.fill}); },
  circle(x, y, r) { __ops.push({op:'circle', x, y, r, c: this.fill}); },
  text(t, x, y) { __ops.push({op:'text', t, x, y, size: this.size, c: this.fill}); },
};
// Setup and render are separate evals on the Rust side — each gets its
// own instruction budget, so heavy init can't borrow the first frame's.
function __setup(dataJson) {
  // `with` parses once at setup — `d` is the same object every frame, so
  // state stashed on it in setup() reaches render() as documented.
  __data = JSON.parse(dataJson);
  if (typeof setup === 'function') { setup(__data); }
  __setup_done = true;
}
function __render(frame, w, h) {
  __ops.length = 0;
  ctx.w = w; ctx.h = h;
  if (typeof render !== 'function') { throw new Error('program must define render(ctx, frame, data)'); }
  render(ctx, frame, __data);
  return JSON.stringify(__ops);
}
"#;

/// A loaded program. `render` evaluates `render(ctx, frame, data)` and
/// returns the ops it drew.
pub struct Program {
    _runtime: Runtime,
    ctx: Context,
    budget: Rc<Cell<u64>>,
}

impl Program {
    /// Compile `source` in a fresh sandbox. `render` must exist by call
    /// time — `setup` is optional.
    pub fn load(source: &str) -> Result<Self, ScriptError> {
        let runtime = Runtime::new().map_err(|e| ScriptError::Runtime(e.to_string()))?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        let budget = Rc::new(Cell::new(INSTRUCTION_BUDGET));
        let b = Rc::clone(&budget);
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let left = b.get().saturating_sub(1);
            b.set(left);
            left == 0 // true = abort this evaluation
        })));
        let ctx = Context::full(&runtime).map_err(|e| ScriptError::Runtime(e.to_string()))?;
        ctx.with(|ctx| {
            ctx.eval::<(), _>(PRELUDE).map_err(|e| e.to_string())?;
            ctx.eval::<(), _>(source).map_err(|e| e.to_string())
        })
        .map_err(ScriptError::Load)?;
        Ok(Program {
            _runtime: runtime,
            ctx,
            budget,
        })
    }

    /// Evaluate the program for one local frame. `data_json` is the
    /// element's `with` payload (already JSON); `w`,`h` are the
    /// element's box so scripts can lay out relative to it. Returns the
    /// ops plus how many were dropped as malformed.
    pub fn render(
        &mut self,
        frame: u32,
        data_json: &str,
        w: f64,
        h: f64,
    ) -> Result<(DrawList, usize), ScriptError> {
        let data = if data_json.is_empty() {
            "{}"
        } else {
            data_json
        };
        // Every eval gets its own budget: the status probe is trivial,
        // setup's init work gets a full allotment, and each frame's
        // render gets a fresh one too.
        self.budget.set(INSTRUCTION_BUDGET);
        let needs_setup = self
            .ctx
            .with(|ctx| ctx.eval::<bool, _>("!__setup_done"))
            .map_err(|e| ScriptError::Eval(e.to_string()))?;
        if needs_setup {
            self.budget.set(INSTRUCTION_BUDGET);
            self.ctx
                .with(|ctx| ctx.eval::<(), _>(format!("__setup({})", js_str(data))))
                .map_err(|e| ScriptError::Eval(e.to_string()))?;
        }
        self.budget.set(INSTRUCTION_BUDGET);
        // The data rides in as a quoted JS *string* — it's JSON.parse'd
        // inside __setup, never evaluated as code.
        let json = self
            .ctx
            .with(|ctx| ctx.eval::<String, _>(format!("__render({frame}, {w}, {h})")))
            .map_err(|e| ScriptError::Eval(e.to_string()))?;
        Ok(DrawList::from_json(&json))
    }
}

/// Quote a string for embedding inside an eval snippet — the request
/// JSON rides into the sandbox as a JS string literal, never as code.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// A `ProgramSource` that draws nothing — for scenes without programs,
/// and for tests that shouldn't pay for a JS runtime.
pub struct NullPrograms;

impl ProgramSource for NullPrograms {
    fn ops(
        &mut self,
        _src: &str,
        _local_frame: u32,
        _with: &str,
        _w: f64,
        _h: f64,
    ) -> Option<DrawList> {
        None
    }
}
pub trait ProgramSource {
    /// Ops for `local_frame` inside a `w`×`h` box, or `None` when the
    /// program can't run — the rasterizer draws its placeholder.
    fn ops(&mut self, src: &str, local_frame: u32, with: &str, w: f64, h: f64) -> Option<DrawList>;
}

/// Per-script state: load failures cache so a broken program fails once
/// instead of once per frame; eval/drop warnings likewise fire once.
enum Slot {
    Failed,
    Loaded {
        program: Program,
        warned_eval: bool,
        warned_drops: bool,
    },
}

/// Shared per-render warning log — the same concrete type as
/// `scene_render::WarnSink`; scene-script can't name that alias without
/// a dependency cycle (scene-render depends on this crate), so it takes
/// `Arc<Mutex<BTreeSet<String>>>` directly. `None` keeps the standalone
/// `eprintln!` behavior for callers with no sink to feed.
pub type WarnSink = std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>;

fn warn(sink: &Option<WarnSink>, msg: String) {
    match sink {
        Some(s) => {
            if let Ok(mut w) = s.lock() {
                w.insert(msg);
            }
        }
        None => eprintln!("warning: {msg}"),
    }
}

/// The real engine: each script loads once per worker, errors cache so
/// a broken program fails once instead of once per frame. Cache key is
/// `(src, with)` — two elements sharing a source but with different
/// payloads get independent program state, so `setup` sees the right
/// `d` in both.
#[derive(Default)]
pub struct SandboxPrograms {
    root: std::path::PathBuf,
    programs: std::collections::HashMap<(String, String), Slot>,
    warnings: Option<WarnSink>,
}

/// Why a `src` couldn't be used — "escapes the root" and "isn't there"
/// deserve different warnings.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// `..` segments, an absolute path, or a symlink pointing outside.
    Escapes,
    /// Doesn't exist (or the root itself can't be resolved).
    Missing,
}

/// Canonicalize `with` for the cache key: formatting and key-order
/// variants of the same payload share one program — serde_json maps
/// sort keys, so `{"a":1}` vs `{"a": 1}` and `{"a":1,"b":2}` vs
/// `{"b":2,"a":1}` all collapse. Invalid/non-object JSON falls back to
/// the raw string — the markup layer already rejects those anyway.
fn canonical_with(with: &str) -> String {
    serde_json::from_str::<serde_json::Value>(with)
        .ok()
        .filter(|v| v.is_object())
        .map(|v| v.to_string())
        .unwrap_or_else(|| with.to_string())
}

impl SandboxPrograms {
    pub fn new(root: std::path::PathBuf) -> Self {
        SandboxPrograms {
            root,
            programs: std::collections::HashMap::new(),
            warnings: None,
        }
    }

    /// Record program failures into `sink` so a missing/refused/broken
    /// script reaches the diagnostic report — same contract the media
    /// sources use (`with_warnings` on the frame sources).
    pub fn with_warnings(mut self, sink: WarnSink) -> Self {
        self.warnings = Some(sink);
        self
    }

    /// Resolve `src` under `root`, refusing anything that escapes —
    /// `..` segments, absolute paths, symlinks pointing outside.
    /// Canonicalizing both sides makes the check lexical-proof.
    pub(crate) fn resolve(&self, src: &str) -> Result<std::path::PathBuf, ResolveError> {
        let root = self
            .root
            .canonicalize()
            .map_err(|_| ResolveError::Missing)?;
        let path = self
            .root
            .join(src)
            .canonicalize()
            .map_err(|_| ResolveError::Missing)?;
        if path.starts_with(&root) {
            Ok(path)
        } else {
            Err(ResolveError::Escapes)
        }
    }
}

impl ProgramSource for SandboxPrograms {
    fn ops(&mut self, src: &str, local_frame: u32, with: &str, w: f64, h: f64) -> Option<DrawList> {
        let warnings = self.warnings.clone();
        let key = (src.to_string(), canonical_with(with));
        if !self.programs.contains_key(&key) {
            let slot = match self.resolve(src) {
                Err(ResolveError::Escapes) => {
                    warn(
                        &warnings,
                        format!("program `{src}` escapes the project root — refused"),
                    );
                    Slot::Failed
                }
                Err(ResolveError::Missing) => {
                    warn(&warnings, format!("program `{src}` not found"));
                    Slot::Failed
                }
                Ok(path) => std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| Program::load(&s).ok())
                    .map(|program| Slot::Loaded {
                        program,
                        warned_eval: false,
                        warned_drops: false,
                    })
                    .unwrap_or_else(|| {
                        warn(&warnings, format!("program `{src}` failed to load"));
                        Slot::Failed
                    }),
            };
            self.programs.insert(key.clone(), slot);
        }
        let slot = self.programs.get_mut(&key)?;
        let Slot::Loaded {
            program,
            warned_eval,
            warned_drops,
        } = slot
        else {
            return None;
        };
        match program.render(local_frame, with, w, h) {
            Ok((list, dropped)) => {
                if dropped > 0 && !*warned_drops {
                    *warned_drops = true;
                    warn(
                        &warnings,
                        format!("program `{src}` dropped {dropped} malformed op(s)"),
                    );
                }
                Some(list)
            }
            Err(e) => {
                if !*warned_eval {
                    *warned_eval = true;
                    warn(&warnings, format!("program `{src}` render failed: {e}"));
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DrawOp;

    #[test]
    fn setup_runs_under_its_own_budget() {
        // Setup burns ~200 ticks (measured: ~1 tick per 5k iterations).
        // If setup shared the render eval's budget, `budget` would show
        // ≤ 20000-200 afterward; a fresh per-eval budget leaves ~19995+.
        let mut p = Program::load(
            "function setup(d){ for(let i=0;i<1_000_000;i++){} d.flag=7; }\n\
             function render(ctx,f,d){ ctx.rect(d.flag,0,1,1); }",
        )
        .unwrap();
        let (ops, _) = p.render(0, "{}", 1.0, 1.0).unwrap();
        assert_eq!(ops.0.len(), 1, "setup ran and render produced ops");
        assert!(
            p.budget.get() > INSTRUCTION_BUDGET - 100,
            "render eval ran under a fresh budget — setup's ticks not counted: left={}",
            p.budget.get()
        );
    }

    #[test]
    fn setup_failure_surfaces_as_eval_error() {
        let mut p =
            Program::load("function setup(d){ nope(); } function render(ctx,f,d){}").unwrap();
        assert!(p.render(0, "{}", 1.0, 1.0).is_err());
    }

    #[test]
    fn resolve_tells_escapes_from_missing() {
        let id = std::process::id();
        let root = std::env::temp_dir().join(format!("resolve-root-{id}"));
        let outside = std::env::temp_dir().join(format!("resolve-out-{id}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("ok.js"), "function render(){}").unwrap();
        std::fs::write(outside.join("x.js"), "function render(){}").unwrap();

        let p = SandboxPrograms::new(root.clone());
        assert!(p.resolve("ok.js").is_ok());
        // A real file reached through `..` is an escape…
        let rel = format!("../resolve-out-{id}/x.js");
        assert_eq!(p.resolve(&rel), Err(ResolveError::Escapes));
        // …and so is an absolute path outside the root.
        assert_eq!(
            p.resolve(outside.join("x.js").to_str().unwrap()),
            Err(ResolveError::Escapes)
        );
        // …while something that simply isn't there is missing.
        assert_eq!(p.resolve("missing.js"), Err(ResolveError::Missing));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn canonical_with_folds_formatting() {
        assert_eq!(canonical_with(r#"{"a":1}"#), canonical_with(r#"{"a": 1}"#));
        // serde_json maps sort keys — key order folds too.
        assert_eq!(
            canonical_with(r#"{"a":1,"b":2}"#),
            canonical_with(r#"{"b":2,"a":1}"#)
        );
        assert_ne!(canonical_with(r#"{"a":1}"#), canonical_with(r#"{"a":2}"#));
        assert_eq!(canonical_with("not json"), "not json");
    }

    #[test]
    fn whitespace_variant_with_shares_one_program() {
        // `{"step":1}` and `{"step": 1}` are the same payload — they must
        // share one program, so `d` state persists across the variant.
        let root = std::env::temp_dir().join(format!("prog-ws-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("p.js"),
            "function setup(d){ d.x = d.step; }\n\
             function render(ctx,f,d){ d.x += d.step; ctx.rect(d.x,0,1,1); }",
        )
        .unwrap();
        let mut progs = SandboxPrograms::new(root.clone());
        let x = |list: DrawList| match &list.0[0] {
            DrawOp::Rect { x, .. } => *x,
            _ => panic!("rect"),
        };
        let a = progs.ops("p.js", 0, r#"{"step":1}"#, 1.0, 1.0).unwrap();
        assert_eq!(x(a), 2.0); // setup 1 + render 1
        // Whitespace variant hits the same slot — `d.x` continues at 3,
        // not reset to 2 by a fresh program.
        let b = progs.ops("p.js", 1, r#"{"step": 1}"#, 1.0, 1.0).unwrap();
        assert_eq!(x(b), 3.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn program_failures_reach_the_sink_once() {
        let id = std::process::id();
        // Unique dir names — other tests in this process already own
        // `prog-out-{id}` and share the same temp namespace.
        let root = std::env::temp_dir().join(format!("prog-sink-{id}"));
        let outside = std::env::temp_dir().join(format!("prog-sink-out-{id}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.js"), "function render(){}").unwrap();
        let sink = WarnSink::default();
        let mut progs = SandboxPrograms::new(root.clone()).with_warnings(sink.clone());

        assert!(progs.ops("gone.js", 0, "{}", 10.0, 10.0).is_none());
        // Cached failure — the warning must not repeat per frame.
        assert!(progs.ops("gone.js", 1, "{}", 10.0, 10.0).is_none());
        {
            let w = sink.lock().unwrap();
            assert_eq!(w.len(), 1, "{w:?}");
            assert!(w.iter().next().unwrap().contains("gone.js"));
        }

        // A real file reached through `..` reports the escape wording.
        let rel = format!("../prog-sink-out-{id}/x.js");
        assert!(progs.ops(&rel, 0, "{}", 10.0, 10.0).is_none());
        let w = sink.lock().unwrap();
        assert!(
            w.iter().any(|m| m.contains("escapes the project root")),
            "{w:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
