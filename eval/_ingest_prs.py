"""One-off: backfill the merged-PR exemplar corpus (pr: docs) into eval
snapshot projects that were freshly indexed BEFORE setup_index gained the
ingest_merged_prs step (2026-07-10). Leak-freedom is query-time
(get_change_set merged_before), so ingesting the full origin history is safe.

Usage: python eval/_ingest_prs.py <project_id> [<project_id> ...]
Requires the shared daemon STOPPED (eval client takes the data_dir lock).
"""
import sys

sys.path.insert(0, "eval")
from engram_client import Engram  # noqa: E402

eng = Engram()
try:
    for pid in sys.argv[1:]:
        print(f"=== {pid} ===")
        out = eng.tool("ingest_merged_prs", {"project_id": pid, "max_commits": 500})
        print(out[:600])
finally:
    eng.close()
