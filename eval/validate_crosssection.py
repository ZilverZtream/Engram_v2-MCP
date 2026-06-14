"""Validate the new cross-section dependency map on real OciusX: run
produce_claude_md (write_to_disk=false) on a kept OciusX index and show the
cross-section-map section. Confirms the standing "touch area A -> also need
area Y" scope signal is generated and useful. Runs on the production data_dir.
"""
import io
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram  # noqa: E402

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")


def main():
    imap = json.load(open(os.path.join(DATA, "p2", "index_map.json"), encoding="utf-8"))
    pid = os.environ.get("VALIDATE_PID", imap.get("1933"))
    eng = Engram(stderr_path=os.path.join(DATA, "validate_xsec.stderr.log"))
    try:
        h = eng.tool("project_health", {"project_id": pid})
        print("health:", h[:160].replace("\n", " "))
        out = eng.tool("produce_claude_md",
                       {"project_id": pid, "write_to_disk": True,
                        "merge_existing": False, "generate_agents_md": False}, _cap=20000)
        print("produce result:", out.strip()[:300])
        # rule files are written under <project_dir>/.claude/rules/
        wt = os.path.join(os.environ.get("TEMP", "/tmp"), "engram_eval_wt", "pr1933")
        rules_dir = os.path.join(wt, ".claude", "rules")
        target = os.path.join(rules_dir, "cross-section-map.md")
        if os.path.isdir(rules_dir):
            print("\nrule files written:", os.listdir(rules_dir))
        if os.path.exists(target):
            print("\n=== cross-section-map.md (OciusX) ===")
            print(open(target, encoding="utf-8").read()[:2500])
        else:
            print(f"\n!! {target} not found")
    finally:
        eng.close()


if __name__ == "__main__":
    main()
