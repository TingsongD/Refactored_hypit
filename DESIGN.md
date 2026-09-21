# Design

## Provenance

Clean-room implementation of an agent-authored video engine. The *idea* —
markup-anchored-to-words video authoring for AI agents, rendered natively —
is implemented here with original code, original vocabulary, and an original
document format. No code, tests, documentation text, or assets were taken
from any existing project.

## Architecture

```
.scene ──parse──▶ RawNode ──lower──▶ Scene IR (@1, immutable wire types)
                                        │
                      realize (M3): anchors → aligned audio → frame domain
                                        │
                              frame program ──▶ render workers (Skia, M1)
                                        │              │ frames, bounded + ordered
                       audio graph (M2) ──▶ 48kHz mix ──▶ ffmpeg mux
                                        ▼
                                    out.mp4
```

Deliberate choices:

- **Rust, one binary.** No runtime transpilation, no package manager at the
  point of use. `cargo-dist`-style install is the target.
- **Data, not code, for scenes.** Agents emit markup; the engine owns the
  schema. Scripted components arrive later as sandboxed programs (M4),
  never as code linked into the engine.
- **Two-phase time.** Authored anchors → measured audio → frames. This is
  the product's core behavior; everything else is ordinary video plumbing.
- **ffmpeg for media, nothing else.** Decode, encode, probe are subprocess
  pipes; the stream is tagged BT.709 end to end (`-colorspace/-primaries/
  -trc bt709`) so the container agrees with the converter.
- **Diagnostics are data.** Stages emit `Diagnostic { severity, message,
  span }`; the CLI renders. Agents get machine-checkable output for free.

## Modules — small, gated, sequential

Each module ships with its own unit tests plus a validation gate. A module
is **done** only when its gate passes (`cargo test -p <crate>` green plus
the stated validation). Modules are ordered by dependency; no module starts
before the previous gate is green.

| # | Module | Contents | Validation gate |
|---|---|---|---|
| M0 | scaffold | workspace, CI, `scene-ir`, `scene-markup`, `engine` CLI | ✅ done — 37 tests green, `check`/`parse`/`init`/`doctor` work |
| M1 | `scene-time` | TimingSource (aligned words), anchor → frame/sample resolution, ResolvedScene, program span, untimed-element inheritance | ✅ done — 21 tests green (58 total) |
| M2 | `scene-media` | ffprobe JSON → MediaInfo; ffmpeg rawvideo decode → frame iterator | ✅ done — 7 unit + 3 env-gated integration tests green (verified with real ffmpeg 8.0: 10 frames decoded from 1s@10fps lavfi clip) |
| M3 | `scene-layout` | element tree + resolved timing → positioned boxes per frame; `at` placement; board child stacking | ✅ done — 19 tests green; `Measure` seam takes `(element, local_s)` |
| M4 | `scene-render` | tiny-skia raster (clip/image/board/text/captions), entrance anims, worker pool, RGBA→NV12 (BT.709) | ✅ done — 12 tests green; byte-identical across 1/4/7 workers *including stateful programs* (shard-prefix ops replay); real cosmic-text ink verified |
| M5 | mux | NV12 pipe → ffmpeg → mp4 (silent); `engine render` end-to-end | ✅ done — 4 unit + 5 gated tests; 30f roundtrip probed 64x36@30/1; `engine render` smoke: 90f → h264/yuv420p/3.0s verified |
| M6 | `scene-audio` | 48kHz clip graph → ffmpeg filtergraph; real sidechain ducking; `Encoder::open_muxed*` muxes AAC | ✅ done — 7 unit + 2 gated mix tests; ducking verified ≥3dB band-isolated; e2e render = h264+aac/48k stereo |
| M7 | `scene-align` | markers-file connector (`cue t0 t1`, even word spread); WhisperX JSON parser (order-matched); `engine align` → `--timings` JSON | ✅ done — 7 tests incl. connector→realize integration; e2e align→render verified |
| M8 | `scene-cap` | `[capabilities.*]` in scene.toml; env + OS-keychain (`security`/`secret-tool`) credentials → `SCENE_CAP_AUTH`; subprocess + curl-HTTP connectors; `engine cap list/call`; reference TTS + image connectors | ✅ done — 9 hermetic tests incl. real subprocess roundtrip; e2e `cap call` verified |
| M9 | `scene-script` | QuickJS sandbox (`rquickjs` no-std): `setup(d)` once → `render(ctx, f, d)` per frame → JSON DrawList (`rect`/`circle`/`text`); `<program src with>` in markup+IR; `ProgramSource`/`SandboxPrograms`/`NullPrograms` seam; raster replays ops in element-local space | ✅ done — 8 sandbox tests (fs/process denied, determinism, malformed-op tolerance, mem+instr limits ~0.5s abort) + markup lowering + pool pixel test; e2e render verified real ops in h264 pixels |
| M10 | adapt | `scene-adapt`: ingest (path / yt-dlp URL) → probe → shot-detect (96×54@4fps decode, mean-abs-diff, median+6·MAD robust threshold, min-shot merge) → `emit_scene` draft markup; `engine adapt` | ✅ done — 10 tests: diff/cut math + emitted markup lowers clean through our own parser; gated test found real cuts @2s/4s in a 3-shot lavfi clip; e2e adapt→check→render h264+aac verified |
| M11 | ship | `SKILL.md` authoring reference (original); `docs/playbooks/` (word-timed captions, program overlay, adapt); `release.yml` tag→3-OS binaries; README refresh | ✅ done — release build smoke: `doctor`/`init`/`check`/`align`/`render` all work on the installed binary; `<program>` ops verified in release output pixels |
| M12 | ui | `engine ui` — localhost test UI. Hand-rolled HTTP/1.1 (zero new deps): editor + `check`/`render` JSON APIs + `/out/` range-served mp4. `render_inner` shared with CLI | ✅ done — 11 ui tests (range parsing incl. inverted-range no-panic, traversal 403, JSON content-type gate, validate-before-save); live verified: check diags, render→mp4, 206 seeking |
| M13 | meme | `scene-meme` + `engine meme` — flash-cut pipeline per `docs/flash-cut-pipeline.md`: `PcmStream` audio decode, fused visual+audio peaks, fact-sheet Jev routing, Gemini on kept frames only, content-hash stage caches, `from`-offset `.scene` emit; all capabilities optional (offline dHash mode complete) | ✅ done — 50 unit + 6 gated tests; e2e: synthetic 4-cut clip → fused `cut_on_beat` keeps → valid `.scene` → rendered mp4; rerun hits caches |

Gates that need ffmpeg mark the test `#[ignore]` unless `SCENE_MEDIA_TESTS=1`
is set — CI runs them on the legs that install ffmpeg.

## Hardening invariants (post-M12 review passes)

Facts a change must preserve — each is pinned by a test:

- **Worker-count determinism covers state.** Before rasterizing its
  shard, a worker replays every `ops()` call frames `0..shard.start`
  would have made (layout only, no pixels). A `render()` that
  accumulates on `d` produces identical output at 1 or N workers — and
  under `--frames` windows, which replay the same prefix from scene
  start rather than from the window's first frame.
- **Program state is per element instance.** The sandbox keys each
  `Program` by element identity, not `(src, with)` — two identical
  `<program>` elements get independent runtimes, so `setup`/`d` can't
  leak between them.
- **Sequential media can go backward by reopening.** A `sample` target
  behind the decode head respawns the stream (with `-ss` when deep);
  serving the stale current frame is the bug that motivated it.
- **`--frames` windows rebase audio.** `AudioGraph::window` clips the
  mix to the rendered range — source reads shift by the cut front,
  delays rebase to the window start. A partial render hears its own
  span, not the opening.
- **ffmpeg pipes are drained concurrently.** stderr runs on a thread
  into a bounded tail; a verbose child can't deadlock the pipe.
- **Credentials never touch argv.** HTTP connector auth goes through a
  0600 `curl -K` config file; subprocess connectors see only
  `SCENE_CAP_AUTH` in their environment. Env and keychain refs resolve
  identically — both produce the header.
- **The UI serves only `out/`'s real contents.** Canonical-path
  containment rejects symlink escapes; malformed percent-encoding and
  inverted ranges return 4xx/200, never panic.
- **A worker panic is an `Err`, not a dead process.**
- **Markup depth is bounded at the parser.** `parse_node` caps at 128
  levels; every downstream pass (lower, resolve, layout, raster) walks
  the same tree, so one bound covers them all.
- **Authored paths stay inside the project root.** `src` attributes
  resolve through canonicalized confinement in the sandbox and warn in
  lowering; `<render target>` gets the same treatment at render time —
  lexical `..` normalization plus canonicalization of the deepest
  existing ancestor, so symlinks inside the root can't tunnel out.
  `--out` is the operator's argument and is used verbatim.
- **Every subprocess has a deadline, and deadlines kill the tree.**
  `proc::wait_timeout` / `output_timeout` kill+reap past a limit on all
  wait/output call sites. Managed children spawn in their own process
  group (`spawn_grouped`), so the kill takes descendants too — a
  connector's `curl` grandchild can't outlive its parent holding our
  pipes open. Streaming decode/encode can't use a wait deadline (a
  blocked pipe `read`/`write` can't rescue itself), so they arm a
  `StallWatchdog`: heartbeat per successful I/O, `killpg` on unix /
  `taskkill /T` on Windows when it goes stale, fixed deadlines on EOF
  reap and muxer teardown.
- **Renders publish atomically.** The encoder writes a unique sibling
  temp (`name.tmp-PID-SEQ.ext`); the final target is replaced by rename
  only after `finish()` succeeds — `-y` can truncate the temp, never a
  previous good output. Intermediate WAVs get unique names too, so a
  failed render deletes only its own litter.
- **`adapt` imports footage into the project.** With `--out`, a source
  outside the scene's root is copied into its `assets/` (never
  clobbering an unrelated file — numeric suffixes), and downloads
  relocate outright; the emitted `src` always resolves inside the root
  it will be rendered under.
- **`from` is a source offset, never program time.** `clip`/`music`/
  `sound` sample the file starting at `from_s` — raster shifts the
  element-local frame by `from_s×fps`, the audio graph carries
  `src_start_s`. Seconds literals only; a frame count can't parse
  because source fps is unknown at parse time.
- **The meme pipeline's data boundary is structural.** Code and
  embeddings see pixels; Jev receives only readable fact rows (tests
  grep the request for vector/base64 keys); Gemini receives only
  materialized keep files — never the source at native fps. Every stage
  cache key names all its inputs, so a changed video/encoder/brief can't
  silently reuse a downstream artifact; absent capabilities degrade to
  warnings, not errors — offline runs still emit scene + keeps.
- **Asset and program failures report through `WarnSink`.** Media
  sources and `SandboxPrograms` share one `BTreeSet` sink — dedup for
  free, surfaced in the render diagnostics bundle and the UI.
- **Frames stream through bounded per-shard channels.** Each worker
  pushes rendered frames into its own `sync_channel(SHARD_QUEUE)`; the
  caller drains shards in index order — disjoint contiguous ranges make
  that ordered concatenation, no reorder buffer — and feeds the encoder
  through a callback, all inside `thread::scope`. Peak frame memory is
  `workers×(q+1)+1` frames (~340 MB at 8 workers/1080×1920) regardless
  of duration, not the whole video. Cancellation is flag + dropped
  receivers (a blocked `send` never observes a flag); the initiating
  error always propagates. `render_frames` remains as a collecting
  wrapper for tests and short clips.

## Timing semantics (locked by M1 tests)

`scene-time::realize(scene, timings) → (Option<ResolvedScene>, Vec<Diagnostic>)`:

- **Lattice.** A timing source's word-boundary lattice is every word's
  start time plus the final word's end, sorted. `±Nw` walks this lattice;
  it may cross cue boundaries (`hook+4w` can land inside `payoff`) and
  clamps at both ends.
- **Quantization.** Instants → frames: start floors, end ceils, so a range
  covers every frame it touches (`0s..0.51s` @30fps → frames `0..16`).
  Boundary-snapped values don't jitter (`1.0s` → exactly frame 30).
  Samples quantize identically at a fixed 48 kHz program rate.
- **Program span.** `0..max(timing-source end, every resolved end)`.
  Untimed elements inherit — children from their parent, roots from the
  program span. A scene where nothing resolves past t=0 is an error.
- **Errors, once.** Pass 1 validates and bounds; pass 2 rebuilds the tree
  silently. Every malformed anchor produces exactly one diagnostic.

## Element vocabulary (v1)

`clip`, `image`, `text`, `board`, `captions`, `music`, `sound`, `program`
(the M4 QuickJS escape hatch, shipped). Common attributes: `id`, `during`,
`at`, `anim`; `program` takes `src` + optional `with` JSON.

## Non-goals for v1

- Arbitrary HTML/CSS rendering — curated element set instead, with a
  browser-backed fallback only if a real component demands it.
- A GUI — `engine ui` (the localhost page) first; native inspector later.
