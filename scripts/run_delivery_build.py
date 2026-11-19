"""Reject small-feature compile cycles before launching the owned build runner."""
import argparse
import json
import os
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
    active_case = policy.get("active_release_case")
    queued = goal["next_source_batch_pending"] if active_case else policy["next_batch_pending"]
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
    if active_case:
        if case != Path(active_case).resolve():
            raise ValueError("Build blocked: requested case is outside the active source batch")
        plan = json.loads((case / "release-plan.json").read_text(encoding="utf-8"))
        worktree = Path(plan["source_worktree"])
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=worktree, text=True).strip()
        dirty = subprocess.check_output(["git", "status", "--porcelain"], cwd=worktree)
        if dirty or head != plan["source_commit"] or set(identifiers) != set(plan["original_requirement_ids"]):
            raise ValueError("Build blocked: the worktree or batch does not match the release plan")
        for item in queued:
            subprocess.run(["git", "merge-base", "--is-ancestor", item["source_commit"], head],
                           cwd=worktree, check=True, capture_output=True)
        if (case / "release1-owner.json").exists():
            raise ValueError("Build blocked: release1 was already started for this frozen batch")
    trailing = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
    command = [sys.executable, "-B", "-u", str(case / "build.py"), *trailing]
    if args.dry_run:
        print(json.dumps({"status": "BATCH_GATE_PASSED_NO_BUILD_STARTED", "command": command, "build_environment": {"CARGO_INCREMENTAL": "1"}}))
        return 0
    # Reuse code-generation work across source batches as well as dependency
    # artifacts. The owned runner keeps the existing shared target directory.
    return subprocess.run(command, cwd=ROOT, env=dict(os.environ, CARGO_INCREMENTAL="1")).returncode

if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, KeyError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(2)
