"""Measure what the IMPROVED dossier actually delivers to the agent: recall +
precision of the rendered (agent-visible) dossier vs the real merged MODIFIED
files, at both exact-path and page-family level. Fast, deterministic — the gate
before spending on the Phase-2 A/B. Read-only.

Usage: python eval/measure_dossier.py 1933 1908 1967 1937 1974
"""
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import canon, page_stem  # noqa: E402

P2 = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "p2")


def dossier_files(pr):
    """Files the agent actually sees in the rendered (capped) dossier."""
    md = open(os.path.join(P2, f"pr{pr}_dossier.md"), encoding="utf-8").read()
    return [canon(x) for x in re.findall(r"^- `([^`]+)`", md, re.M)]


def real_modified(pr):
    m = json.load(open(os.path.join(P2, f"pr{pr}.json"), encoding="utf-8"))
    return [canon(cf["path"]) for cf in m["ground_truth"]["changed_files"]
            if "add" not in (cf.get("change", "") or "").lower()
            and "rename" not in (cf.get("change", "") or "").lower()]


def main():
    prs = [int(x) for x in sys.argv[1:]] or [1933, 1908, 1967, 1937, 1974]
    print(f"{'PR':6}{'#doss':>6}{'real':>5}{'hit':>4}{'recall':>8}{'pg_recall':>10}{'prec':>7}  missed (modified)")
    tr = tpr = 0.0
    for pr in prs:
        doss = dossier_files(pr)
        real = real_modified(pr)
        dset, rset = set(doss), set(real)
        hit = dset & rset
        rec = len(hit) / len(real) if real else 0
        # page-family recall
        dpg = {page_stem(f) for f in doss}
        rpg = {page_stem(f) for f in real}
        pgrec = len(dpg & rpg) / len(rpg) if rpg else 0
        prec = len(hit) / len(doss) if doss else 0
        missed = sorted(p.split("/")[-1] for p in (rset - dset))
        tr += rec
        tpr += pgrec
        print(f"{pr:<6}{len(doss):>6}{len(real):>5}{len(hit):>4}{rec:>8.2f}{pgrec:>10.2f}{prec:>7.2f}  {', '.join(missed)[:60]}")
    n = len(prs)
    print(f"\nMEAN exact-recall: {tr/n:.3f}   MEAN page-recall: {tpr/n:.3f}")


if __name__ == "__main__":
    main()
