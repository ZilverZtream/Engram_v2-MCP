"""Compare two _recall_subset.py --json outputs (before vs after a
get_change_set change): per-PR recall / candidates / precision deltas.

Usage: python eval/_recall_compare.py baseline.json after.json
"""
import json
import sys


def load(p):
    return {r["pr"]: r for r in json.load(open(p, encoding="utf-8"))}


def main():
    a, b = load(sys.argv[1]), load(sys.argv[2])
    print("| PR | recall before → after | candidates before → after | precision before → after |")
    print("|---|---|---|---|")
    ta = tb = ra = rb = ca = cb = pa = pb = 0
    for pr in sorted(set(a) | set(b)):
        x, y = a.get(pr), b.get(pr)
        if not x or not y:
            print(f"| {pr} | missing on one side | | |")
            continue
        print(
            f"| {pr} | {x['recall']}/{x['real']} → {y['recall']}/{y['real']} "
            f"| {x['cands']} → {y['cands']} "
            f"| {x['prec_hits']}/{x['cands']} ({x['prec_hits']/max(1,x['cands']):.0%}) → {y['prec_hits']}/{y['cands']} ({y['prec_hits']/max(1,y['cands']):.0%}) |"
        )
        ra += x["recall"]; rb += y["recall"]; ta += x["real"]; tb += y["real"]
        ca += x["cands"]; cb += y["cands"]; pa += x["prec_hits"]; pb += y["prec_hits"]
    if ta and tb:
        print(
            f"| **all** | {ra}/{ta} ({ra/ta:.1%}) → {rb}/{tb} ({rb/tb:.1%}) "
            f"| {ca} → {cb} | {pa/max(1,ca):.1%} → {pb/max(1,cb):.1%} |"
        )


if __name__ == "__main__":
    main()
