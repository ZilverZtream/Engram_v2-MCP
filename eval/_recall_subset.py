"""Recall + precision of the native get_change_set dossier on a PR subset.

Reads eval/data/p2/pr<N>_prov.json (the exact candidate set dumped by
phase2_prep.py) and scores it against the PR's real changed files, mirroring
_recall_sweep.py's `covered()` so the numbers are comparable.

Usage: python eval/_recall_subset.py 1908 1913 1937 1933 1967 [--json out.json]
"""
import ast
import json
import os
import sys

DATA = os.path.join("eval", "data", "p2")
CORPUS = os.path.join("eval", "data", "ociusx_prs.json")


def canon(p):
    p = p.replace("\\", "/").lower().lstrip("/")
    for pre in ("site/", "src/"):
        if p.startswith(pre):
            p = p[len(pre):]
    return p


def changed(rec):
    gt = rec.get("ground_truth")
    if isinstance(gt, str):
        gt = ast.literal_eval(gt)
    out = []
    for f in gt.get("changed_files", []):
        if isinstance(f, dict):
            out.append((canon(f["path"]), f.get("change", "edit")))
        else:
            out.append((canon(f), "edit"))
    return out


def covered(cands, rf):
    tail = "/".join(rf.split("/")[-3:])
    base = rf.split("/")[-1]
    return any(rf == c or c.endswith(tail) or c.endswith("/" + base) for c in cands)


def hit_by_candidate(cands, real):
    """True for each candidate that matches some real file (precision side)."""
    out = []
    for c in cands:
        out.append(any(c == rf or c.endswith("/".join(rf.split("/")[-3:])) or rf.endswith("/" + c.split("/")[-1]) for rf in real))
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    out_json = None
    if "--json" in sys.argv:
        out_json = sys.argv[sys.argv.index("--json") + 1]
    prs = args or ["1908", "1913", "1937", "1933", "1967"]
    recs = {str(r["pr_id"]): r for r in json.load(open(CORPUS, encoding="utf-8"))}
    rows = []
    tot_hit = tot_real = tot_cand = tot_prec = 0
    for pr in prs:
        rec = recs.get(pr)
        ppath = os.path.join(DATA, f"pr{pr}_prov.json")
        if not rec or not os.path.exists(ppath):
            print(f"PR{pr}: missing prov/record")
            continue
        prov = json.load(open(ppath, encoding="utf-8"))
        cands = [canon(p) for p in prov]
        real = changed(rec)
        real_paths = [p for p, _ in real]
        hits = sum(1 for rf in real_paths if covered(cands, rf))
        prec_hits = sum(hit_by_candidate(cands, real_paths))
        rows.append({"pr": pr, "recall": hits, "real": len(real_paths), "cands": len(cands), "prec_hits": prec_hits})
        tot_hit += hits; tot_real += len(real_paths); tot_cand += len(cands); tot_prec += prec_hits
        print(f"PR{pr}: recall {hits}/{len(real_paths)} = {hits/len(real_paths):.0%} | candidates {len(cands)} | precision {prec_hits}/{len(cands)} = {prec_hits/max(1,len(cands)):.0%}")
    if tot_real:
        print(f"OVERALL: recall {tot_hit}/{tot_real} = {tot_hit/tot_real:.1%} | precision {tot_prec}/{tot_cand} = {tot_prec/max(1,tot_cand):.1%} | mean candidates {tot_cand/len(rows):.0f}")
    if out_json:
        json.dump(rows, open(out_json, "w", encoding="utf-8"), indent=1)


if __name__ == "__main__":
    main()
