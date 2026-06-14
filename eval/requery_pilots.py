"""Aggregate recall re-measure across the kept pilot indexes WITHOUT re-indexing:
get_change_set is a query and the base graphs are unchanged, so re-querying the
kept indexes with the current binary measures the change-set LOGIC fixes (concept
path-priority, plural stems, yaml). Reads eval/data/p2/index_map.json for pids.
Runs on the PRODUCTION data_dir (where the kept indexes live).
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
P2 = os.path.join(DATA, "p2")
PREPPED = [1937, 1965, 1967, 1908, 1933, 1913, 1974]


def story_of(m):
    s = m["story"]
    t = s["title"]
    if s.get("description"):
        t += "\n\n" + s["description"]
    if s.get("acceptance"):
        t += "\n\nAcceptance:\n" + s["acceptance"]
    return t


def real_of(m):
    return {canon(cf["path"]) for cf in m["ground_truth"]["changed_files"]
            if "add" not in (cf.get("change", "") or "").lower()}


def main():
    imap = json.load(open(os.path.join(P2, "index_map.json"), encoding="utf-8"))
    eng = Engram(stderr_path=os.path.join(DATA, "requery_pilots.stderr.log"))
    tot_hit = tot_real = 0
    rows = []
    try:
        for pr in PREPPED:
            pid = imap.get(str(pr))
            mp = os.path.join(P2, f"pr{pr}.json")
            if not pid or not os.path.exists(mp):
                rows.append((pr, "no index/manifest", 0, 0))
                continue
            m = json.load(open(mp, encoding="utf-8"))
            h = eng.tool("project_health", {"project_id": pid})
            if "graph_nodes" not in h:
                rows.append((pr, "unhealthy", 0, 0))
                continue
            cs = eng.tool("get_change_set", {"project_id": pid, "story": story_of(m)})
            if "TOOL_ERROR" in cs:
                rows.append((pr, "cs error", 0, 0))
                continue
            real = real_of(m)
            doss = {canon(x) for x in re.findall(r"`([^`]+)`", cs)}
            hit = real & doss
            tot_hit += len(hit)
            tot_real += len(real)
            miss = sorted(p.split("/")[-1] for p in (real - doss))
            rows.append((pr, f"{len(hit)}/{len(real)}", len(hit), len(real), miss))
        print(f"{'PR':6}{'recall':>9}   missed")
        for r in rows:
            pr = r[0]
            rec = r[1]
            miss = r[4] if len(r) > 4 else ""
            print(f"{pr:<6}{rec:>9}   {', '.join(miss)[:70] if miss else ''}")
        if tot_real:
            print(f"\nAGGREGATE exact-recall: {tot_hit}/{tot_real} = {tot_hit/tot_real:.3f}")
    finally:
        eng.close()


if __name__ == "__main__":
    main()
