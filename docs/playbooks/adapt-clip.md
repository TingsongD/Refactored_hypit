# Playbook: adapt an existing clip into a draft scene

`engine adapt` turns footage you already have — or a URL — into a
draft `.scene`: probed canvas size, hard cuts detected, one labeled
board per shot. It's a skeleton to edit, not a finished video.

## 1. Point it at media

```bash
engine adapt assets/lecture.mp4 --out main.scene
# or fetch first:
engine adapt "https://example.com/talk" --out-dir assets --out main.scene
```

URLs route through `yt-dlp` (needs it installed — `engine doctor`
checks). Local paths pass straight through; the `src` written into the
scene is relative to `--out-dir` when the file lives inside it.

## 2. Read what it found

```
adapt: assets/lecture.mp4 — 94.20s 1920x1080, 6 cut(s), has audio
```

Each cut is a detected hard shot boundary (robust frame-diff —
median + 6·MAD spike threshold, so noisy footage doesn't phantom-cut).
The emitted draft:

```xml
<scene canvas="1920x1080" fps="30" clear="#000">
  <track kind="visual">
    <clip src="assets/lecture.mp4" during="0s..94.200s"/>
    <board during="0.000s..12.500s" at="bottom" anim="rise">
      <text>Shot 1</text>
    </board>
    <board during="12.500s..31.250s" at="bottom" anim="rise">
      <text>Shot 2</text>
    </board>
    …
  </track>
  <track kind="audio">
    <music src="assets/lecture.mp4" during="0s..94.200s"/>
  </track>
  <render target="out/adapted.mp4"/>
</scene>
```

## 3. Make it yours

The `Shot N` placeholders are where your content goes:

- Replace a `<text>Shot 2</text>` with `<text bind="hook.text"/>` once
  you've written a `<script>` (with `<line id="hook">…`) and switched
  the literal `during`s to cue anchors. `.text` is the only bindable
  property today.
- Soft cuts and dissolves don't register — only hard boundaries.
  Add boards by hand where the content actually turns.
- The music element keeps the source's own audio; effectively mute it
  with `gain="-60dB"` (or delete the audio track) if you're laying new
  voice. Gains must be finite — `inf`/`nan` are rejected.

## What to check

- `engine check main.scene` — the emitter always produces valid markup;
  if check fails, it's something *you* edited.
- Single-shot footage emits **one** board covering the whole duration —
  the emitter always writes at least a `Shot 1` placeholder.
- `adapt` never stores word timing; literal `Ns..Ms` anchors are what
  the draft uses. Bring in `<script>` + `align` when you want symbolic
  timing.
- Footage outside the draft's directory is imported into its `assets/`
  first (`--out nested/draft.scene` gets its own copy) — the emitted
  `src` always stays inside the scene's project root, which is what
  render's source confinement requires.
