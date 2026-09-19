# Dev Log

Chronological record of work on this repo. Newest last.

## 2026-09-18 — `4e0362d` Initial commit: clean-room word-timed scene engine

All 11 planned modules landed as one sequential build (see `DESIGN.md`
for the per-module gates): markup → IR → timing → layout → raster →
audio → mux, plus alignment connectors, capability registry, the QuickJS
`<program>` sandbox, `adapt` ingestion, and the `SKILL.md` authoring
reference. ~160 hermetic tests; ffmpeg-dependent tests gated behind
`SCENE_MEDIA_TESTS=1`.

## 2026-09-18 — `e7e4e3c` Localhost web UI + first hardening pass

Added `engine ui` — a dependency-free HTTP/1.1 server on 127.0.0.1 with
an editor page, `/api/check`, `/api/render`, and range-served `/out/`
playback. `render_inner` was factored out of the CLI so UI and CLI share
one render path.

The same commit fixed six review findings: an inverted-Range slice panic
(one crafted header killed the server), `setup(d)` mutations not
reaching `render()` (the `with` object was re-parsed per frame), a
`setFont`/`setFontSize` name mismatch between docs and sandbox prelude,
hardcoded `fps="30"` in `emit_scene` (now emits the analyzed rate as an
exact rational), JSON endpoints accepting any Content-Type (now `415`,
blocks drive-by POSTs), and `with` accepting non-object JSON.

Hardening alongside: 10s socket timeouts on the serial server,
once-per-program warnings for eval failures and dropped DrawOps,
canonical-path containment on `<program src>` (covers `..`, absolutes,
symlink escapes), `api_render` validates before it may overwrite
`main.scene`, sandbox cache keyed on `(src, with)`, and the `FrameStream`
spawn plumbing deduplicated.

Verified live against a running server plus a real 60-frame render.

## 2026-09-19 — `a326def` Second review round: Modules A–F

A fresh review found crash/hang/correctness issues the first pass
missed. Patched as six gated modules, each with its own tests:

- **A — UI server.** `url_decode` no longer panics on multibyte UTF-8
  after `%` (byte-slice the input, not the `&str`); range responses read
  only the requested span via `seek`+`take` — still buffered in memory
  then (true chunked streaming landed in the next round); `out/` rejects
  symlink escapes (canonicalize + `starts_with`); `HEAD` answered with
  headers and no body; content types by extension; `api_check` now runs
  the same missing-asset scan as CLI `check`. (The "stale timings"
  bullet overstated the check — it only tested file existence. Real
  fingerprint-based detection landed in the next round.)
- **B — ffmpeg stderr deadlock.** Both decode and encode piped stderr
  but only drained it after the child exited — a verbose child (>64 KB
  of stderr) blocked on write while the parent blocked on stdin/stdout,
  hanging the render forever. New `stderr.rs` drains concurrently into a
  bounded tail used for diagnostics.
- **C — worker panic isolation.** `render_frames` now returns `Result`;
  each worker's shard loop runs under `catch_unwind`, so a raster panic
  surfaces as an error instead of taking down the process (and the UI
  server with it).
- **D — sandbox.** `setup()` evaluates under its own instruction budget
  instead of sharing frame 0's; `resolve()` distinguishes "escapes root"
  from "doesn't exist"; `with` JSON is canonicalized for the cache key
  so `{"a":1}` and `{"a": 1}` share one program.
- **E — commands/adapt/misc.** `adapt` no longer emits absolute paths
  for sources inside the project; `program.wav` is removed by a drop
  guard on success and failure; failed encodes delete the partial mp4;
  `--frames N` renders a single frame; `emit_scene` normalizes cuts
  (drop NaN/out-of-range, sort, dedup); declared MSRV corrected to
  Rust 1.88 (let-chains + edition 2024).
- **F — sequential media seek.** `SeqFrameSource` opens with `-ss` half
  a frame before a deep first target, so a worker whose shard starts at
  frame N no longer decodes frames 0..N per clip. Pixel-equality gated
  test verifies the seeked decode is bit-identical to the sequential one.

## 2026-09-19 — `89294a2` Third review round: determinism + correctness

- **Stateful `<program>` output depended on worker count.** Each worker
  built fresh sandbox state at its shard boundary, so a `render()` that
  accumulates on `d` diverged from a single-worker run — breaking the
  pool's byte-identical-across-workers contract. Fix: before rendering
  its shard, each worker replays `ops()` calls for the shard's prefix
  frames through `layout_frame` (layout only, no raster), reproducing
  the exact call sequence — including draw gating and board-nesting
  order. Scenes with no `Program` elements skip the pass entirely.
  Regression test proven to fail without it.
- **A clip reused by a later element froze on the previous tail.** The
  sequential stream only advanced forward; a backward target served the
  stale `current` frame forever. `sample` now reopens the stream (with
  `-ss` when deep) on backward targets. Gated test: restart is
  pixel-identical to a fresh decode of frame 0.
- **`--frames a:b` rendered the wrong audio.** The video honored the
  range but the mix always started at program t=0. New
  `AudioGraph::window(start, end)` clips each clip to the window —
  shifting `src_start_s` by the cut front, rebasing delays to zero,
  remapping duck links — so a partial render hears its own span.
- **HTTP capability connectors mishandled credentials twice.** Env-var
  auth resolved the secret but never sent the header (the gate only
  matched keychain refs), and the header itself was a literal argv
  element — visible in `ps` to any local process. Now any resolved
  secret produces the header, and it travels in a `curl -K` config file
  written 0600 and deleted after the child exits.
- **TTS connector requested the wrong audio format.** `output_format`
  went in the JSON body where ElevenLabs ignores it — the API defaulted
  to MP3, which the script then decoded as raw PCM. Moved to the query
  parameter per the API reference.
- **`adapt --out nested/draft.scene` emitted cwd-relative sources.** The
  draft resolves assets against its own directory; a nested output now
  gets a correctly relativized `../…` path (absolute when the source
  lives outside the project tree).

## 2026-09-19 — Fourth review round: 13 fixes + doc corrections

- **Short audio truncated the video.** The mix's `-t` capped length but
  couldn't pad — a 0.5s sting in a 3s program produced a 0.5s WAV and
  the muxer's `-shortest` ate the video. `apad=whole_d=<program>` now
  fills the tail with silence; a gated test asserts a 3s WAV with a
  silent tail.
- **Unicode input crashed parser and color parse.** `skip_ws` bumped
  one byte per char — a nonbreaking space (2 bytes) left the cursor
  mid-character → panic on the next slice. `Color::parse` sliced
  `hex[0..1]` — `#中` panicked instead of erroring. Both fixed; both
  produce diagnostics now.
- **Shell connectors exposed API keys in argv.** Both reference
  connectors now pass credentials through a `curl -K` config tempfile
  (0600, trap-cleaned) — `ps` sees no header.
- **Capability subprocess deadlock.** The engine wrote the whole stdin
  request before draining stderr — a connector that logged >64KB first
  blocked both pipes. stdin writes and stderr draining now run on
  threads while `wait()` reaps. Regression test floods stderr before
  reading; a `recv_timeout` turns a re-deadlock into a failure, not a
  hang.
- **Explicit anchor edges were overwritten.** `hook.end..payoff.start`
  became `hook.start..payoff.end`. Range parse now applies positional
  defaults only when the author didn't write a `.start`/`.end` suffix.
- **Nested boards rendered blank.** Board children measured through the
  flat `Measure` (which returns `None` for boards) and grandchildren
  were hardcoded empty. `stack_size`/`board_size`/`place_stack_child`
  now recurse — nested boards size from their own stack and place their
  descendants.
- **Transparent images double-composited.** Straight-alpha RGBA went
  into a premultiplied pixmap — a 50% pixel read as >100% color. The
  rasterizer now premultiplies; a pixel test pins (128,0,0,128)-on-black
  → ~128 red, not 255.
- **Stale timings silently reused.** `TimingMap` gained a `script_hash`
  field — `engine align` stamps the script's FNV-1a fingerprint; render
  warns "re-run engine align" when it doesn't match. Files without the
  field (older, hand-written) skip the check.
- **Missing assets rendered silently.** `render_inner` now emits the
  same existence warnings `check` does, plus a shared `WarnSink`
  collects decode-time failures (`exists but won't open`) once per
  asset — the report and the UI's `diagnostics` show them. Missing
  images draw the slate placeholder like clips already did.
- **`hook 0 NaN` wrote `null` into timings.** Marker parse rejects
  non-finite timestamps before they reach the JSON.
- **A stale `out` file masqueraded as connector output.** `fulfill`
  deletes the destination before spawn — a connector exiting 0 without
  writing now correctly reports `NoOutput`.
- **Caption defaults depended on document order.** `script_track` is
  pre-scanned from the `<scene>` children before lowering, so
  `<captions>` works whether `<script>` appears first or last.
- **UI file responses truly stream now.** `Response.body` is
  `Bytes | File` — file bodies copy through a 64KB buffer in `handle`,
  so a plain GET on a large mp4 never buffers it whole.
- **CI runs the media suite.** The workflow checks ffmpeg/ffprobe are
  present (fails loudly if an image drops them) and reruns the tests
  with `SCENE_MEDIA_TESTS=1`.

Docs corrected alongside: `adapt-clip.md` bind example uses `.text`
and no longer claims single-shot footage emits zero boards; `DESIGN.md`
lists `program` as shipped and names `engine ui`; the Module A entry
above now honestly describes buffered ranges.

## 2026-02-11 — Module G: streaming render pool

`render_frames` used to materialize every frame before encoding — at
1080×1920 RGBA a 60 s/30 fps render held ~15 GB in RAM. Now
`render_frames_into` streams frames to a `consume` callback:

- **Bounded per-shard channels.** Each worker owns a contiguous shard
  and pushes frames into its own `sync_channel(4)`; the caller drains
  shard 0, then shard 1, … — disjoint ordered ranges make sequential
  draining ordered concatenation, no reorder buffer. Receiving runs
  *inside* `thread::scope` — with bounded channels, receive-after-join
  deadlocks on `send`. Peak frame memory ≈ `workers×5+1` frames
  (~340 MB at 8 workers) instead of all frames — measured on a
  600-frame 1080×1920 render: 360 MB peak footprint vs ~5 GB before;
  resident total is ~1.3 GB, dominated by per-worker font/renderer
  state that doesn't grow with duration.
- **Encode overlaps render.** `commands.rs` passes `encoder.write_frame`
  as the callback; first frame reaches ffmpeg while the last is drawn.
- **Cancellation is flag + receiver-drop.** A blocked `send` can't see
  a flag — dropping the receivers makes it fail. On `consume` error or
  a worker's terminal report, the flag is set, receivers drop, workers
  exit, `thread::scope` joins, and the initiating error propagates;
  `Encoder::drop` kills ffmpeg and the `-y`-truncated partial mp4 is
  removed. Worker panics still arrive as tagged `Err` on a separate
  control channel.
- `render_frames` stays as a collecting wrapper so the existing
  determinism suite exercises the streaming path unchanged.

Trade-off: strict ordered delivery + small buffers mean later workers
block once their shard queue fills — sustained parallelism collapses
toward the current shard's pace. `SHARD_QUEUE` is the dial; memory
stays ≪1 GB well past `q=16`.

## Deferred, lower stakes

`open_scaled` can't take a seek window; `read_request` doesn't parse
chunked request bodies; `emit_scene` reuses the source video as the
draft's `<music src>` (intentional placeholder); no LICENSE file yet.
