"""Reference generateContent adapter. Network access occurs only in main()."""
import base64
import json
import mimetypes
import os
from pathlib import Path
import re
import sys
import urllib.request


def build_body(params):
    parts = [{"text": params["prompt"]}]
    for keep in params["keeps"]:
        path = Path(keep["file"])
        parts.append({"text": json.dumps({k: v for k, v in keep.items() if k != "file"})})
        part = {"inlineData": {"mimeType": mimetypes.guess_type(path)[0] or "application/octet-stream",
                               "data": base64.b64encode(path.read_bytes()).decode("ascii")}}
        if params["mode"] == "windows":
            part["videoMetadata"] = {"fps": params["brief"]["gemini_fps"]}
        parts.append(part)
    return {"contents": [{"parts": parts}], "generationConfig": {
        "responseMimeType": "application/json", "maxOutputTokens": params["max_output_tokens"]}}


def normalize(response):
    text = "".join(p.get("text", "") for p in response["candidates"][0]["content"]["parts"])
    result = json.loads(text)
    if not isinstance(result, dict) or not isinstance(result.get("beats"), list):
        raise ValueError("Gemini response must contain a beats array")
    usage = response.get("usageMetadata")
    if usage is not None:
        result["usage"] = {"input_tokens": usage.get("promptTokenCount", 0),
                           "output_tokens": usage.get("candidatesTokenCount", 0)}
    return result


def main():
    req = json.load(sys.stdin)
    params = req["params"]
    model = params.get("model")
    if not model or not re.fullmatch(r"[A-Za-z0-9._-]+", model):
        raise ValueError("an explicit valid Gemini model is required")
    request = urllib.request.Request(
        f"https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent",
        data=json.dumps(build_body(params)).encode(),
        headers={"content-type": "application/json", "x-goog-api-key": os.environ["SCENE_CAP_AUTH"]})
    with urllib.request.urlopen(request, timeout=300) as response:
        result = normalize(json.load(response))
    Path(req["out"]).write_text(json.dumps(result), encoding="utf-8")


if __name__ == "__main__":
    main()
