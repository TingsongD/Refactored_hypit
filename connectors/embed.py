#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["open-clip-torch", "torch", "pillow", "numpy"]
# ///
"""Frame-embedding connector for `engine meme` — request JSON on stdin,
embeddings JSON written to request.out.

    [capabilities.embed]
    command = ["connectors/embed.py"]
    # no auth — the model runs locally under uv

params: {"video": "path", "size": 224, "fps": 30, "vocab": [...]}
writes: {"embeddings": [[f32] * D] * N_frames,
         "vocab_embeddings": [[f32] * D] * N_vocab}

The model is MobileCLIP2-S0 via open_clip (OPEN_CLIP_MODEL env to
override). First run downloads weights — expect a few hundred MB.
Embeddings stay local: they feed cosine math in the engine, and never
enter a Jev request.
"""

import json
import os
import subprocess
import sys



def fail(msg: str) -> "NoReturn":
    print(f"embed connector: {msg}", file=sys.stderr)
    sys.exit(1)


def decode_batches(video, size, fps, batch_size=32):
    """Stream bounded RGB batches; stderr inherits the bounded connector capture."""
    import numpy as np
    cmd = [os.environ.get("FFMPEG", "ffmpeg"), "-v", "error", "-i", video,
           "-vf", f"scale={size}:{size},fps={fps}", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE)
    frame_len = size * size * 3
    try:
        batch = []
        while True:
            raw = proc.stdout.read(frame_len)
            if not raw:
                break
            if len(raw) != frame_len:
                raise ValueError("truncated RGB frame")
            batch.append(np.frombuffer(raw, np.uint8).reshape(size, size, 3))
            if len(batch) == batch_size:
                yield batch
                batch = []
        if batch:
            yield batch
        if proc.wait() != 0:
            raise ValueError("ffmpeg decode failed")
    finally:
        proc.stdout.close()
        if proc.poll() is None:
            proc.kill()
        proc.wait()


def load_model(open_clip, params):
    model_name = params["model"]
    model, _, preprocess = open_clip.create_model_and_transforms(
        model_name, pretrained=params["weights_id"], require_pretrained=True)
    model.eval()
    return model, preprocess, open_clip.get_tokenizer(model_name)


def main() -> None:
    req = json.load(sys.stdin)
    params = req["params"]
    out = req["out"]

    try:
        import open_clip
        import torch
        from PIL import Image
    except ImportError as e:
        fail(f"missing dependency {e.name} — run via `uv run` so the inline deps resolve")

    model, preprocess, tokenizer = load_model(open_clip, params)
    vocab = list(params.get("vocab", []))
    rows = []
    with torch.no_grad():
        for batch in decode_batches(params["video"], int(params["size"]), float(params["fps"])):
            imgs = torch.stack([preprocess(Image.fromarray(f)) for f in batch])
            emb = torch.nn.functional.normalize(model.encode_image(imgs), dim=-1)
            rows.extend(emb.float().tolist())
        if not rows:
            fail("ffmpeg produced no frames")
        vocab_rows = []
        for start in range(0, len(vocab), 32):
            encoded = model.encode_text(tokenizer(vocab[start:start + 32]))
            vocab_rows.extend(torch.nn.functional.normalize(encoded, dim=-1).float().tolist())
    doc = {"embeddings": rows, "vocab_embeddings": vocab_rows}
    with open(out, "w") as f:
        json.dump(doc, f)


if __name__ == "__main__":
    main()
