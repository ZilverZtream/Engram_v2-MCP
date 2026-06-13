"""Post-hoc analysis of a Phase-1 tournament result file.

Answers the user's real question — "what is the best Engram tool SEQUENCE to land
closest to the merged implementation?" — with per-strategy win-rates, a
findable-vs-unwinnable split, and a recommended sequence. Read-only.

Usage: python eval/analyze_phase1.py [eval/data/phase1_pilot.json]
"""
import json
import sys
import os

PRIMARY = "recall_modified_page"   # headline metric


def mean(xs):
    return sum(xs) / len(xs) if xs else 0.0


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else \
        os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "phase1_pilot.json")
    with open(path, encoding="utf-8") as fh:
        rows = json.load(fh)

    names = []
    for r in rows:
        for n in r.get("strategies", {}):
            if n not in names:
                names.append(n)

    # Per-strategy aggregates.
    agg = {n: {"pg": [], "mod": [], "all": [], "prec": [], "np": [], "wins": 0,
               "errors": 0} for n in names}
    findable, hard = [], []

    print("=" * 92)
    print("PER-STORY RESULTS (best strategy by recall_modified_page)")
    print("=" * 92)
    for r in rows:
        pr = r["pr_id"]
        strat = r.get("strategies", {})
        # best by primary metric (None treated as 0)
        scored = [(n, (s.get(PRIMARY) or 0.0), s) for n, s in strat.items() if "error" not in s]
        best_pg = max((v for _, v, _ in scored), default=0.0)
        winners = [n for n, v, _ in scored if v == best_pg and best_pg > 0]
        for n, v, s in scored:
            a = agg[n]
            a["pg"].append(s.get(PRIMARY) or 0.0)
            a["mod"].append(s.get("recall_modified") or 0.0)
            a["all"].append(s.get("recall_all") or 0.0)
            if s.get("precision") is not None:
                a["prec"].append(s["precision"])
            a["np"].append(s.get("n_predicted", 0))
        for n, s in strat.items():
            if "error" in s:
                agg[n]["errors"] += 1
        # award a win share to each tied winner
        for n in winners:
            agg[n]["wins"] += 1
        title = (r.get("title", "")[:46])
        if best_pg > 0:
            findable.append(pr)
            print(f"PR {pr} [{r.get('author','')[:14]:14s}] {title:46s} "
                  f"best={best_pg:.2f} by {','.join(winners)}")
        else:
            hard.append(pr)
            print(f"PR {pr} [{r.get('author','')[:14]:14s}] {title:46s} "
                  f"UNWINNABLE (all strategies 0)")

    print(f"\nfindable: {len(findable)}/{len(rows)}  hard/unwinnable: {len(hard)} {hard}")

    print("\n" + "=" * 92)
    print(f"STRATEGY TOURNAMENT — ranked by mean {PRIMARY} (n={len(rows)} stories)")
    print("=" * 92)
    print(f"{'strategy':22s} {'mean_page':>9s} {'mean_mod':>9s} {'mean_all':>9s} "
          f"{'precision':>9s} {'avg_pred':>9s} {'wins':>5s} {'err':>4s}")
    ranked = sorted(names, key=lambda n: mean(agg[n]["pg"]), reverse=True)
    for n in ranked:
        a = agg[n]
        print(f"{n:22s} {mean(a['pg']):>9.3f} {mean(a['mod']):>9.3f} {mean(a['all']):>9.3f} "
              f"{mean(a['prec']):>9.3f} {mean(a['np']):>9.1f} {a['wins']:>5d} {a['errors']:>4d}")

    # Same ranking restricted to FINDABLE stories (removes the unwinnable floor).
    print(f"\n--- restricted to {len(findable)} findable stories ---")
    print(f"{'strategy':22s} {'mean_page':>9s} {'mean_mod':>9s} {'precision':>9s} {'avg_pred':>9s}")
    fset = set(findable)
    for n in ranked:
        pg = [r["strategies"][n].get(PRIMARY) or 0.0 for r in rows
              if r["pr_id"] in fset and n in r.get("strategies", {}) and "error" not in r["strategies"][n]]
        md = [r["strategies"][n].get("recall_modified") or 0.0 for r in rows
              if r["pr_id"] in fset and n in r.get("strategies", {}) and "error" not in r["strategies"][n]]
        pc = [r["strategies"][n].get("precision") for r in rows
              if r["pr_id"] in fset and n in r.get("strategies", {}) and "error" not in r["strategies"][n]
              and r["strategies"][n].get("precision") is not None]
        npd = [r["strategies"][n].get("n_predicted", 0) for r in rows
               if r["pr_id"] in fset and n in r.get("strategies", {}) and "error" not in r["strategies"][n]]
        print(f"{n:22s} {mean(pg):>9.3f} {mean(md):>9.3f} {mean(pc):>9.3f} {mean(npd):>9.1f}")

    if ranked:
        w = ranked[0]
        print(f"\n>>> WINNER: {w}  (mean {PRIMARY}={mean(agg[w]['pg']):.3f}, "
              f"{agg[w]['wins']} per-story wins, precision={mean(agg[w]['prec']):.3f})")
        # Best precision-respecting alternative: highest page recall with avg_pred <= 50
        alt = [n for n in ranked if mean(agg[n]["np"]) <= 50]
        if alt and alt[0] != w:
            print(f">>> Best focused (avg_pred<=50): {alt[0]} "
                  f"(page={mean(agg[alt[0]]['pg']):.3f}, avg_pred={mean(agg[alt[0]]['np']):.1f})")


if __name__ == "__main__":
    main()
