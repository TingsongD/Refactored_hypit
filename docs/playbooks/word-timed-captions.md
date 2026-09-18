# Playbook: word-timed captions over footage

The canonical scene: a voice line drives when captions and cards appear.
Nothing in the markup stores seconds — the alignment step does.

## 1. Write the script into the scene

```xml
<scene canvas="1080x1920" fps="30" clear="#000">
  <script track="voice">
    <line id="hook">Everyone quotes the first half.</line>
    <line id="turn">The second half is the warning.</line>
  </script>
  <track id="voice" kind="audio">
    <sound src="assets/voice.wav" during="hook..turn"/>
  </track>
  <track kind="visual" anchor="voice">
    <clip src="assets/bg.mp4" during="hook..turn"/>
    <captions anchor="voice.words" during="hook..turn"/>
    <board during="turn-1w..turn" at="top" anim="rise">
      <text bind="turn.text"/>
    </board>
  </track>
  <render target="out/captioned.mp4"/>
</scene>
```

Note `turn-1w` — the card rises one *word* before the turn lands, not a
guessed number of frames.

## 2. Align the voice

`timings.json` maps each track to per-word timing. The markers
connector takes `cue start_s end_s` lines:

```bash
cat > markers.txt <<'EOF'
hook 0.00 1.85
turn 1.85 4.10
EOF
engine align main.scene --markers markers.txt --out timings.json
```

A WhisperX-style JSON works too (`--whisperx out.json`); word timings
are order-matched to the script's lines.

## 3. Render

```bash
engine render main.scene --timings timings.json
```

Re-cut the voice or rewrite a line and re-align — the composition
re-flows. No element ever needed a frame number.

## What to check

- `engine check` flags unknown cue ids before you render.
- If captions look empty, the `anchor` on the track and the
  `anchor="voice.words"` on captions must point at the audio track that
  the timing source names.
- Word offsets that walk off the ends clamp — `hook-10w` at the very
  start is just `hook`'s start.
