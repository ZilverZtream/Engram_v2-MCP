"""Phase G1: build dossiers + manifests for the 15 canonical PRs against the
FRESH G0 indexes (phase2_prep --reuse picks them up from index_map.json).
Sequential — each prep spawns its own eval server on the single-writer store.

Usage: python eval/_g1_run.py [--only 1933,1908]
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
P2 = os.path.join(HERE, "data", "p2")


def canonical_ids():
    v = json.load(open(os.path.join(P2, "_ab15_final_verdicts.json"), encoding="utf-8"))
    return sorted({str(x["pr"]) for x in v["verdicts"]})


def main():
    only = None
    if "--only" in sys.argv:
        only = set(sys.argv[sys.argv.index("--only") + 1].split(","))
    ids = canonical_ids()
    if only:
        ids = [i for i in ids if i in only]
    for n, pr in enumerate(ids, 1):
        print(f"[{n}/{len(ids)}] prep PR {pr}", flush=True)
        r = subprocess.run(
            [sys.executable, os.path.join(HERE, "phase2_prep.py"), "--pr", pr, "--reuse"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        tail = "\n".join((r.stdout or "").splitlines()[-4:])
        print(tail, flush=True)
        if r.returncode != 0:
            print(f"PREP FAILED PR {pr}:\n{(r.stderr or '')[-800:]}", flush=True)
            return 2
    print("G1 complete", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
