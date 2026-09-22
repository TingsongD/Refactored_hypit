# Consolidation acceptance record

Local feature history `3e2ba28` and upstream `dff979c` were preserved by merge
`2ff4d2b`. Recovery branch: `recovery/pre-consolidation-3e2ba28`. Working branch:
`fix/consolidate-review`. No history rewriting, release tag, visibility change,
paid request, or model download is part of this delivery.

## Finding traceability

| Finding | Resolution | Regression evidence |
|---|---|---|
| S1 analysis cache identity | Source hash, complete keeps, mode, effective brief, prompt version and connector identity | Mock provider warm-cache/invalidation test; endpoint-selector test |
| S2 embedding cache | Resolution, vocabulary, model/checkpoint and connector identity; validate dimensions, counts and finite/nonzero vectors | Shape/value validation, reused-directory vocabulary/resolution/model/checkpoint/grid invalidation, and offline factory tests |
| S3 package cache | Hash complete package request, effective brief and connector | Package request includes current keeps and analysis |
| S4 reused output source | Content-addressed staged source imports | CLI reruns changed footage into one directory and renders new pixels |
| S5 materialized paths | Scene-relative content-addressed beat directory | CLI materialized scene renders pixels and audio |
| S6 capability selection | Use configured capability for every Gemini call | `custom_connector_preflight_usage_and_cache_invalidation` uses `custom` |
| F1 connector syntax | Python adapter with small POSIX wrapper | Offline Python transport tests |
| F2 pretrained model | Explicit MobileCLIP2-S0/dfndr2b, require successful loading, 32-item batches | Mock OpenCLIP factory asserts arguments and propagates checkpoint failure |
| F3 representative still | Separate beat/representative timestamps; extract representative | `still_uses_representative_not_beat_timestamp` compares real extracted PNG bytes |
| F4 exclusive search | Search ends before the next minimum-gap boundary | Dense-cut regression |
| F5 cosine scale | Raw cosine threshold | 0.90 cosine remains distinct at 0.92 threshold |
| F6 global dedup | Rank all candidates by importance/sharpness/change/frame; chronological output | Stronger later duplicate wins |
| F7 audio annotations | Keep/request/prompt contain beat, onset and word information | Mock adapter checks metadata and sampling |
| F8 controls/reporting | Strict aliases, independent snapping, onset threshold, explicit models, immediate preflight, usage and optional prices | Config/control tests and callback-before-provider assertion |
| C1 CI setup | Explicit FFmpeg installation on Linux/macOS/Windows; native path assertion | Three-platform workflow gates |
| C2 huge word offsets | Saturating arithmetic before indexing; reject nonfinite anchor numbers | `word_offsets_saturate_before_indexing` |
| C3 silent-source music | Independent music track with content-addressed import | CLI real-media regression checks audio stream |
| C4 invalid beat widths | Early finite/positive representable-width check; consistent source windows | CLI rejects zero/NaN/infinity; span tests |
| I1 PCM deadlines | Guard active reads; managed process groups retain descendants through teardown | PCM idle/blocked-read and descendant-stderr deadline tests |
| D1 examples/docs | Add voice tracks; correct optional script semantics, crate count, cache/provider/media-test docs | Complete README, SKILL, guide, adapt and caption example checks |
| Deferred scheduler | Shared ascending jobs, ordered results, maximum 4×workers outstanding, persistent renderers and incremental state replay | Worker-count/window byte equality, sustained four-worker activity, bound, cancellation and panic tests |
| Deferred scaled windows | Seek with normalized timestamps, frame-grid half-open bounds and decode postroll | Sequential pixel equality including nonzero timestamp origin and fractional boundaries |
| Deferred chunked HTTP | Bounded headers, decoded body, trailers; strict framing and explicit 400/408/413 errors | Fragmented reads, extensions/trailers, conflicts, malformed/truncated/oversize and timeout tests |
| Proprietary notice | `COPYRIGHT`, no usage rights granted; third-party terms retained | Notice review; repository visibility unchanged |

## Additional review corrections

- Root and target confinement use the same canonical ancestor representation on
  Windows, including nonexistent project directories and native absolute paths.
- JSON float round trips preserve cache identity across fresh and cached runs.
- HTTP query selectors affect cache keys; conventional credential parameters and
  registry auth values do not. Put credentials in registry authentication fields.
- Single-window retries use video windows, retain unrelated beats, and publish the
  merged analysis back to its complete-keep-set cache. A targeted retry does not
  require a fresh full-list provider call first. A cold partial retry never fills the
  complete-analysis cache or reports export readiness.
- Streaming process owners retain their process group/Windows Job after parent
  exit. Unix exit observation uses `WNOWAIT` until group cleanup, preventing PID
  reuse races. Deadlines include stderr EOF; cancellation kills descendants before joining.
- JSON and generated media publish through private staging; an unsuccessful
  individual publication preserves the old destination. This is not a multi-file
  transaction.

## Verification scope

Local acceptance: formatting and strict all-target Clippy pass; hermetic workspace
tests pass; `SCENE_MEDIA_TESTS=1` workspace tests report 333 passed, 0 failed,
4 ignored subprocess fixtures. Four Python offline adapter tests pass. Release
build, an 18-frame video/audio CLI smoke render, and release large-offset test pass. Main publication additionally requires the working
branch's Linux/macOS/Windows workflow to pass; the delivery report links the exact
validated revision and subsequent main workflow.

The provider evidence is **offline only**. Jev's reference gateway contract is not
claimed to match a live vendor endpoint. Model weights were not downloaded.
OpenCLIP's [checkpoint registry](https://raw.githubusercontent.com/mlfoundations/open_clip/main/src/open_clip/pretrained.py)
and [factory](https://raw.githubusercontent.com/mlfoundations/open_clip/main/src/open_clip/factory.py)
identify the pretrained configuration; Gemini's
[video documentation](https://ai.google.dev/gemini-api/docs/video-understanding)
describes explicit sampling. Actual provider accuracy and billing remain unverified.
