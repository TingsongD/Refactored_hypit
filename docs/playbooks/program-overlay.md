# Playbook: animated overlay with a `<program>` element

When a visual is easier to describe as code than as markup — a progress
bar, a counter, a waveform-ish flourish — a program element draws it
inside a QuickJS sandbox. The script emits *draw ops*, the engine
rasters them. No fs, no network, capped CPU and memory.

## 1. Write the script

`assets/progress.js`:

```js
function setup(d) {
  d.total = (d.seconds || 6) * 30;   // `with` payload becomes `d` — stash derived state here
}

function render(ctx, f, d) {
  var p = Math.min(f / d.total, 1);          // 0..1 across the program
  // track
  ctx.setFill('#333a44');
  ctx.rect(20, ctx.h - 34, ctx.w - 40, 8);
  // fill
  ctx.setFill('#ff4466');
  ctx.rect(20, ctx.h - 34, (ctx.w - 40) * p, 8);
  // label
  ctx.setFill('#ffffff');
  ctx.setFont(24);
  ctx.text(Math.round(p * 100) + '%', ctx.w - 70, ctx.h - 44);
}
```

`setup(d)` runs once per element instance — stash derived state on `d`.
`render(ctx, f, d)` runs per frame; `f` is the element-local frame
(first live frame = 0). `d` is the `with` JSON object itself —
`with='{"seconds": 6}'` arrives as `d.seconds`, and anything you add
in `setup` is still there in `render`. Two `<program>` elements —
even with the same `src` and `with` — get their own `d`, so state
never leaks between them.

Ops are element-local: `(0,0)` is the element's top-left, and
`ctx.w`/`ctx.h` report its box. A program with no `at` covers the
canvas.

## 2. Reference it in the scene

```xml
<track kind="visual">
  <clip src="assets/bg.mp4" during="0s..6s"/>
  <program src="assets/progress.js" with='{"seconds": 6}' during="0s..6s"/>
</track>
```

`with` must be a JSON object literal — `check` rejects anything else.

## 3. Render and verify

```bash
engine check main.scene
engine render main.scene
```

## What to check

- **Determinism**: the same `f` must emit the same ops. Anything
  time-seeded or random makes frames run-dependent.
- **Bad ops degrade, not crash**: a malformed op is dropped; the rest
  still draw. A script that fails to load draws the broken-media
  placeholder, not a panic.
- **Limits**: `setup` and each `render` call get their own instruction
  budget (~20k ticks). If your render needs more, it's doing too much
  per frame — precompute in `setup`.
- **State**: `d` is the same object across `setup`/`render`, and
  `render` calls replay in frame order even when workers shard the
  range — counters on `d` are deterministic at any worker count. A
  `--frames a:b` window replays the program from scene start too, so
  `f` keeps its element-local numbering and `d` carries the same state
  a full render would — the window only changes which frames land in
  the video. Prefer deriving frame-varying values from `f` directly —
  clearer, and unaffected by either sharding or windows.
