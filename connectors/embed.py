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

import numpy as np


def fail(msg: str) -> "NoReturn":
    print(f"embed connector: {msg}", file=sys.stderr)
    sys.exit(1)


def decode_frames(video: str, size: int, fps: float) -> np.ndarray:
    """ffmpeg → RGB bytes on the analysis grid, one row per frame."""
    cmd = [
        os.environ.get("FFMPEG", "ffmpeg"),
        "-v", "error",
        "-i", video,
        "-vf", f"scale={size}:{size},fps={fps}",
        "-f", "rawvideo",
        "-pix_fmt", "rgb24",
        "-",
    ]
    try:
        proc = subprocess.run(cmd, capture_output=True, check=True)
    except subprocess.CalledProcessError as e:
        fail(f"ffmpeg decode failed: {e.stderr.decode(errors='replace')[-400:]}")
    raw = proc.stdout
    frame_len = size * size * 3
    n = len(raw) // frame_len
    if n == 0:
        fail("ffmpeg produced no frames")
    return np.frombuffer(raw[: n * frame_len], np.uint8).reshape(n, size, size, 3)


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

    model_name = os.environ.get("OPEN_CLIP_MODEL", "MobileCLIP2-S0")
    model, _, preprocess = open_clip.create_model_and_transforms(model_name)
    tokenizer = open_clip.get_tokenizer(model_name)
    model.eval()

    frames = decode_frames(params["video"], int(params["size"]), float(params["fps"]))
    vocab = list(params.get("vocab", []))

    with torch.no_grad():
        # Batched image encode — one row per analysis frame, L2-normalized
        # so the engine's cosine is a plain dot product shape.
        imgs = torch.stack(
            [preprocess(Image.fromarray(f)) for f in frames]
        )
        emb = model.encode_image(imgs)
        emb = torch.nn.functional.normalize(emb, dim=-1)
        vocab_emb = torch.zeros(0, emb.shape[-1])
        if vocab:
            vocab_emb = torch.nn.functional.normalize(
                model.encode_text(tokenizer(vocab)), dim=-1
            )

    doc = {
        "embeddings": emb.float().tolist(),
        "vocab_embeddings": vocab_emb.float().tolist(),
    }
    with open(out, "w") as f:
        json.dump(doc, f)


if __name__ == "__main__":
    main()
