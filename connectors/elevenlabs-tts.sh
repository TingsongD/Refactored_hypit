#!/bin/sh
# Reference TTS connector — request JSON on stdin, asset to request.out.
# Auth arrives as SCENE_CAP_AUTH (declare `auth = { env = "..." }`).
#
#   [capabilities.tts]
#   command = ["sh", "connectors/elevenlabs-tts.sh"]
#   auth = { env = "ELEVENLABS_API_KEY" }
#
# params: {"text": "...", "voice_id": "...", "model_id": "eleven_v3"?}
set -eu
req=$(cat)
out=$(printf '%s' "$req" | python3 -c 'import json,sys; print(json.load(sys.stdin)["out"])')
text=$(printf '%s' "$req" | python3 -c 'import json,sys; print(json.load(sys.stdin)["params"]["text"])')
voice=$(printf '%s' "$req" | python3 -c 'import json,sys; print(json.load(sys.stdin)["params"].get("voice_id","21m00Tcm4TlvDq8ikWAM"))')
model=$(printf '%s' "$req" | python3 -c 'import json,sys; print(json.load(sys.stdin)["params"].get("model_id","eleven_multilingual_v2"))')
: "${SCENE_CAP_AUTH:?connector needs SCENE_CAP_AUTH (declare auth in scene.toml)}"
body=$(python3 -c 'import json,sys; print(json.dumps({"text":sys.argv[1],"model_id":sys.argv[2]}))' "$text" "$model")
# The API key goes in a curl -K config, never argv — `ps` would expose a
# literal -H header to any local process inspector.
cfg=$(mktemp "${TMPDIR:-/tmp}/scene-cap-cfg.XXXXXX")
trap 'rm -f "$cfg" "$out.pcm"' EXIT
chmod 600 "$cfg"
printf 'header = "xi-api-key: %s"\n' "$SCENE_CAP_AUTH" > "$cfg"
# output_format is a *query* parameter — inside the JSON body it is
# ignored and the API returns mp3, which the raw-PCM decode below
# would turn into noise.
curl -sfS -X POST \
  -K "$cfg" -H "content-type: application/json" \
  --data "$body" \
  "https://api.elevenlabs.io/v1/text-to-speech/$voice?output_format=pcm_48000" \
  -o "$out.pcm"
# pcm_48000 = signed 16-bit little-endian mono → wav
ffmpeg -y -v error -f s16le -ar 48000 -ac 1 -i "$out.pcm" "$out"
