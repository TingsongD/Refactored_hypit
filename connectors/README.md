# Reference adapters

These adapters were tested with offline transports and model factories only.
No live provider behavior, paid requests, or model downloads were used for the
consolidation acceptance checks.

Configure capability commands with `python3 connectors/gemini.py` or
`python3 connectors/jev.py` (use `python` on Windows when appropriate). The `.sh`
files are optional POSIX wrappers. Declare authentication through the capability
registry's `auth.env`; the runtime supplies `SCENE_CAP_AUTH` to the subprocess.

Gemini uses `generateContent`, inline keep images/videos, explicit video sampling
metadata, and normalized usage counts. Set `gemini_model` in the brief or
`GEMINI_MODEL` in the environment. A configured Gemini capability without a model
fails before decoding or provider execution. `gemini_cap` selects custom capability
names. The brief wins over environment model settings.

Jev is a **reference JSON gateway contract**, not a verified vendor protocol.
Set `JEV_ENDPOINT` to your gateway. Routing sends per-candidate questions and
expects `answers`; packaging sends one question set and expects `package`.
Adapt `jev.py` to a vendor's documented wire contract before live use. No default
endpoint is assumed. Optional `jev_model` overrides `JEV_MODEL`.

Embedding uses `MobileCLIP2-S0` with explicit `dfndr2b` pretrained weights and
`require_pretrained=True`. `embedding_model`/`weights_id` override
`OPEN_CLIP_MODEL`/`OPEN_CLIP_PRETRAINED`. A missing checkpoint fails; random
initialization is not accepted. Image and vocabulary inference batches are capped
at 32. Running this adapter for real may download model weights through OpenCLIP.

The engine emits Gemini `preflight` progress before calling the adapter, then
actual usage after completion when the provider reports it. Input tokens are a
rough estimate; output tokens in preflight are the configured cap. Set both
`input_price_per_million` and `output_price_per_million` for estimated USD cost;
without prices monetary estimates are unavailable. These are not billing quotes.

Run offline checks: `python3 -m unittest discover -s connectors -p 'test_*.py'`.
