"""Fast re-validate PR1913 recall WITHOUT re-indexing: get_change_set is a query,
the base graph is unchanged, so just re-query the existing fresh index with the
current binary. Seconds, not the ~13min reindex. Reuses engram_dbg_data3.
"""
import io
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram, canon  # noqa: E402

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")
PID = os.environ.get("REQUERY_PID", "249f7f5f-8af7-4b12-8782-7e1c4d6f1fcb")
TARGETS = ["roqentriescontroller", "roqentryservice", "iroqentryservice"]


def main():
    m = json.load(open(os.path.join(DATA, "p2", "pr1913.json"), encoding="utf-8"))
    s = m["story"]
    story = s["title"]
    if s.get("description"):
        story += "\n\n" + s["description"]
    if s.get("acceptance"):
        story += "\n\nAcceptance:\n" + s["acceptance"]

    eng = Engram(stderr_path=os.path.join(DATA, "requery.stderr.log"))
    try:
        h = eng.tool("project_health", {"project_id": PID})
        if "TOOL_ERROR" in h or "graph_nodes" not in h:
            print("project not found / unhealthy:", h[:200]); return
        cs = eng.tool("get_change_set", {"project_id": PID, "story": story})
        if "TOOL_ERROR" in cs:
            print("get_change_set error:", cs[:200]); return
        low = cs.lower()
        print("=== change_set: target files present? ===")
        for t in TARGETS:
            print(f"  {t:28} {t in low}")
        # exact recall vs ground truth
        real = {canon(cf["path"]) for cf in m["ground_truth"]["changed_files"]
                if "add" not in (cf.get("change", "") or "").lower()}
        doss = {canon(x) for x in re.findall(r"`([^`]+)`", cs)}
        hit = real & doss
        print(f"\nPR1913 recall: {len(hit)}/{len(real)}")
        print("  HIT :", sorted(p.split('/')[-1] for p in hit))
        print("  MISS:", sorted(p.split('/')[-1] for p in (real - doss)))
    finally:
        eng.close()


if __name__ == "__main__":
    main()
