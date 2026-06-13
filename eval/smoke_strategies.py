"""Fast smoke test of all 11 strategies against the LIVE OciusX index (current
code — no re-index needed). Goal: catch arg/schema errors and confirm each
strategy returns paths without crashing, BEFORE the expensive pilot run.
Recall is meaningless here (live index is post-PR); we only check execution.
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram  # noqa: E402
import run_phase1 as rp  # noqa: E402

LIVE = "664003e4-2ac5-4902-a0ce-6382b6026fe5"
REC = {
    "pr_id": 1906, "author": "x",
    "story": {"title": "As an admin I want changes to markers to be logged",
              "description": "", "acceptance": ""},
}


def main():
    e = Engram()
    try:
        for name, fn in rp.STRATEGIES:
            t0 = time.time()
            try:
                pred = fn(e, LIVE, REC)
                dt = time.time() - t0
                samp = sorted(pred)[:3]
                print(f"{name:22s} OK  paths={len(pred):3d}  {dt:5.1f}s  e.g. {samp}")
            except Exception as ex:
                print(f"{name:22s} ERROR: {repr(ex)[:200]}")
    finally:
        e.close()


if __name__ == "__main__":
    main()
