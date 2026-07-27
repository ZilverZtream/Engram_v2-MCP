"""Aggregate repeated arm-B seals for one PR into mean/spread.

Every canonical score in the 2026-07-10 sessions was single-run, and
fork-class PRs swing wildly (PR1937: 85.7 / 50.0 / 75.0 across three
runs of the same protocol). A single F1 is luck-exposed; the honest
statistic is mean ± spread over N reps. This globs every
`armB_<pr>_*.md` seal for a PR, scores each via the existing
`_armb_score.py`, and prints per-seal F1 plus mean/min/max/std for both
the exact and name-tolerant columns.

Usage:
  python eval/_score_reps.py <pr_id>              # all armB_<pr>_*.md seals
  python eval/_score_reps.py <pr_id> a.md b.md    # explicit seal list
"""
import glob
import os
import re
import statistics
import subprocess
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
HERE = os.path.dirname(os.path.abspath(__file__))


def score_one(pr, path):
    """Return (raw_f1, name_tolerant_f1) for one seal, or None on failure."""
    r = subprocess.run(
        [sys.executable, os.path.join(HERE, "_armb_score.py"), str(pr), path],
        capture_output=True, text=True, encoding="utf-8", errors="replace",
    )
    out = r.stdout
    raw_m = re.search(r"^P=[\d.]+ R=[\d.]+ F1=([\d.]+)", out, re.M)
    if not raw_m:
        return None
    raw = float(raw_m.group(1))
    nt_m = re.search(r"name-tolerant:.*?F1=([\d.]+)", out)
    nt = float(nt_m.group(1)) if nt_m else raw
    return raw, nt


def summarize(label, vals):
    if not vals:
        return f"{label}: (none)"
    mean = statistics.mean(vals)
    std = statistics.pstdev(vals) if len(vals) > 1 else 0.0
    return (f"{label}: mean {mean:.1f}  min {min(vals):.1f}  max {max(vals):.1f}  "
            f"std {std:.1f}  (n={len(vals)})")


def main():
    if len(sys.argv) < 2:
        sys.exit("usage: python eval/_score_reps.py <pr_id> [seal.md ...]")
    pr = sys.argv[1]
    if len(sys.argv) > 2:
        seals = sys.argv[2:]
    else:
        seals = sorted(glob.glob(os.path.join(HERE, "data", "p2", f"armB_{pr}_*.md")))
    if not seals:
        sys.exit(f"no seals found for PR{pr} (armB_{pr}_*.md)")

    raws, nts = [], []
    print(f"PR{pr}: {len(seals)} seal(s)")
    for s in seals:
        res = score_one(pr, s)
        name = os.path.basename(s)
        if res is None:
            print(f"  {name}: UNSCORABLE (no '## Files to change'?)")
            continue
        raw, nt = res
        raws.append(raw)
        nts.append(nt)
        tag = "" if raw == nt else f"  (nt {nt:.1f})"
        print(f"  {name}: F1 {raw:.1f}{tag}")
    print()
    print(summarize("raw F1        ", raws))
    print(summarize("name-tolerant ", nts))
    if len(raws) > 1:
        spread = max(raws) - min(raws)
        note = "  ⚠ HIGH — likely a factoring-fork PR; do not canonicalize a single run" if spread >= 20 else ""
        print(f"\nraw spread (max-min): {spread:.1f}{note}")


if __name__ == "__main__":
    main()
