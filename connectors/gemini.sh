#!/bin/sh
# Reference Gemini analysis connector — request JSON on stdin, normalized
# analysis JSON to request.out.
#
#   [capabilities.gemini]
#   command = ["sh", "connectors/gemini.sh"]
#   auth = { env = "GEMINI_API_KEY" }
#
# params in:  {"task":"gemini_analyze","mode":"stills|windows",
#              "brief":{...},"model_env":"GEMINI_MODEL",
#              "keeps":[{"id","t","frame","file"},...],"prompt":"..."}
# writes out: {"verdict":"...","joke":"...","text_on_screen":[...],
#              "beats":[{...}],"drop":[...],"missing":"..."|null}
#
# The script builds a generateContent body with one inline_data part per
# keep file (stills) — windows mode uses the Files API (upload → file_uri
# part), a second round-trip. Adapt to your preferred Gemini surface.
set -eu
req=$(cat)
eval "$(printf '%s' "$req" | python3 -c '
import json, sys, shlex
req = json.load(sys.stdin)
print("out=" + shlex.quote(req["out"]))
print("mode=" + shlex.quote(req["params"]["mode"]))
print("prompt=" + shlex.quote(req["params"]["prompt"]))
')"
: "${SCENE_CAP_AUTH:?connector needs SCENE_CAP_AUTH (declare auth in scene.toml)}"
: "${GEMINI_MODEL:=gemini-3.1-flash-lite-preview}"
endpoint="https://generativelanguage.googleapis.com/v1beta/models/${GEMINI_MODEL}:generateContent"

# Assemble parts: prompt text + one inline_data part per keep file.
body=$(printf '%s' "$req" | python3 -c '
import base64, json, mimetypes, sys
req = json.load(sys.stdin)
p = req["params"]
parts = [{"text": p["prompt"]}]
for k in p["keeps"]:
    path = k["file"]
    data = base64.b64encode(open(path, "rb").read()).decode()
    mime = mimetypes.guess_type(path)[0] or "application/octet-stream"
    # A label text part before each image keeps ids unambiguous.
    parts.append({"text": f"keep id={k[\"id\"]} t={k[\"t\"]:.2f} frame={k[\"frame\"]}"})
    parts.append({"inline_data": {"mime_type": mime, "data": data}})
print(json.dumps({
    "contents": [{"parts": parts}],
    "generationConfig": {"responseMimeType": "application/json"},
}))')

# API key travels as a query param for Gemini — pass it via -K url so it
# never appears in argv.
cfg=$(mktemp "${TMPDIR:-/tmp}/scene-cap-cfg.XXXXXX")
trap 'rm -f "$cfg"' EXIT
chmod 600 "$cfg"
printf 'url = "%s?key=%s"\n' "$endpoint" "$SCENE_CAP_AUTH" > "$cfg"

resp=$(mktemp "${TMPDIR:-/tmp}/scene-cap-resp.XXXXXX")
trap 'rm -f "$cfg" "$resp"' EXIT
curl -sfS -X POST \
  -K "$cfg" -H "content-type: application/json" \
  --max-time 300 \
  --data "$body" \
  -o "$resp"

# Extract the model's JSON text and write it verbatim to out.
python3 -c '
import json, sys
resp = json.load(open(sys.argv[1]))
try:
    text = resp["candidates"][0]["content"]["parts"][0]["text"]
except (KeyError, IndexError) as e:
    sys.stderr.write(f"gemini: unexpected response shape: {e}\n")
    sys.stderr.write(json.dumps(resp)[:2000] + "\n")
    sys.exit(1)
open(sys.argv[2], "w").write(text)
' "$resp" "$out"
