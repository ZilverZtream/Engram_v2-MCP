"""Validate the native Engram get_change_set tool against a kept eval index:
call it with a story, measure its file recall vs the PR's real modified files,
and show the change-set. Confirms the in-Engram tool reproduces the harness
recipe. Read-only. Run AFTER deploying the rebuilt binary.

Usage: python eval/validate_change_set.py [pr_id]   (default 1933)
"""
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram, canon, page_stem  # noqa: E402

P2 = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "p2")


def main():
    pr = int(sys.argv[1]) if len(sys.argv) > 1 else 1933
    imap = json.load(open(os.path.join(P2, "index_map.json"), encoding="utf-8"))
    pid = imap.get(str(pr))
    if not pid:
        print(f"no kept index for PR {pr}")
        return
    man = json.load(open(os.path.join(P2, f"pr{pr}.json"), encoding="utf-8"))
    story = man["story"]["title"]
    real = [canon(cf["path"]) for cf in man["ground_truth"]["changed_files"]
            if "add" not in (cf.get("change", "") or "").lower()
            and "rename" not in (cf.get("change", "") or "").lower()]
    real_pg = {page_stem(p) for p in real}

    e = Engram()
    try:
        out = e.tool("get_change_set", {"project_id": pid, "story": story})
    finally:
        e.close()
    if "TOOL_ERROR" in out:
        print("get_change_set ERROR:\n", out[:500])
        return

    files = [canon(x) for x in re.findall(r"^- `([^`]+)`", out, re.M)]
    fset = set(files)
    hit = fset & set(real)
    pg = {page_stem(f) for f in files} & real_pg
    print(f"PR {pr}: {story[:60]}")
    print(f"  change_set files: {len(files)}")
    print(f"  real modified: {len(real)} | exact hits: {len(hit)} "
          f"({len(hit)/len(real):.0%}) | page hits: {len(pg)}/{len(real_pg)}")
    print(f"  missed: {sorted(p.split('/')[-1] for p in (set(real)-fset))}")
    print("\n--- change_set head ---")
    print("\n".join(out.splitlines()[:40]))


if __name__ == "__main__":
    main()
