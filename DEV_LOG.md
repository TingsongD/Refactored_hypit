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

## 2026-09-19 — Module G: streaming render pool

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

## 2026-09-20 — Review round 3: input-craft and hang hardening

A full-codebase review after Module G surfaced six more fixes:

- **Parser depth cap.** `parse_node` recursed per nesting level with no
  bound — a ~10k-deep document segfaulted the process (uncatchable, and
  it takes the UI server down too). Cap at 128; the over-deep doc is a
  diagnostic. Since every downstream pass (lower/resolve/layout) walks
  the same tree, one bound at the parser bounds them all.
- **Mid-stream decode errors reach the sink.** `SeqFrameSource` folded
  `Some(Err)` into the EOF arm — a corrupt-in-the-middle clip silently
  froze on its last good frame. Now the error warns once per source and
  the stream is marked dead so a failed child is never re-polled.
- **`<render target>` is confined.** It used to resolve verbatim —
  `../` or absolute targets let markup create dirs and `-y`-truncate
  files anywhere ffmpeg could reach. `confine_target` normalizes
  lexically, then canonicalizes the root and the target's deepest
  existing ancestor so a symlink inside the project pointing out is
  refused too. `--out` stays the operator's own argument, verbatim.
  `lower` flags `..`/absolute targets at check time.
- **Program warnings route through `WarnSink`.** `SandboxPrograms`
  `eprintln!`ed refused/missing/failed scripts — the UI never saw them.
  `with_warnings` feeds the shared sink, same contract as the media
  sources; dedup falls out of the `BTreeSet`.
- **Input validation.** `parse_gain_db` rejects `nan`/`inf`/`1e999`
  (they used to reach the filtergraph as `volume=nan`). New
  `TimingSource::validate` reports words with non-finite/negative or
  reversed times — whisperx and `--timings` files both checked.
  `parse_frame_range` uses `checked_add` so `--frames 4294967295`
  returns `None` instead of wrapping.
- **Every subprocess has a deadline.** New `proc::wait_timeout` /
  `output_timeout` (kill+reap, drained pipes) cover `ffprobe`, the mix
  render, `yt-dlp`, keychain reads, connectors, `curl` (plus its own
  `--max-time 300`), and `doctor`. Streaming decode/encode can't use a
  wait deadline alone — a blocked pipe `read`/`write` can't rescue
  itself — so they carry a `watchdog::StallWatchdog`: an atomic
  heartbeat per successful I/O, SIGKILL by pid on unix when it goes
  stale (5 min decode / 2 min encode), plus fixed deadlines on EOF reap
  and muxer teardown. On non-unix the watchdog compiles to a no-op;
  `wait_timeout` still applies everywhere.

Gate: fmt, clippy `-D warnings`, workspace tests, `SCENE_MEDIA_TESTS=1`
— all green.

## 2026-09-20 — Review round 4: uniform src confinement

The last review caught that confinement was uniform everywhere *except*
the most common path: media `src`.

- **Media `src` is now confined like everything else.** `clip`, `image`,
  `music`, and `sound` used bare `root.join(src)` — an absolute path or
  `..` sailed through to ffmpeg/the image decoder while `program` src
  and `<render target>` were already refused. New shared helper
  `scene_media::confine_under_root` (lexical `..` normalize +
  canonicalize deepest existing ancestor → symlink-out fails too) is
  used by `SeqFrameSource`, `StillFrameSource`, `AudioGraph::collect`,
  and `confine_target` — one implementation, four call sites. Escapes
  warn once (`WarnSink` / diagnostics) and degrade like a missing file
  instead of killing the render. `src_attr` in lowering now flags
  absolute paths at `check` time, not just `..`.
- **Disarm-before-reap in decode EOF.** `FrameStream`'s EOF arm reaped
  with `wait_timeout` *then* disarmed the stall watchdog — backwards:
  a reaped pid is recyclable while the watchdog was still armed. Now
  disarm runs first (matching `Encoder::finish`); the reap deadline is
  `wait_timeout`'s own kill path.
- **Doc corrections.** `adapt-clip.md` suggested `gain="-inf"`, which
  the new finite check rejects (`-60dB` is the mute). `duck` is now
  documented as `<music>`-only — on `<sound>` it's dropped with a
  warning. DEV_LOG date ordering fixed (Module G / round 3 were
  mislabeled 2026-02-xx).

Gate: fmt, clippy `-D warnings`, workspace tests — all green.

## 2026-09-21 — Review round 5: output safety, duck fixes, tree kills

Nine findings, all verified against code before patching — including one
round-4 regression (`adapt` emitting `..` srcs the new confinement
refuses).

- **Renders publish atomically.** The encoder used to take the final
  target with ffmpeg `-y`, so a mid-render failure had already truncated
  the previous good output before cleanup ran — and the predictable
  `*.program.wav` temp could clobber *and then delete* an unrelated user
  file. Video now encodes to a unique sibling temp and is renamed over
  the target only after `finish()` succeeds; the WAV mix gets its own
  unique name. On any failure the old output survives untouched.
- **Ducking no longer truncates music at the key's end.** The sidechain
  key submix had no `apad` — `sidechaincompress` (a framesync filter)
  stops when its secondary input ends, so a 3s bed under a 1s voice went
  silent at 1s. The key submix is now padded to program length before
  compression (gated test: `ducked_music_survives_past_the_key`).
- **Multiple ducks can share one key.** `asplit=2` emitted a single
  `[a{i}k]` label that every consuming link reused → `Invalid stream
  specifier`. Each key clip now splits N+1 ways (main + one label per
  link); gated repro `two_ducks_share_one_key` mixes clean.
- **Timeouts kill the process tree, not just the child.** A connector
  script's `curl` grandchild inherited our pipes, survived the parent
  kill, and hung the drain join forever. `spawn_grouped` puts every
  managed child in its own process group (unix) so `wait_timeout`,
  `output_timeout`, and the stall watchdog `killpg`/`taskkill /T` the
  whole tree — pipes reach EOF, drains finish, no zombies. The watchdog
  is real on Windows now (`taskkill /PID /T /F`) instead of a no-op.
- **`adapt` imports footage instead of emitting escapes.** Round-4's
  confinement made `adapt clip.mp4 --out nested/draft.scene` produce
  `src="../clip.mp4"` — unrenderable. With `--out`, footage outside the
  scene's root is now copied into `assets/` (downloads move outright),
  never clobbering an unrelated file (`name-1.ext` suffixes); sources
  already inside are left alone.
- **Program state is per element instance.** The sandbox cache keyed
  `(src, with)` — two identical `<program>` elements shared one runtime
  and their `d` state leaked together. `ops()` now takes a stable
  element key (the resolved element's address); same src+with, different
  elements → independent counters and `setup`.
- **`--frames` windows replay program state from zero.** Warm-up ran
  `range_start..shard.start`, so `--frames 90:120` skipped the state
  built over 0–89. Now `0..shard.start` — a window pixel-matches the
  same frames of a full run (new test proves it, 1 and 2 workers).
- **`doctor` checks exit status and uses the right flag.** Any spawned
  process reported "ok" — even `yt-dlp -version` exiting nonzero.
  Version args are per-tool (`-version` for ffmpeg/ffprobe, `--version`
  for yt-dlp/uv) and a nonzero status reports MISSING with the exit code.

Gate: fmt, clippy `-D warnings`, workspace tests, `SCENE_MEDIA_TESTS=1`
— all green, including two new FFmpeg-gated ducking regressions and the
process-group kill tests.

## 2026-09-21 — Flash-cut pipeline: `engine meme` (M-0…M-J)

The `docs/flash-cut-pipeline.md` handover, implemented end to end as the
`scene-meme` crate plus one new CLI verb. Pixels stay local; Jev and
Gemini are optional `scene.toml` capabilities, so a bare checkout still
runs the whole funnel in dHash mode.

- **`from` source offsets (M-0).** `<clip>`, `<music>` and `<sound>`
  accept `from="12.37s"` — element frame 0 samples source time `from`,
  so emitted beats can window the source without materializing media.
  Verified pixel-exact: `from="1.0s"` frame 0 equals the plain clip's
  frame 30; audio slices keep their offset through the graph.
- **`PcmStream` (M-B).** FFmpeg-decoded mono PCM at the analysis hop
  rate, mirroring `FrameStream`'s spawn/drain/watchdog shape. `None`
  when the source has no audio stream — silence is data, not an error.
- **Perceive + metrics (M-C).** One decode pass fills per-frame luma
  metrics (sharpness, motion, brightness, contrast, dHash `change`) and
  per-hop audio metrics (loud_db, spectral flux via rustfft, onset,
  silence, word). Word snapping reads `--timings` word lattices.
- **Fused peak pick (M-D).** Four co-equal generators — signature-change
  spikes, pixel-diff spikes (flat-color cuts move every pixel while the
  dHash can't see it), audio onsets, and importance maxima — merged
  within `min_gap`, snapped to onset/word boundaries, deduped against
  the kept set. The sharpness gate stands down on textureless clips;
  dedup needs structure AND level match (dHash ⊕ luma), so a red beat
  and a blue beat are different beats.
- **Content-hash cache + embed connector (M-E).** Every stage key names
  all inputs (`metrics:{video,encoder,…}`, `peaks:{video,encoder,brief}`
  …); writes are temp+rename. `connectors/embed.py` is a `uv` reference
  script returning frame + vocab embeddings; `encoder = "dhash"` needs
  nothing external.
- **Pack + jev_route (M-F).** Fact sheets are readable rows only — the
  data-boundary tests grep for embedding/base64/dhash keys. Keep-rule in
  code: `keep ∧ too_similar<0.5 ∧ cut_strength≥3`; confidence under the
  floor flags `low_confidence` instead of auto-exporting.
- **Gemini connector (M-G).** Stills mode (one labeled PNG per keep) is
  default; `--gemini-mode windows` transcodes `gemini_window_sec` clips
  at `gemini_fps` — Gemini's 1 fps default would step over 2–6-frame
  cuts. `connectors/gemini.sh` inlines parts; the key rides via a `curl
  -K` file, never argv.
- **Typed package (M-H).** `export | need_more_peaks | rerun_window`
  parses strict; `rerun_window` re-analyzes only the named keep's window
  (per-window cache keys), bounded at 2 loops. Low-confidence routes
  force `need_more_peaks`.
- **Emit (M-I).** Each keep → `<clip from>` + matching `<sound>` slice
  so audio cuts with picture; `--materialize` re-encodes physical beats
  to `out/beats/`; optional `<music>` bed. Output writes are atomic.
- **CLI + e2e (M-J).** `engine meme in.mp4 --brief b.toml --out out/
  --timings t.json --config scene.toml [--materialize|--beat-sec|
  --gemini-mode|--rerun-window|--music]` writes metrics/peaks/pack/
  keeps/gemini/package JSON + `meme.scene` + a per-stage report with
  cache hits. Gated e2e proves the loop: synthetic 4-cut clip →
  `f15`/`f30` keeps (the swell-fused beat lands `cut_on_beat`), the
  emitted scene passes `check`, and `render` produces 1.2s of mp4.
  Second run hits perceive/peaks caches.

Found while validating on flat-color fixtures: spectral-flux hop 0
reported the whole spectrum as novelty (false onset at t=0 — now primed,
not spiked), and flat frames dedup-collapsed (see M-D above).

Gate: fmt, clippy `-D warnings`, workspace tests, `SCENE_MEDIA_TESTS=1`
workspace run — all green; `scene-meme` is 50 unit + 6 gated tests.



## Safe output and subprocess follow-up

Plan and rollback: [docs/SAFE_PATCH_PLAN.md](docs/SAFE_PATCH_PLAN.md).

- Replaced predictable render intermediates and delete-then-rename recovery
  with shared, exclusively created staging directories and a single publish
  rename. Unix staging starts with mode 0700. Capability calls now use the
  same path and preserve old assets after spawn failures, failed execution,
  missing output or empty output. Auxiliary files cannot collide with the
  asset basename. Extensionless renders still select MP4.
- Capture deadlines now include output EOF after the parent exits. A Unix
  process group / Windows Job Object remains available to kill descendants.
  Connector stdin uses an anonymous file, stderr remains bounded, and unused
  stdout remains discarded. Deliberately daemonizing connectors are outside
  the managed group/job contract.
- Media watchdogs track only outstanding pipe operations. Idle workers and
  completed operations can wait indefinitely for demand; stalled reads and
  writes still terminate the child. Disarming wakes the watchdog immediately.
- Bare `adapt --out draft.scene` imports external footage into the current
  project's assets directory.
- Fixed the duck validation Clippy warning and current stable's fixed-size
  chunk warnings without suppressing lints. Replaced shell-only connector
  fixtures with portable test executables; lifecycle tests run on all CI OSes.

Local validation (Linux, Rust 1.98.1): strict Clippy, the workspace suite,
plus the full FFmpeg-enabled suite pass. Regression coverage includes failed
publication and connector preservation, private staging, parent-first exits,
unread stdin, idle/active watchdogs, bare-output adaptation, and real encoder
success/failure. Cross-platform CI is the remaining PR gate.

## 2026-09-21 — Consolidation and review acceptance

Merged local `3e2ba28` and upstream `dff979c` as `2ff4d2b`, preserving both
histories and their log/test additions. Recovery branch:
`recovery/pre-consolidation-3e2ba28`; delivery branch: `fix/consolidate-review`.

Fixed review S1–S6, F1–F8, C1–C4, I1 and D1. The traceability table and limits are
in `docs/consolidation-review.md`. Highlights: content-addressed staged assets;
versioned complete cache keys with exact float round trips; validated embeddings;
quality-ranked dedup, separate representative timing and onset/snapping controls;
explicit pretrained model/checkpoint selection; strict brief parsing; offline
provider adapters, preflight callbacks and usage/cost fields; persistent partial
retry corrections; overflow-safe word offsets; audio for silent-source music.

Completed the deferred shared renderer queue (4×workers outstanding), incremental
program-state replay, scaled source-time windows and bounded chunked HTTP parsing.
Follow-up review caught fractional/offset timestamp boundaries and descendant
stderr teardown; regressions cover both. Streaming process managers retain the
Unix process group or Windows Job after the parent exits.

Local acceptance: formatting and strict all-target Clippy pass; hermetic workspace
run passes; latest FFmpeg-enabled workspace run: **332 passed, 0 failed**, with
four ignored subprocess fixtures invoked by parent tests. Four Python adapter
contract tests pass. No paid requests or model downloads. Release CLI generated
and rendered an 18-frame, 160×90 scene with an audio stream; the large-word-offset
regression also passes under release optimization. Complete README, SKILL,
USER_GUIDE and word-timed-caption examples pass `engine check` (missing example
assets remain expected warnings).

CI now installs FFmpeg explicitly on all three operating systems and runs offline
connector tests plus both Rust suites. Cross-platform publication remains gated
on the actual branch workflow results, recorded in the delivery report; these
local checks are not a claim of a live provider or cross-platform pass.

Added a proprietary/all-rights-reserved notice granting no usage rights. Third-party
terms and repository visibility remain unchanged.

The first branch CI run found Rust 1.98's new constant-chunk Clippy lint (local
initial checks used 1.94). PCM, metric and pixel-test iteration now uses typed
array chunks; the lint is fixed rather than suppressed. The CI-matching 1.98
toolchain is also used for local follow-up validation.

A final complete-example audit corrected the adapt playbook's literal ellipsis
and outdated path explanation; its scene now passes validation. A tiny positive
beat-width smoke test exposed a zero-frame export: widths shorter than one output
frame now fail after probing, before perception or any provider call.

The second CI run passed Linux/macOS and exposed remaining slash-literal
assertions in the Windows adapt-path test. The entire test now compares native
Path values, including the absolute-source case, instead of platform strings.
