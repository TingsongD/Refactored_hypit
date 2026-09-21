"""Reference JSON gateway contract, not a claim of vendor API compatibility.
Configure JEV_ENDPOINT to a gateway accepting this contract; adapt vendor mapping
here. Routing has per-candidate questions; packaging has one global question set.
"""
import json
import os
from pathlib import Path
import sys
import urllib.request


def build_body(params):
    task = params["task"]
    if task == "jev_route":
        questions = {c["id"]: params["questions"] for c in params["state"]["candidates"]}
    elif task == "jev_package":
        questions = params["questions"]
    else:
        raise ValueError("unsupported Jev task")
    return {"system": task, "model": params.get("model"), "brief": params["brief"],
            "state": params["state"], "questions": questions}


def normalize(task, response):
    if task == "jev_route":
        answers = response["answers"]
        if not isinstance(answers, dict):
            raise ValueError("routing answers must be an object")
        for answer in answers.values():
            if (answer["route"] not in ("keep", "skip", "maybe")
                or not 1 <= answer["cut_strength"] <= 5
                or not 0 <= answer["too_similar"] <= 1
                or not 0 <= answer.get("confidence", 1) <= 1):
                raise ValueError("invalid routing answer")
    elif task == "jev_package":
        if response["package"] not in ("export", "need_more_peaks", "rerun_window"):
            raise ValueError("invalid package decision")
        if not 0 <= response.get("confidence", 1) <= 1 or not 0 <= response.get("postable", 0) <= 5:
            raise ValueError("invalid package scores")
        if response["package"] == "rerun_window" and not response.get("keep_id"):
            raise ValueError("rerun requires keep_id")
    else:
        raise ValueError("unsupported Jev task")
    return response


def main():
    req = json.load(sys.stdin)
    params = req["params"]
    request = urllib.request.Request(os.environ["JEV_ENDPOINT"],
        data=json.dumps(build_body(params)).encode(), headers={"content-type": "application/json",
        "authorization": "Bearer " + os.environ["SCENE_CAP_AUTH"]})
    with urllib.request.urlopen(request, timeout=120) as response:
        result = normalize(params["task"], json.load(response))
    Path(req["out"]).write_text(json.dumps(result), encoding="utf-8")


if __name__ == "__main__":
    main()
