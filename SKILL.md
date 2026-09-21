---
name: scene-engine
description: Author word-timed videos as .scene markup and render them with the `engine` CLI. Use when a task involves composing video from a script, timing visuals to speech, generating caption-styled edits, adapting downloaded clips, or drawing animated overlays from a small sandboxed program.
---

# Scene Engine

Videos are **documents**, not timelines. You write a `.scene` file —
script lines, tracks, anchored elements — and `engine render` compiles
it to mp4. Timing is *symbolic*: elements anchor to script cues
(`during="hook..payoff"`, `during="beat+2w"`), so editing the words
re-times the whole composition automatically.

## Workflow

```bash
engine init myvideo          # scaffold scene.toml + main.scene + assets/
# …write main.scene, drop media into assets/…
engine check main.scene      # validate: every error has a source span
engine align main.scene --markers markers.txt --out timings.json
engine render main.scene --timings timings.json --out out/final.mp4
engine render main.scene --frames 60:120    # a window; audio rebases to it
engine render main.scene --frames 90        # exactly one frame
```

External tools: `ffmpeg` + `ffprobe` are required for render; `yt-dlp`
only for `adapt` from URLs. Run `engine doctor` to verify.

## Document shape

```xml
<scene canvas="1080x1920" fps="30" clear="#0e0e12">
  <script track="voice">
    <line id="hook">The part nobody mentions.</line>
    <line id="payoff">It compounds either way.</line>
  </script>
  <track id="voice" kind="audio"/>
  <track kind="visual" anchor="voice">
    <clip src="assets/bg.mp4" during="hook..payoff"/>
    <captions anchor="voice.words" during="hook..payoff"/>
    <board during="payoff" at="bottom" anim="rise">
      <text bind="payoff.text"/>
    </board>
  </track>
  <track kind="audio">
    <music src="assets/bed.mp3" gain="-14dB" duck="voice" during="hook..payoff"/>
  </track>
  <render target="out/final.mp4"/>
</scene>
```

## Timing grammar (the core idea)

Anchors resolve against the script's word alignment (`--timings`):

| anchor | meaning |
|---|---|
| `hook..payoff` | start of `hook` → end of `payoff` |
| `payoff` | just that cue's span |
| `hook+2w` | hook's span shifted 2 **word** boundaries |
| `beat-6f` | shifted −6 frames |
| `1.5s..4s`, `0f..90f` | literal escape hatches |

`+Nw` walks word-start boundaries across cue lines and clamps at the
stream ends — use it for "start a beat before this word" phrasing.
Elements without `during` fill the program; board children inherit the
board's span.

## Elements

- `clip src` — video footage, canvas-cover (`at` = crop focal);
  `from="12.37s"` starts sampling the source at that offset
- `image src` — still, canvas-cover
- `text` — literal body text or `bind="line.field"` into the script
- `board` — styled group; children stack vertically inside it
- `captions anchor="track.words"` — word-highlighted subtitles
- `music` / `sound` — 48 kHz audio; `gain="-6dB"` (finite), `duck="voice"`,
  `from="Ns"` source offset — `duck` works on `music` only; on `sound`
  it's ignored with a warning
- `program src with` — sandboxed JS draw program (below)

Common attributes: `during`, `at` (`center|top|bottom|left|right` or
`x,y`), `anim` (`rise`, `pop`, `fade`), `id`.

All `src` attributes and `<render target>` resolve against the project
root and must stay inside it — `..` and absolute paths are refused
(`check` warns, `render` errors). `--out` is the only way to write
outside the project dir.

## Programs (sandboxed draw scripts)

`<program src="assets/fx.js"/>` runs a QuickJS script per frame — no
fs, no process, CPU/memory-capped. Ops land in a `ctx` canvas:

```js
function setup(d) { d.phase = 0; }            // once; `d` is the `with` object —
                                            // fields you set here persist into render
function render(ctx, f, d) {                  // f = local frame
  ctx.setFill('#ff4466');                     // persistent state
  ctx.setFont(28);
  ctx.rect(10, ctx.h - 10 - (f % 30) * 4, 40, (f % 30) * 4);
  ctx.text('frame ' + f, 10, 30);
}
```

`ctx.w`/`ctx.h` are the element's box (the canvas by default). Ops:
`rect(x,y,w,h)`, `circle(x,y,r)`, `text(str,x,y)` — all in the current
fill color. Malformed ops are dropped, never fatal.

`render` is called once per frame in order — state accumulated on `d`
is deterministic at any worker count (shard prefixes replay before a
worker starts mid-range). `d` is per *element instance*: two programs
with the same `src` and `with` get independent runtimes. Under
`--frames a:b` the program still replays from scene start, so `f`/`d`
carry the same values a full render would — the window only changes
which frames are emitted.

## Adapt (existing footage → draft scene)

```bash
engine adapt clip.mp4 --out main.scene        # or a URL via yt-dlp
```

Probes the file, detects hard cuts (robust frame-diff), and emits a
draft: full-span clip + one labeled board per shot + music track when
the source has audio. It always parses clean — it's a starting point
you edit, not a finished edit. With `--out`, footage outside the draft's
project root is copied into its `assets/` first, so the emitted `src`
always resolves inside the scene (a downloaded URL moves, not copies).

## Meme (flash-cut analysis → draft scene)

```bash
engine meme clip.mp4 --out meme-out [--brief b.toml] [--timings t.json]
```

Local perception only — nothing uploads: per-frame visual metrics +
audio onsets fuse into beat candidates, snap to word boundaries when
`--timings` is present, and emit `meme.scene` where each keep is a
`<clip from="t">` + `<sound from="t">` pair (audio cuts with picture).
Optional `scene.toml` capabilities add a Jev text route (fact sheets —
never pixels/vectors) and a Gemini pass over kept stills/short windows;
absent → fully offline dHash mode. Decisions are typed:
`export | need_more_peaks | rerun_window` (the last re-analyzes one keep
only). `--materialize` writes physical `out/beats/<content-key>/*.mp4` instead of
`from` offsets. Stage caches under `out/.cache/v2/` key on content hashes —
a rerun replays only what changed. Configure `gemini_model` (or `GEMINI_MODEL`)
when enabling Gemini; explicit brief settings win. Provider adapters are reference
implementations tested offline. See `connectors/README.md`. Spec: `docs/flash-cut-pipeline.md`.

## Capabilities (external asset generation)

`scene.toml` can register connector capabilities — TTS, image APIs —
invoked as `engine cap call <name> --params '{...}' --out assets/x`.
Credentials come from env or the OS keychain, never the scene file.
See `connectors/` for reference scripts.

## Diagnostics

`check` reports like rustc — every error carries a byte span rendered
against the source. Unknown elements/attributes, unresolved cues,
missing `src`, bad JSON in `with`, missing asset files all surface
before render. Render-time problems (missing/corrupt media, refused or
broken programs, stale timing fingerprints, invalid word times) collect
into a warnings bundle — treat warnings as real feedback; they usually
mean the scene won't look like you expect.
