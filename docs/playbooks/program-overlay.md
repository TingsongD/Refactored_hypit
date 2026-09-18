# Playbook: animated overlay with a `<program>` element

When a visual is easier to describe as code than as markup — a progress
bar, a counter, a waveform-ish flourish — a program element draws it
inside a QuickJS sandbox. The script emits *draw ops*, the engine
rasters them. No fs, no network, capped CPU and memory.

## 1. Write the script

`assets/progress.js`:

```js
function setup(d) {
  d.total = (d.with && d.with.seconds || 6) * 30; // frames
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

`setup(d)` runs once per render worker — stash constants on `d`.
`render(ctx, f, d)` runs per frame; `f` is the element-local frame
(first live frame = 0). `ctx.with`… no — the `with` JSON rides on `d`:
`d.with` is whatever the markup's `with='{...}'` attribute carried.

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
  time-seeded or random makes frames worker-count-dependent.
- **Bad ops degrade, not crash**: a malformed op is dropped; the rest
  still draw. A script that fails to load draws the broken-media
  placeholder, not a panic.
- **Limits**: runaway loops and memory hogs are aborted (~20k
  instruction ticks). If your render needs more, it's doing too much
  per frame — precompute in `setup`.
- **State**: `d` persists between frames on a worker; workers each get
  their own sandbox. Never depend on `d` for correctness across
  *frames* — derive everything from `f`.
