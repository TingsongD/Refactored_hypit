# User Guide

Scene Engine compiles `.scene` documents — XML-like markup with script
lines, tracks, and timed elements — into mp4 video. The unusual part:
elements never store seconds. They anchor to the *words* of the script,
and an alignment pass maps those anchors to real time once the voice
track exists. Edit the words, re-align, and the composition re-times
itself.

This guide walks from install to a finished video, then documents every
command, element, and attribute.

## Install

```bash
cargo build --release -p engine
./target/release/engine doctor
```

`doctor` probes the external tools the engine shells out to:

| tool | required | used for |
|---|---|---|
| `ffmpeg` | yes (render) | decode, encode, audio mix, mux |
| `ffprobe` | yes (render) | media metadata |
| `yt-dlp` | no | `adapt` from URLs only |

The binary itself is self-contained — no runtime deps beyond those
tools.

## Your first video

```bash
engine init myvideo
cd myvideo
```

`init` scaffolds:

```
myvideo/
  scene.toml     # project config (+ capability registry)
  main.scene     # the document you'll edit
  assets/        # media inputs live here
  out/           # render output lands here
  cache/         # scratch
  .gitignore     # ignores cache/ and out/
```

The template scene references `assets/narration.wav` and
`assets/backdrop.png` — drop in any wav + png (or trim the elements you
don't need yet). Then:

```bash
engine check main.scene
```

`check` parses, lowers, and validates — every error prints with a source
span, rustc-style. Fix anything it reports.

Now the timing step. The engine needs to know *when each word* of the
script is said. Two ways in:

```bash
# Option A: markers — "cue_id start_s end_s" per line
cat > markers.txt <<'EOF'
hook   0.00 1.85
payoff 1.85 4.10
EOF
engine align main.scene --markers markers.txt --out timings.json

# Option B: WhisperX alignment output
engine align main.scene --whisperx whisperx_out.json --out timings.json
```

Either produces `timings.json` — a `TimingMap` the renderer consumes.
`align` stamps the script's fingerprint into it, so a later render warns
if the script changed since alignment (your captions would show stale
words otherwise).

```bash
engine render main.scene --timings timings.json
```

Renders to `<render target>` — `out/final.mp4` in the template. Override
with `--out`.

## How timing actually works

Each `<line id="...">` in the script is a **cue**. Alignment produces a
word lattice: every word's start time plus the final word's end. Element
`during` attributes are anchor expressions that resolve against it:

| anchor | meaning |
|---|---|
| `hook` | the cue's whole span |
| `hook..payoff` | start of `hook` → end of `payoff` |
| `hook.end..payoff.start` | the gap *between* cues (`.start`/`.end` pick an edge) |
| `hook+2w` | hook's span shifted +2 **word** boundaries |
| `beat-6f` | shifted −6 **frames** |
| `hook+0.4s` | shifted +0.4 **seconds** |
| `1.5s..4s`, `0f..90f` | literal escape hatches (no cues needed) |

`+Nw` walks the word lattice *across* cue lines — `hook+2w` on a
one-word line lands inside the next line — and clamps at the stream
ends. It's how you say "start a beat before this word" without counting
frames.

Elements with no `during` fill the program span; `<board>` children
inherit the board's span. A scene where nothing resolves past t=0 is an
error.

## Document anatomy

```xml
<scene canvas="1080x1920" fps="30" clear="#0e0e12">
  <script track="voice" voice="narrator">   <!-- one per scene -->
    <line id="hook">Your opening line.</line>
    <line id="payoff">The payoff lands here.</line>
  </script>

  <track id="voice" kind="audio">           <!-- named: the script's track -->
    <sound src="assets/narration.wav" during="hook..payoff"/>
  </track>

  <track kind="visual" anchor="voice">      <!-- anchor: which timing source -->
    <image src="assets/backdrop.png" during="hook..payoff"/>
    <board during="payoff" at="center" anim="rise">
      <text bind="payoff.text"/>
    </board>
    <captions style="karaoke" anchor="voice.words"/>
    <program src="assets/fx.js" with='{"seconds": 6}'/>
  </track>

  <track kind="audio">
    <music src="assets/bed.mp3" gain="-14dB" duck="voice" during="hook..payoff"/>
  </track>

  <render target="out/final.mp4"/>          <!-- one per scene -->
</scene>
```

Top-level: exactly one `<script>`, any number of `<track>`s, at most one
`<render>`.

`<script track="…">` names the audio track that carries the narration —
that pairing is what `anchor="voice"` and `anchor="voice.words"` resolve
against. `voice` is an optional label. `<line>` takes `id` + text only.

### Elements

| element | purpose | extra attributes |
|---|---|---|
| `clip` | video footage, canvas-cover | `src` |
| `image` | still image, canvas-cover | `src` |
| `text` | text box — literal body **or** `bind` | `bind="line.text"` |
| `board` | styled group; children stack vertically inside | — |
| `captions` | word-timed subtitles | `style` (`karaoke` default, `block`), `anchor="track.words"` |
| `music` | audio bed | `src`, `gain`, `duck` |
| `sound` | audio clip | `src`, `gain` |
| `program` | sandboxed JS draw script | `src`, `with` (JSON object) |

Attributes every visual element accepts:

- `during` — anchor expression (table above). Omit → fills the program.
- `at` — placement: `center`, `top`, `bottom`, `left`, `right`, or
  `x,y` in canvas pixels. Boards stack their children inside the box.
- `anim` — entrance animation: `rise`, `fade`, or `pop`.
- `id` — a name, mostly useful for readability.

Audio specifics:

- `gain="-14dB"` or `gain="-14"` — must be a finite number.
- `duck="voice"` — sidechain-duck this element under the named track.
  `duck` works on `<music>` only; on `<sound>` it's ignored (with a
  warning).
- `<captions>` with no `anchor` defaults to the script's track at word
  granularity; it's an error only when the scene has no `<script>`.

### Path safety

All `src` attributes and `<render target>` resolve against the **project
root** (the scene file's directory) and may not escape it — `..`
segments and absolute paths outside the root are refused, including
through symlinks. `check` warns; `render` refuses outright. The one
exception is `--out`: that's your own command-line argument and goes
where you say.

## Programs

`<program src="assets/fx.js" with='{...}'/>` runs a JavaScript file in a
QuickJS sandbox — no filesystem, no process access, CPU- and
memory-capped. Two hooks:

```js
function setup(d) { d.phase = 0; }       // once; `d` starts as the `with` object
function render(ctx, f, d) {             // per frame; f = local frame index
  ctx.setFill('#ff4466');
  ctx.setFont(28);
  ctx.rect(10, ctx.h - 10 - (f % 30) * 4, 40, (f % 30) * 4);
  ctx.text('frame ' + f, 10, 30);
}
```

`ctx.w`/`ctx.h` is the element's box (full canvas unless placed).
Drawing ops: `rect(x,y,w,h)`, `circle(x,y,r)`, `text(str,x,y)` — all in
the current fill.

Guarantees worth knowing:

- State accumulated on `d` is **deterministic at any worker count** — a
  worker starting mid-range replays the prefix's `render` calls first.
- `d` is per *element instance* — two `<program>` elements with the
  same `src` and `with` get independent runtimes; a counter stashed on
  `d` can't leak between them.
- Under `--frames a:b` the program still replays from scene start, so
  `f`/`d` carry the same values a full render would — a window only
  changes which frames are emitted.
- Malformed ops are dropped, not fatal; a crashed program fails once
  (cached), then every frame, and the failure lands in the render's
  diagnostics bundle.
- Program `src` follows the same project-root confinement as `src`.

## Rendering

```bash
engine render main.scene --timings timings.json
engine render main.scene --timings t.json --out draft.mp4
engine render main.scene --frames 60:120     # a window — audio rebases to it
engine render main.scene --frames 90         # exactly one frame
engine render main.scene --workers 8         # default 4
```

- `--timings` — the `TimingMap` JSON from `align`. Without it, literal
  anchors (`1.5s..4s`) still resolve; cue anchors error.
- `--frames a:b` renders only that window. The audio mix is clipped to
  the same window, so partial renders sound right, and stateful
  `<program>` scripts replay from scene start — window pixels match a
  full render exactly.
- Output is byte-identical for any `--workers` value — worker count
  changes speed, never pixels.
- Memory is bounded: roughly `workers × 5 + 1` frames (~340 MB at 8
  workers on 1080×1920), independent of video length.
- A failed render never touches an existing output — frames encode to a
  temporary sibling file and only replace the target once the encode
  succeeds.

## Adapt — draft a scene from footage

```bash
engine adapt clip.mp4 --out draft.scene
engine adapt https://… --out-dir assets --out draft.scene   # URLs via yt-dlp
```

Probes the media, detects hard cuts, and emits a starting-point scene:
full-span clip + one labeled board per shot + a music track when the
source has audio. The draft always parses clean — edit it, don't ship
it. When `--out` lands the draft somewhere other than the footage's
directory, the footage is imported into the draft's `assets/` first
(a yt-dlp download moves outright) — the emitted `src` never escapes
the scene's project root.

## Capabilities — generated assets

`scene.toml` can register connectors for external services (TTS, image
APIs):

```toml
[capabilities.tts]
command = ["sh", "connectors/elevenlabs-tts.sh"]
auth = { env = "ELEVENLABS_API_KEY" }
```

```bash
engine cap list
engine cap call tts --params '{"text": "Hello.", "voice": "…"}' --out assets/narration.wav
```

The connector receives a JSON request on stdin and writes the asset.
Credentials resolve from env or the OS keychain (`auth = { keychain =
{ service = "…", account = "…" } }`) and arrive to the connector as
`SCENE_CAP_AUTH` — never in argv, never in the scene file. Reference
connector scripts live in `connectors/`.

## The UI

```bash
engine ui --dir myvideo          # http://localhost:8484
engine ui --dir myvideo --port 9000 --no-open
```

A single-page editor: edit `main.scene`, `check` with inline
diagnostics, `render`, watch the result with seeking. It's a local test
harness — renders run serially and there's no cancel button, so a long
render blocks the UI until it finishes. It serves only `out/`'s real
contents; nothing else on disk is reachable.

## Diagnostics and warnings

Every stage reports through `Diagnostic { severity, message, span }` —
`check` prints them against the source like a compiler. Render-time
problems (missing `src`, a corrupt clip mid-file, a refused program
path, stale timing fingerprints, invalid word times) collect into a
warnings bundle that prints with the render result and shows in the UI.

Treat warnings as failures-in-waiting: "clip not found" means a hole in
the video, "program escapes the project root" means your overlay is a
black frame.

## Limits and behavior notes

- Documents nest at most 128 levels deep — beyond that `check`/`render`
  report a diagnostic, not a crash.
- All external processes run under deadlines: probes 30 s, the audio mix
  10 min, connectors 10 min (curl self-aborts at 5), downloads 15 min,
  and stalled decoder/encoder pipes are killed after a 5/2-minute I/O
  stall. A wedged tool is an error, never a hang.
- A failed render leaves no partial mp4 — the `-y`-truncated file is
  removed.
- Same-source overlapping `<clip>`s decode correctly but reopen the file
  on backward seeks — prefer one clip per source per moment.

## Where to look next

- `SKILL.md` — the authoring reference agents read
- `docs/playbooks/` — worked scenes (captions over footage, program
  overlays, adapt workflow)
- `DESIGN.md` — architecture, module gates, hardening invariants
- `DEV_LOG.md` — change history
