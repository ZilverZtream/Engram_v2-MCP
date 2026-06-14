"""Broad recall sweep across all pilot PRs. For each: dossier hits/misses vs the
real changed_files, then categorize MISSES by:
  - add vs edit (a brand-new 'add' file has no history to retrieve; an 'edit'
    miss is a genuine recall failure Engram could fix)
  - file type / layer
  - area (top-2 path segments)
Aggregate to find the next SYSTEMATIC gap category. Tool-free (reads dossiers)."""
import ast
import json
import os
from collections import Counter

DATA = os.path.join("eval", "data", "p2")
CORPUS = os.path.join("eval", "data", "ociusx_prs.json")
imap = json.load(open(os.path.join(DATA, "index_map.json"), encoding="utf-8"))
recs = {str(r["pr_id"]): r for r in json.load(open(CORPUS, encoding="utf-8"))}


def canon(p):
    p = p.replace("\\", "/").lower().lstrip("/")
    for pre in ("site/", "src/"):
        if p.startswith(pre):
            p = p[len(pre):]
    return p


def ext(p):
    b = p.rsplit("/", 1)[-1]
    for e in (".aspx.vb", ".ascx.vb", ".aspx", ".ascx", ".designer.vb"):
        if b.endswith(e):
            return e
    return "." + b.rsplit(".", 1)[-1] if "." in b else "(noext)"


def changed(rec):
    gt = rec.get("ground_truth")
    if isinstance(gt, str):
        gt = ast.literal_eval(gt)
    out = []
    for f in gt.get("changed_files", []):
        if isinstance(f, dict):
            out.append((f["path"], f.get("change", "edit")))
        else:
            out.append((f, "edit"))
    return out


def covered(doss, rf):
    tail = "/".join(rf.split("/")[-3:])
    base = rf.split("/")[-1]
    return rf in doss or tail in doss or ("/" + base) in doss


tot_real = tot_hit = 0
miss_by_change = Counter()
miss_edit_ext = Counter()
miss_edit_area = Counter()
add_total = 0
per_pr = []
for pr in imap:
    rec = recs.get(pr)
    dpath = os.path.join(DATA, f"pr{pr}_dossier.md")
    if not rec or not os.path.exists(dpath):
        continue
    doss = open(dpath, encoding="utf-8").read().lower().replace("\\", "/")
    cf = changed(rec)
    real_total = len(cf)
    hit = 0
    for path, chg in cf:
        rf = canon(path)
        if chg == "add":
            add_total += 1
        if covered(doss, rf):
            hit += 1
        else:
            miss_by_change[chg] += 1
            if chg == "edit":
                miss_edit_ext[ext(rf)] += 1
                miss_edit_area["/".join(rf.split("/")[:2])] += 1
    tot_real += real_total
    tot_hit += hit
    per_pr.append((pr, hit, real_total))

print(f"OVERALL dossier recall: {tot_hit}/{tot_real} = {tot_hit/tot_real:.1%}  ({add_total} of the real files are brand-new 'add' files)")
print("\nper-PR:")
for pr, h, t in sorted(per_pr, key=lambda x: x[1] / x[2]):
    print(f"  PR{pr}: {h}/{t} = {h/t:.0%}")
print("\nMISSES by change-type:", dict(miss_by_change))
print("\nEDIT-misses (genuine recall failures) by file type:")
for e, n in miss_edit_ext.most_common():
    print(f"   {n:3d}  {e}")
print("\nEDIT-misses by area:")
for a, n in miss_edit_area.most_common(12):
    print(f"   {n:3d}  {a}")
