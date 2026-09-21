#!/bin/sh
# Reference Jev routing connector — request JSON on stdin, normalized
# answers JSON to request.out.
#
#   [capabilities.jev]
#   command = ["sh", "connectors/jev.sh"]
#   auth = { env = "TYPESAFE_API_KEY" }
#
# params in:  {"task":"jev_route","brief":{...},"state":{"candidates":[rows]},
#              "questions":{...}}
# writes out: {"answers":{"f371":{"route":"keep","cut_strength":4,
#              "too_similar":0.2,"confidence":0.9}, ...}}
#
# ADAPT ME: the Jev endpoint shape below is a plausible mapping of the
# TypeSafe /v1/systemone call — adjust `body` and the response jq/python
# projection to the real wire format before relying on it.
set -eu
req=$(cat)
out=$(printf '%s' "$req" | python3 -c 'import json,sys; print(json.load(sys.stdin)["out"])')
: "${SCENE_CAP_AUTH:?connector needs SCENE_CAP_AUTH (declare auth in scene.toml)}"
: "${JEV_ENDPOINT:=http://localhost:8787/v1/systemone}"

# Translate our self-describing doc into the service call: state rows +
# one question set per candidate id.
body=$(printf '%s' "$req" | python3 -c '
import json, sys
req = json.load(sys.stdin)
params = req["params"]
ids = [c["id"] for c in params["state"].get("candidates", [])]
print(json.dumps({
    "system": params["task"],
    "brief": params["brief"],
    "state": params["state"],
    "questions": {i: params["questions"] for i in ids},
}))')

# The API key rides in a curl -K config — argv is visible in `ps`.
cfg=$(mktemp "${TMPDIR:-/tmp}/scene-cap-cfg.XXXXXX")
trap 'rm -f "$cfg"' EXIT
chmod 600 "$cfg"
printf 'header = "authorization: Bearer %s"\n' "$SCENE_CAP_AUTH" > "$cfg"

curl -sfS -X POST \
  -K "$cfg" -H "content-type: application/json" \
  --max-time 120 \
  --data "$body" \
  "$JEV_ENDPOINT" \
  | python3 -c '
import json, sys
resp = json.load(sys.stdin)
# Normalize whatever the service returns into {"answers": {id: {...}}}.
# Adapt this projection to the real response shape.
answers = resp.get("answers", resp.get("questions", resp))
print(json.dumps({"answers": answers}))
' > "$out"
