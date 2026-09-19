#!/bin/sh
# Reference image connector — request JSON on stdin, PNG to request.out.
#
#   [capabilities.image]
#   command = ["sh", "connectors/openai-image.sh"]
#   auth = { env = "OPENAI_API_KEY" }
#
# params: {"prompt": "...", "size": "1024x1024"?}
set -eu
req=$(cat)
out=$(printf '%s' "$req" | python3 -c 'import json,sys; print(json.load(sys.stdin)["out"])')
prompt=$(printf '%s' "$req" | python3 -c 'import json,sys; print(json.load(sys.stdin)["params"]["prompt"])')
size=$(printf '%s' "$req" | python3 -c 'import json,sys; print(json.load(sys.stdin)["params"].get("size","1024x1024"))')
: "${SCENE_CAP_AUTH:?connector needs SCENE_CAP_AUTH (declare auth in scene.toml)}"
body=$(python3 -c 'import json,sys; print(json.dumps({"model":"gpt-image-1","prompt":sys.argv[1],"size":sys.argv[2]}))' "$prompt" "$size")
# The API key goes in a curl -K config, never argv — `ps` would expose a
# literal -H header to any local process inspector.
cfg=$(mktemp "${TMPDIR:-/tmp}/scene-cap-cfg.XXXXXX")
trap 'rm -f "$cfg"' EXIT
chmod 600 "$cfg"
printf 'header = "authorization: Bearer %s"\n' "$SCENE_CAP_AUTH" > "$cfg"
b64=$(curl -sfS -X POST \
  -K "$cfg" -H "content-type: application/json" \
  --data "$body" \
  "https://api.openai.com/v1/images/generations" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"][0]["b64_json"])')
printf '%s' "$b64" | python3 -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode(sys.stdin.read()))' > "$out"
