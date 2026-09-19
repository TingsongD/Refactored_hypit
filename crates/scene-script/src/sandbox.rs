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
function __run(frame, dataJson, w, h) {
  __ops.length = 0;
  ctx.w = w; ctx.h = h;
  // `with` parses once at setup — `d` is the same object every frame, so
  // state stashed on it in setup() reaches render() as documented.
  if (!__setup_done) {
    __data = JSON.parse(dataJson);
    if (typeof setup === 'function') { setup(__data); }
    __setup_done = true;
  }
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
        self.budget.set(INSTRUCTION_BUDGET);
        let data = if data_json.is_empty() {
            "{}"
        } else {
            data_json
        };
        // The data rides in as a quoted JS *string* — it's JSON.parse'd
        // inside __run, never evaluated as code.
        let json = self
            .ctx
            .with(|ctx| {
                ctx.eval::<String, _>(format!("__run({frame}, {}, {w}, {h})", js_str(data)))
            })
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

/// The real engine: each script loads once per worker, errors cache so
/// a broken program fails once instead of once per frame. Cache key is
/// `(src, with)` — two elements sharing a source but with different
/// payloads get independent program state, so `setup` sees the right
/// `d` in both.
#[derive(Default)]
pub struct SandboxPrograms {
    root: std::path::PathBuf,
    programs: std::collections::HashMap<(String, String), Slot>,
}

impl SandboxPrograms {
    pub fn new(root: std::path::PathBuf) -> Self {
        SandboxPrograms {
            root,
            programs: std::collections::HashMap::new(),
        }
    }

    /// Resolve `src` under `root`, refusing anything that escapes —
    /// `..` segments, absolute paths, symlinks pointing outside.
    /// Canonicalizing both sides makes the check lexical-proof.
    fn resolve(&self, src: &str) -> Option<std::path::PathBuf> {
        let root = self.root.canonicalize().ok()?;
        let path = self.root.join(src).canonicalize().ok()?;
        path.starts_with(&root).then_some(path)
    }
}

impl ProgramSource for SandboxPrograms {
    fn ops(&mut self, src: &str, local_frame: u32, with: &str, w: f64, h: f64) -> Option<DrawList> {
        let key = (src.to_string(), with.to_string());
        if !self.programs.contains_key(&key) {
            let slot = self
                .resolve(src)
                .and_then(|path| std::fs::read_to_string(&path).ok())
                .and_then(|s| Program::load(&s).ok())
                .map(|program| Slot::Loaded {
                    program,
                    warned_eval: false,
                    warned_drops: false,
                })
                .unwrap_or_else(|| {
                    eprintln!("warning: program `{src}` failed to load");
                    Slot::Failed
                });
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
                    eprintln!("warning: program `{src}` dropped {dropped} malformed op(s)");
                }
                Some(list)
            }
            Err(e) => {
                if !*warned_eval {
                    *warned_eval = true;
                    eprintln!("warning: program `{src}` render failed: {e}");
                }
                None
            }
        }
    }
}
