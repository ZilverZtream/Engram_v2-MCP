"""Reject small-feature compile cycles before launching the owned build runner."""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRATCH = Path(r"C:\ai-projects\Engram-MCP_v2\target\diagnostics\snapshot-verification-20261101\p131-modern-state-20261112\current-task-check")

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", type=Path, required=True)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    goal = json.loads((ROOT / "eval/engram-delivery-goal-20261110.json").read_text(encoding="utf-8-sig"))
    policy = goal["delivery_cadence_correction_20261115"]
    queued = policy["next_batch_pending"]
    minimum = policy["minimum_independent_deliverables_before_next_compile"]
    identifiers = [item.get("original_requirement_id", "") for item in queued]
    if len(queued) < minimum or len(set(identifiers)) != len(queued):
        raise ValueError(f"Build blocked: need {minimum} independent source-complete original requirements; queue has {len(queued)}. Continue source work.")
    for item in queued:
        if not re.fullmatch(r"(?:EG-\d{2}|TODO-\d+[a-z]?)", item["original_requirement_id"]):
            raise ValueError("Build blocked: each item needs an original EG/TODO requirement identity")
        if item.get("status") != "SOURCE_COMPLETE_AWAITING_COMBINED_VALIDATION" or not item.get("acceptance_contract"):
            raise ValueError("Build blocked: incomplete source or missing finite acceptance contract")
        commit = item.get("source_commit", "")
        if not re.fullmatch(r"[0-9a-f]{40}", commit):
            raise ValueError("Build blocked: each source delivery needs its recorded commit")
        subprocess.run(["git", "cat-file", "-e", commit + "^{commit}"], cwd=ROOT, check=True, capture_output=True)
    case = args.case.resolve()
    if not case.is_relative_to(SCRATCH.resolve()) or not (case / "build.py").is_file():
        raise ValueError("Build blocked: use an existing owned runner in the recorded scratch directory")
    trailing = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
    command = [sys.executable, "-B", "-u", str(case / "build.py"), *trailing]
    if args.dry_run:
        print(json.dumps({"status": "BATCH_GATE_PASSED_NO_BUILD_STARTED", "command": command}))
        return 0
    return subprocess.run(command, cwd=ROOT).returncode

if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, KeyError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(2)
