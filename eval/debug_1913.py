"""Debug PR1913's api-v2 recall gap: re-index its base on a FRESH data_dir with
the current binary, then check whether the real target files (RoqEntriesController/
RoqEntryService/IRoqEntryService) are (a) in the change_set, (b) found by concept
footprint, (c) indexed at all. Distinguishes an EXTRACTION gap from a RANKING gap.
No agents (pure indexing + retrieval).
"""
import io
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram  # noqa: E402
import run_phase1 as rp  # noqa: E402

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")

TARGETS = ["roqentriescontroller", "roqentryservice", "iroqentryservice"]


def main():
    m = json.load(open(os.path.join(DATA, "p2", "pr1913.json"), encoding="utf-8"))
    rec = {"pr_id": 1913, "base_commit": m["base_commit"]}
    s = m["story"]
    story = s["title"]
    if s.get("description"):
        story += "\n\n" + s["description"]
    if s.get("acceptance"):
        story += "\n\nAcceptance:\n" + s["acceptance"]

    eng = Engram(stderr_path=os.path.join(DATA, "debug1913.stderr.log"))
    try:
        pid, wt, secs, health = rp.setup_index(eng, rec)
        print(f"indexed in {secs:.0f}s  pid={pid}", flush=True)

        cs = eng.tool("get_change_set", {"project_id": pid, "story": story})
        low = cs.lower()
        print("\n=== change_set: target files present? ===")
        for t in TARGETS:
            print(f"  {t:28} in change_set: {t in low}")

        cf = eng.tool("get_concept_footprint", {"project_id": pid, "concept": "RoqEntries"})
        cfl = cf.lower()
        print("\n=== concept_footprint('RoqEntries'): target files present? ===")
        for t in TARGETS:
            print(f"  {t:28} in footprint:  {t in cfl}")

        # Is the file indexed at ALL? Use a lexical/vector search for the symbol.
        for q in ["RoqEntriesController", "setAsBilled"]:
            r = eng.tool("ask_codebase", {"project_id": pid, "question": f"where is {q} defined"})
            rl = (r or "").lower()
            hit = any(t in rl for t in TARGETS) or "roqentries" in rl
            print(f"\n=== ask_codebase '{q}': mentions a target? {hit} ===")
            print("  ", (r or "")[:300].replace("\n", " "))

        # how many api-v2 vs redovisning files did the change_set capture?
        paths = re.findall(r"`([^`]+)`", cs)
        apiv2 = [p for p in paths if "api-v2" in p.lower()]
        redov = [p for p in paths if "redovisning" in p.lower()]
        print(f"\n=== change_set composition: api-v2={len(apiv2)}  redovisning={len(redov)}  total_backticked={len(paths)} ===")
        print("  api-v2 captured:", apiv2[:12])
    finally:
        eng.close()


if __name__ == "__main__":
    main()
