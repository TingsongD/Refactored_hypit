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
body=$(python3 -c 'import json,sys; print(json.dumps({"text":sys.argv[1],"model_id":sys.argv[2],"output_format":"pcm_48000"}))' "$text" "$model")
curl -sfS -X POST \
  -H "xi-api-key: $SCENE_CAP_AUTH" -H "content-type: application/json" \
  --data "$body" \
  "https://api.elevenlabs.io/v1/text-to-speech/$voice" \
  -o "$out.pcm"
# pcm_48000 = signed 16-bit little-endian mono → wav
ffmpeg -y -v error -f s16le -ar 48000 -ac 1 -i "$out.pcm" "$out"
rm -f "$out.pcm"
