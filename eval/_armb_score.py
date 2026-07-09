"""Score an arm-B implementation plan (markdown with a '## Files to change'
section) against the PR's real changed_files. Canon-basename matching like
the historical arm-B scoring; prints TP/FP/FN with per-file classification
so riders / factoring choices can be judged by hand afterwards.

Usage: python eval/_armb_score.py <pr_id> <result_md_path>
"""
import ast
import json
import re
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
pr, path = sys.argv[1], sys.argv[2]

recs = {str(r["pr_id"]): r for r in json.load(open("eval/data/ociusx_prs.json", encoding="utf-8"))}
gt = recs[pr]["ground_truth"]
gt = ast.literal_eval(gt) if isinstance(gt, str) else gt


def base(p):
    return p.replace("\\", "/").lower().rstrip("/").split("/")[-1]


real = {base(f["path"] if isinstance(f, dict) else f) for f in gt["changed_files"]}

md = open(path, encoding="utf-8").read()
sec = md.split("## Files to change", 1)
if len(sec) < 2:
    print("no '## Files to change' section found")
    sys.exit(2)
body = sec[1].split("\n## ", 1)[0]
# One path per bullet: first `code span` or bare path-ish token on the line.
proposed = set()
for line in body.splitlines():
    line = line.strip().lstrip("-*").strip()
    if not line:
        continue
    m = re.search(r"`([^`]+)`", line) or re.search(r"([\w~./\\-]+\.[\w.]+)", line)
    if m:
        proposed.add(base(m.group(1)))

tp = sorted(proposed & real)
fp = sorted(proposed - real)
fn = sorted(real - proposed)
p = len(tp) / len(proposed) if proposed else 0.0
r = len(tp) / len(real) if real else 0.0
f1 = 2 * p * r / (p + r) if p + r else 0.0
print(f"proposed={len(proposed)} real={len(real)}  TP={len(tp)} FP={len(fp)} FN={len(fn)}")
print(f"P={p * 100:.1f} R={r * 100:.1f} F1={f1 * 100:.1f}")
print("\nTP:", *tp, sep="\n  ")
print("\nFP (proposed, not in PR — judge: rider-class? factoring?):", *fp, sep="\n  ")
print("\nFN (in PR, not proposed):", *fn, sep="\n  ")
