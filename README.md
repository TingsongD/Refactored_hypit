# Scene Engine

Agent-authored video, timed by words. Write a `.scene` document — script,
tracks, anchored elements — and the `engine` binary compiles it into an
mp4. Rewrite a line and the composition re-times itself.

Status: **all 11 modules complete** — markup → IR → timing → layout →
raster → audio → mux, plus alignment, capabilities, sandboxed `<program>`
scripts, and `adapt` ingestion. See `DESIGN.md` for architecture and the
per-module gates; `SKILL.md` for the authoring reference;
`docs/playbooks/` for worked examples.

## Install

```bash
cargo build --release -p engine
# binary at target/release/engine — self-contained except external tools:
# ffmpeg + ffprobe (required for render), yt-dlp (adapt from URLs only)
./target/release/engine doctor    # verify external tools
```

Tagged releases build binaries for macOS-arm64, Linux-x86_64 and
Windows-x86_64 (`.github/workflows/release.yml`).

## Quickstart

```bash
engine init myvideo               # scaffold scene.toml + main.scene + assets/
engine check myvideo/main.scene   # validate; every error has a source span
engine adapt clip.mp4 --out draft.scene   # draft a scene from footage
engine align main.scene --markers m.txt --out timings.json
engine render main.scene --timings timings.json
```

## The idea

Elements never store seconds. They store **symbolic anchors** into the
script — `during="hook..payoff"`, `during="beat+2w"`, `bind="payoff.text"` —
which a realize pass resolves into frame positions once the voice track is
rendered and word-aligned. Edit the words and the whole composition re-flows.
Literal anchors (`during="1.5s..4s"`, `during="0f..90f"`) are the escape hatch.

```xml
<scene canvas="1080x1920" fps="30" clear="#0e0e12">
  <script track="voice">
    <line id="hook">Nobody talks about the third rule.</line>
    <line id="payoff">Compound interest is a treadmill.</line>
  </script>
  <track kind="visual" anchor="voice">
    <captions anchor="voice.words"/>
    <board during="payoff" at="center" anim="rise">
      <text bind="payoff.text"/>
    </board>
    <program src="assets/progress.js" with='{"seconds": 6}'/>
  </track>
  <track kind="audio">
    <music src="assets/bed.mp3" gain="-14dB" duck="voice" during="hook..payoff"/>
  </track>
  <render target="out/final.mp4"/>
</scene>
```

## Layout

```
crates/
  scene-ir       canonical types, spans, diagnostics, anchor grammar
  scene-markup   .scene parser (syntax only) + lowering (vocabulary + validation)
  scene-time     anchors + word alignment → frame/sample domains
  scene-media    ffprobe/ffmpeg subprocesses: probe, decode, encode
  scene-layout   resolved elements → positioned boxes per frame
  scene-render   tiny-skia raster + cosmic-text + deterministic worker pool
  scene-audio    48 kHz clip graph → ffmpeg filtergraph mix (ducking)
  scene-align    markers-file / WhisperX → TimingMap
  scene-cap      capability registry: credentials + subprocess/HTTP connectors
  scene-script   QuickJS sandbox → JSON DrawList (no fs/process, capped)
  scene-adapt    ingest → probe → shot-detect → draft .scene
  engine         CLI binary: init check parse align adapt render cap doctor
connectors/      reference capability connectors (TTS, image)
docs/playbooks/  worked examples
```

Error style is rustc-like: every diagnostic carries a byte span into the
source and the CLI renders it with the offending text.

## Checks

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                 # unit tests, hermetic
SCENE_MEDIA_TESTS=1 cargo test --workspace   # + real-ffmpeg integration
```

CI runs the hermetic set on Ubuntu, Windows and macOS.
