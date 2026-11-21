"""Run the public, pinned story replays with one or two Engram binaries.

python -B eval/run_story_workflow_suite.py --candidate SERVER --baseline OLD_SERVER --output NEW_DIR

Requires Git, Python 3.12+, and read access to GitHub. Only original issue text
and first-parent source are passed to Engram. The local-repository entry point
story_workflow_replay.py also works offline. The public cases are regression
examples after publication. Run fresh held-out issues before claiming a gain.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
from urllib.request import Request, urlopen

import story_workflow_replay as replay


def public_case(entry, workspace):
    repository = entry["repository"]
    if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository) or not re.fullmatch(r"[a-f0-9]{40}", entry["fix"]):
        raise ValueError("Expected a GitHub repository and a full commit SHA")
    issue = int(entry["issue"])
    request = Request(f"https://api.github.com/repos/{repository}/issues/{issue}",
                      headers={"User-Agent": "Engram-story-workflow-replay"})
    with urlopen(request, timeout=30) as response:
        data = json.load(response)
    if "pull_request" in data:
        raise ValueError("Expected a user issue")
    text = (data["title"].strip() + "\n\n" + (data["body"] or "").strip()).replace("\r\n", "\n")
    if hashlib.sha256(text.encode()).hexdigest() != entry["story_sha256"]:
        raise ValueError(f"The original issue changed: {repository}#{issue}")
    story = workspace / "issue.txt"
    story.write_bytes(text.encode())
    repo = workspace / "history.git"
    subprocess.run(["git", "init", "--quiet", "--bare", str(repo)], check=True)
    subprocess.run(["git", "-C", str(repo), "fetch", "--quiet", "--no-tags", "--depth=2",
                    f"https://github.com/{repository}.git", entry["fix"]], check=True)
    return repo, story


def run(args):
    args.output.mkdir(parents=True, exist_ok=False)
    summary = []
    for entry in json.loads(args.cases.read_text(encoding="utf8")):
        with tempfile.TemporaryDirectory(dir=args.output) as workspace:
            repo, story = public_case(entry, Path(workspace))
            for label in ("baseline", "candidate"):
                binary = getattr(args, label)
                if binary is None:
                    continue
                case = entry["repository"].replace("/", "-") + "-" + str(entry["issue"])
                output = args.output / f"{case}-{label}.json"
                replay.run(argparse.Namespace(repo=repo, fix=entry["fix"], binary=binary,
                                              story_file=story, scope=entry["scope"], output=output))
                receipt = json.loads(output.read_text(encoding="utf8"))
                score = receipt["score"]
                observations = receipt["observations"]
                row = dict(case=case, version=label, ranked_file=bool(score["ranked_file_hits"]),
                           ranked_method=bool(score["ranked_method_hits"]),
                           ranked_callee_file=bool(score["ranked_callee_file_hits"]),
                           named_route_file=bool(score["named_route_file_hits"]),
                           search_file=bool(score["search_file_hits"]),
                           sites=len(observations["sites"]),
                           tool_errors=sum("error" in call for call in observations["calls"]))
                summary.append(row)
                print(json.dumps(row), flush=True)
                (args.output / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=Path, default=Path(__file__).parent / "story_workflow_cases.json")
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args())
