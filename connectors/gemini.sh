#!/bin/sh
# Reference adapter; authentication is supplied in SCENE_CAP_AUTH.
set -eu
exec python3 "$(dirname "$0")/gemini.py"
