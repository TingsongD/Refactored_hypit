# Flash-cut pipeline — dev handover

Status: implemented (`engine meme`, crate `scene-meme`; see DEV_LOG
round "Flash-cut pipeline M-0…M-J"). Connectors `connectors/jev.sh`,
`connectors/gemini.sh`, `connectors/embed.py` are reference scripts —
adapt their wire projections to real endpoints before relying on them.
Owner intent: find the beat frames in a 30 fps clip, keep flash-cut
peaks, analyze only those frames, emit an editable `.scene` draft.

Rule: embeddings and code see pixels. Jev routes on text. Gemini sees
only approved frames. The audio track is a first-class signal — meme
cuts are audio-driven.

## 1. Goal

Given a video, return:

- a timestamped keep list for a fast-cut / flash-cut edit
- a Gemini analysis of that sequence (joke, text, rhythm, whether the
  cut works)
- a final typed decision: `export` | `need_more_peaks` | `rerun_window`
- on export: a **`.scene` draft** — keep beats as sequential `<clip>`
  segments on the visual track, the source audio bed as `<music>` —
  the native input our renderer edits and renders

Do not send raw embeddings or raw video to Jev.
Do not send every frame to Gemini.

## 2. System split

| Stage | Who | Input | Output |
|---|---|---|---|
| brief | human / config | job text | locked `brief.yaml` |
| perceive | `FrameStream` + `PcmStream` + optional `embed` connector | every frame | metrics + embeddings |
| peak_pick | code (`scene-meme`) | metrics + embeddings + onsets | candidate peaks |
| score_pack | code | candidates | JSON fact sheets |
| jev_route | Jev (`scene.toml` capability) | fact sheets + brief | keep / skip / maybe |
| gemini_analyze | Gemini Flash (capability) | kept frames or windows | analysis JSON |
| jev_package | Jev (capability) | Gemini JSON + keep list | export route |
| emit | code (`emit_flash_scene`) | keep list + analysis | `.scene` draft |
| cache | `out/.cache/` | any approved stage output | reuse on retry |

Picsart-style loop: cheap decision before expensive generation. If
Gemini fails on one beat, rerun only that window. The visual stages are
pure Rust; every external model rides the existing connector contract
(JSON in → file out, auth via env/keychain, deadline + tree-kill).

## 3. Locked brief

Store once in `brief.yaml`. Never regenerate from a new prompt mid-run.

```yaml
job: flash_cut_meme
fps: 30
min_gap_frames: 2          # flash cuts land 2–6 frames apart — the gap
                          # rule must not starve them; dedup does the work
dup_cosine: 0.92           # embedding dedup (or dhash_hamming <= 6)
change_keep: 0.35          # embedding/metric spike = likely cut
change_skip: 0.15          # below this the frame is same-shot
min_sharpness: 0.35        # drop smear/mid-transition
brightness_lo: 0.05        # almost-black drop
brightness_hi: 0.97        # almost-white drop
onset_keep: 0.40           # spectral-flux spike = audio beat
snap_to_onset: true        # keep timestamps land on beats/words
snap_to_sharp: true        # keep frame lands on sharpest post-cut frame
max_candidates_to_jev: 60
jev_min_confidence: 0.55
gemini_model: gemini-3-flash-preview   # pin a real model ID
gemini_fps: 8
gemini_window_sec: 0.6
encoder: mobileclip2-s0    # via `embed` connector; absent → dHash mode
weights_id: mc2-s0-v1
```

Jev questions are written against this brief, not "does it look cool."

## 4. Perceive

Two decode passes, both inside `scene-media` with the standard
hardening (piped stdout, `StderrDrain`, `StallWatchdog`,
`spawn_grouped`).

### 4a. Video pass — `FrameStream::open_scaled(path, info, 224, 224, 30.0)`

Per frame, pure Rust on RGBA pixels — no deps:

```json
{
  "frame": 371, "t": 12.3667,
  "emb_id": "emb_000371",
  "sharpness": 0.81, "motion": 0.74, "brightness": 0.41,
  "contrast": 0.55, "change": 0.62,
  "tags": ["close-up", "face", "high-contrast"]
}
```

- `change = 1 - cosine(emb[i-1], emb[i])` when embeddings exist; else
  `frame_diff(prev, cur) / 32` — a fixed scale, not per-clip max, so
  thresholds are portable across videos.
- `motion` — mean abs pixel diff, same fixed scale.
- `sharpness` — Laplacian variance over luminance, rolling-window
  normalized.
- `brightness`, `contrast` — mean and stddev of luminance, 0–1.
- `tags` — CLIP zero-shot, fixed vocab, **computed on candidates only**
  (the fact sheet is the only consumer; tagging every frame wastes
  ~60× the text-tower work). Vocab: `close-up, wide, face, people,
  text-on-screen, dark, bright, action, idle, crowd, product,
  meme-format, blurry, logo`.
- `emb` — MobileCLIP2-S0 via the `embed` connector (a `uv` subprocess
  script in `connectors/` — same JSON-in/file-out contract as TTS).
  No connector → `dHash` mode: 64-bit perceptual hash per frame, Hamming
  distance does the dedup job. Embeddings store to `emb.npy`/`parquet`
  beside `out/`; they never enter a Jev request.

### 4b. Audio pass — `PcmStream` (new in `scene-media`)

`ffmpeg -vn -ac 1 -ar 16000 -f f32le -` on stdout — mono f32, hop of
`rate/fps` samples per video frame (512-sample hop, 1024 window at
30 fps). Per hop:

```json
{ "frame": 371, "loud_db": -14.2, "flux": 0.71, "onset": 0.80,
  "silence": false, "speech": true, "word": "boom", "beat_frame": 371 }
```

- `loud_db` — windowed RMS → dB.
- `flux` — spectral flux (sum of rectified frame-to-frame magnitude
  deltas): the onset novelty function. `rustfft` dep, or ffmpeg's
  `aspectralstats` side-channel if we want zero new deps — decide at
  M-B.
- `onset` — flux scored through the same median+6·MAD spike detector
  `detect_cuts` uses, normalized 0–1.
- `silence` — sustained `loud_db < -50` runs.
- `speech`, `word`, `beat_frame` — when `--timings` (the `align`
  product) exists, the word lattice says whether this hop is inside a
  word, which word, and the nearest word boundary in frames. This is
  the differentiator: the meme's speech rhythm is already parseable.

### 4c. Why both

A visual cut detector alone misses the actual structure of a flash-cut
meme — the "vine boom", the voice hit, the beat drop. An onset with no
visual spike is still a beat; a spike with no onset may be camera shake.

## 5. Peak pick (code only)

Cut detector + beat detector, fused. Do not replace with Jev.

```text
visual candidate:
  change[i] >= change_keep AND sharpness[i] >= min_sharpness
  AND brightness in [lo, hi] AND gap from last peak >= min_gap_frames
audio candidate:
  onset[i] >= onset_keep AND gap rule, same
candidate = visual OR audio spike
  coincidence bonus: visual ∧ onset within ±2 frames → strongest keep

snap:
  frame:  argmax(sharpness) over [peak, peak + min_gap) — never keep
          the blurry cut-boundary frame
  time:   nearest onset or word boundary within ±3 frames (snap_to_onset)

dedup (after the full pass, not streaming):
  max cosine(emb[i], kept) <= dup_cosine, or dHash hamming <= 6
  nearest_kept computed against the FINAL keep list — order-independent

importance = change * (0.4*motion + 0.3*sharpness + 0.2*contrast)
           + 0.5 * onset
keep local maxima of importance over ±min_gap windows
```

Expected: hundreds/thousands of frames → 20–80 candidates.

## 6. Score pack

Jev state is an array of readable rows, capped at
`max_candidates_to_jev` — pre-rank by importance, top 60 plus maybe
retries.

```json
{
  "brief": "flash-cut meme. keep punchy unique frames. drop shake, blur, duplicates.",
  "candidates": [
    { "id": "f371", "t": 12.37, "frame": 371,
      "change": 0.62, "sharpness": 0.81, "motion": 0.74,
      "brightness": 0.41, "contrast": 0.55,
      "loud_db": -14.2, "onset": 0.80, "speech": true,
      "word": "boom", "cut_on_beat": true,
      "faces": 1, "tags": ["close-up", "face", "high-contrast"],
      "nearest_kept": "f298", "nearest_kept_sim": 0.71 }
  ]
}
```

Hard limits: no embedding vectors, no base64 frames, no hex/RGB dumps,
one hop — point Jev at `candidates[i]`, nothing referential.

## 7. Jev route

One request, fan-out — shared state, one question set per candidate id.
Jev is a `scene.toml` capability, `Connector::Http`:

```toml
[capabilities.jev]
endpoint = "https://…/v1/systemone"
auth = { env = "TYPESAFE_API_KEY" }
```

Questions per candidate:

- **Choice route** — `keep` | `skip` | `maybe`
- **Score cut_strength 1–5** — idle / small move / usable change /
  hard cut / peak meme beat (face hit, text hit, snap, beat drop)
- **Score too_similar 0–1** — a continuous score, not a bool: `1.0`
  = same beat as `nearest_kept`. (Declared type fixes the original
  bool-vs-`<0.5` ambiguity.)

```text
keep  if route=keep AND too_similar < 0.5 AND cut_strength >= 3
maybe if route=maybe AND cut_strength >= 3
else  skip
```

Cache the keep list. Re-ask only when brief or thresholds change.
Never ask Jev to compare raw vectors or count frames — the numbers on
the sheet already carry that.

## 8. Gemini (Flash)

Send pixels, not embeddings. Two modes:

- **Stills** — kept frames as PNGs (`Pixmap::encode_png` or
  `ffmpeg -ss t -frames:v 1`), each labeled `t=12.37 frame=371`.
  Fits `Connector::Http` — base64 `inline_data` parts, same pattern
  `connectors/openai-image.sh` already runs.
- **Windows** — 0.6 s clips around each keep at `gemini_fps`.
  `generateContent` with a video part needs the Files API (upload →
  poll → generate): multi-request, so it's a **subprocess connector**
  (`connectors/gemini.sh`), not the single-shot Http shape.
  Default Gemini video sampling is ~1 fps and will miss 2–6-frame
  flash cuts — always set the fps explicitly.

Prompt must include: the locked brief, ordered keep timestamps with
audio annotations (`cut_on_beat`, `word`), and ask for: on-screen
text, subject, whether each cut reads, dead frames to drop, suggested
final order, one-line verdict.

```json
{ "verdict": "works",
  "joke": "…",
  "text_on_screen": ["…"],
  "beats": [ {"t": 12.37, "role": "punch", "keep": true, "note": "face snap on 'boom'"} ],
  "drop": ["t=8.10 too similar to 7.90"],
  "missing": "need a title card beat near 0.3s" }
```

Cost control: never attach the full source at 30 fps. Show cost before
Gemini fires (log per stage: latency, tokens, cache hit).

## 9. Jev package

State = Gemini JSON + keep timestamps + brief. Questions:

- **Choice package**: `export` | `need_more_peaks` | `rerun_window`
- **Score analysis_complete** (bool-ish noul)
- **Score postable 1–5**

`rerun_window` calls Gemini only on the named window — perceive, peak
pick, other keeps stay cached.

## 10. Cache

`out/.cache/` — new module, ~150 lines, `sha2` dep for content hashes:

```text
video_sha256
encoder + weights_id
brief_hash
metrics:{video,encoder}          → metrics.json
peaks:{video,encoder,brief_hash} → peaks.json   (encoder IN the key —
                                                peaks depend on embeddings)
jev_route:{peaks_hash,question_hash} → jev.json
gemini:{keeps_hash,prompt_hash,model} → gemini.json
jev_package:{gemini_hash}             → package.json
frames/f{n}.png
```

Retry policy: Gemini timeout/empty beats → rerun Gemini only · Jev
confidence < `jev_min_confidence` → surface to human, no auto-export ·
encoder change → invalidate perceive and everything downstream · brief
change → invalidate Jev stages only if metrics still exist.

## 11. Repo layout

Inside this workspace — a new crate, not a Python pipeline:

```text
crates/scene-meme/
  src/perceive.rs     # FrameStream metrics pass (+ embed connector call)
  src/hear.rs         # PcmStream metrics pass
  src/onset.rs        # flux curve → onset peaks (median+MAD pattern)
  src/peaks.rs        # fusion, snap, dedup, importance
  src/pack.rs         # fact-sheet JSON
  src/route.rs        # jev_route via capability
  src/analyze.rs      # gemini call + response parse
  src/package.rs      # jev_package + decision
  src/emit.rs         # emit_flash_scene → .scene draft
  src/cache.rs        # content-hash store
crates/scene-media/src/pcm.rs     # PcmStream (new)
connectors/embed.py               # MobileCLIP2 via uv (optional)
connectors/gemini.sh              # windows mode (Files API flow)
prompts/jev_route.json
prompts/gemini_analyze.md
prompts/jev_package.json
vocab/tags.txt
brief.yaml
```

CLI:

```bash
engine meme input.mp4 --brief brief.yaml --timings timings.json --out out/
```

Writes `out/metrics.json`, `out/audio.json`, `out/peaks.json`,
`out/keeps.json`, `out/gemini.json`, `out/package.json`,
`out/meme.scene`, `out/frames/f371.png`.

`--timings` is optional — without it `speech`/`word`/`beat_frame` are
absent and the pipeline is audio-onset-driven only.

## 12. Module plan (sequential gates — our discipline)

Each module lands with unit tests; the gate is
`fmt + clippy -D warnings + workspace tests` before the next starts.

| # | Module | Tests |
|---|---|---|
| M-A | `PcmStream` in scene-media | gated: decode a sine → RMS/peak sanity; ungated: chunked-read logic |
| M-B | `hear` + `onset` | pure f32 tests: synthetic flux curve → onsets; silence runs; hop math |
| M-C | `perceive` | pure: frame_diff/sharpness/brightness/contrast on synthetic pixels; gated: real clip decode |
| M-D | `peaks` | pure: fusion, snap-to-sharp, snap-to-onset, final-list dedup, min_gap=2 dense beats |
| M-E | `pack` + `route` | serde shape tests; fact-sheet contains no vectors; nearest_kept order-independence |
| M-F | frame export + `analyze` | PNG write; gated Gemini only when credentialed |
| M-G | `package` | decision enum parse; rerun_window names a window |
| M-H | `emit` + `engine meme` | emitted scene parses+lowers clean; keeps become `<clip>` beats; bed becomes `<music>` |
| M-I | `cache` | key stability; encoder change invalidates peaks; retry reuses perceive |
| M-J | docs | this doc + playbook + SKILL/USER_GUIDE entries |

## 13. Env / clients

```text
scene.toml:
  [capabilities.embed] command = ["uv", "run", "connectors/embed.py"]
  [capabilities.jev]   endpoint = "…", auth = { env = "TYPESAFE_API_KEY" }
  [capabilities.gemini] endpoint = "…", auth = { env = "GEMINI_API_KEY" }

JEV_MODEL / GEMINI_MODEL in env — pinned real IDs, not doc placeholders.
```

## 14. Acceptance tests

- Static shot, no cuts, no onsets → 0–1 keeps; package may be
  `need_more_peaks` or export "no montage."
- Hard cut every 200 ms → keeps land on cuts, not mid-dissolve —
  and land on the *sharpest* post-cut frame, not the boundary frame.
- Ten near-identical punch frames → one keep, rest skipped as
  duplicates (final-list dedup, not streaming order).
- Camera shake, no scene change → not kept.
- Loud transient with no visual cut (sfx hit) → audio candidate keeps.
- Cut landing mid-word vs on a word boundary → `cut_on_beat`/`word`
  fields populated when `--timings` exists.
- Tiny on-screen text for 3 frames → reaches Gemini (no 1 fps loss).
- Jev request body contains zero embedding arrays.
- Kill Gemini after 3 keeps, retry → perceive and peak pick replay
  from cache, nothing re-decodes.
- `--frames`-style partial runs aren't in scope — this pipeline always
  perceives the whole file.

## 15. Explicit non-goals

- Jev reading 256–512-d vectors
- Jev called once per frame at 30 Hz
- Gemini on all 30 fps frames
- Any stage pretending to see pixels it never saw
- Using Jev to caption, count, or do cosine math
- Reimplementing embeddings in Rust — ML inference is a connector,
  same as TTS/image-gen
- ONNX/`ort` in the binary — single-binary story is not worth the dep;
  `dHash` covers the no-encoder path

If a future TypeSafe model takes images, still keep perceive + peak
pick in code. Routing and export stay Jev. Seeing stays a vision model.
Hearing stays math.

## Consolidated implementation contract

The reference adapters have offline contract coverage, not live vendor acceptance.
See `connectors/README.md` for model settings, checkpoint loading, transport
contracts, and cost reporting. Offline dHash analysis remains the default.

Brief fields are strict: unknown fields and duplicate aliases fail.
`brightness_lo/hi` alias `brightness_min/max`. `onset_keep`, `snap_to_onset`, and
`snap_to_sharp` independently control onset candidates and snapping. Beat timing
and representative-frame timing are separate; stills use representative timing.
Dedup ranks the complete candidate set by importance, sharpness, change, then
ascending frame index; returned keeps are chronological. Cosine stays on its raw
−1…1 scale. Nearest-kept scores use the final selection.

Cache entries live under `.cache/v2`; older namespaces are ignored without deletion.
Keys include canonical stage inputs and upstream identity: source contents,
analysis grid, timings, vocabulary, effective model/checkpoint, connector command
and script content, prompt version, mode, and complete keep records where used.
Credentials are not stage inputs. Invalid embedding dimensions, vocabulary counts,
nonfinite values, or zero vectors cause a miss. New model settings never reuse a
prior embedding entry. Source and music imports use content-addressed filenames;
generated scene references are relative to their scene directory. Materialized
beats and offset-based beats use the same clamped source windows.

Generated files use private staging and atomic publication. A failed individual
publication preserves its previous destination. This is per-file atomicity, not a
transaction spanning every report and media file in a run.

Beat durations must cover at least one output frame. Smaller positive widths are
rejected after probing, before perception or provider execution, to prevent zero-frame exports.
