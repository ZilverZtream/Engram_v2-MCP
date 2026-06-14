"""Pre-fetch precedent SCOPE per pilot: call find_similar_changes with the
dossier's predicted files to get the most similar past PRs, their actual
file-set size/layers, and the companion analysis ("what similar changes touch
that your plan doesn't"). This is the scope-calibration signal for the A/B —
it tells the implementer how BIG/which-layers a change like this really is,
directly countering the over-scope failure (1974 pipeline, 1913 wrong-layer).

Writes eval/data/p2/pr{pr}_ctx_scope.md. Runs on the production data_dir (kept
indexes via index_map.json). Leakage-safe: the index is at base, so the target
PR is not in history; find_similar_changes returns OTHER (past) PRs only.
"""
import io
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram  # noqa: E402

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")
P2 = os.path.join(DATA, "p2")
PILOTS = [1908, 1933, 1965, 1974, 1913]


def predicted_files(pr):
    """Top predicted files from the dossier prov (prefer co-change/history)."""
    try:
        prov = json.load(open(os.path.join(P2, f"pr{pr}_prov.json"), encoding="utf-8"))
    except Exception:
        return []
    golden = [p for p, sig in prov.items() if set(sig) & {"history", "cochange"}]
    rest = [p for p in prov if p not in golden]
    return (golden + rest)[:10]


def main():
    imap = json.load(open(os.path.join(P2, "index_map.json"), encoding="utf-8"))
    eng = Engram(stderr_path=os.path.join(DATA, "prefetch_scope.stderr.log"))
    try:
        for pr in PILOTS:
            pid = imap.get(str(pr))
            files = predicted_files(pr)
            if not pid or not files:
                print(f"PR {pr}: no pid/files, skip"); continue
            out = eng.tool("find_similar_changes",
                           {"project_id": pid, "files": files, "top": 6, "max_commits": 800})
            path = os.path.join(P2, f"pr{pr}_ctx_scope.md")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write("# Precedent scope — how this team makes similar changes\n\n")
                fh.write("Engram found the most similar PAST changes (by file-shape) and which "
                         "files they touched, plus companions your plan may miss. Use this to "
                         "CALIBRATE your scope: match the SIZE and LAYERS of these precedents — "
                         "do not over-build (no infra/files beyond what precedents show) and do "
                         "not under-build (include the recurring companions).\n\n")
                fh.write(out)
            print(f"PR {pr}: scope ctx {len(out)} chars -> {path}")
    finally:
        eng.close()


if __name__ == "__main__":
    main()
