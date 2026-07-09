"""One-off: incremental update_project on eval snapshot projects (e.g. to
pick up files newly covered by an extension-preset change without a full
reindex). Requires the shared daemon STOPPED.

Usage: python eval/_update_eval.py <project_id> [<project_id> ...]
"""
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.path.insert(0, "eval")
from engram_client import Engram  # noqa: E402

eng = Engram()
try:
    for pid in sys.argv[1:]:
        print(f"=== {pid} ===")
        out = eng.tool("update_project", {"project_id": pid, "wait": True})
        print(out[:800])
finally:
    eng.close()
