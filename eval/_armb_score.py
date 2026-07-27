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
# One path per BULLET line only — prose paragraphs in the section (e.g.
# "Explicitly NOT touched: `Web.config` ...") must not count as proposals.
proposed = set()
for line in body.splitlines():
    stripped = line.strip()
    if not stripped.startswith(("-", "*")):
        continue
    stripped = stripped.lstrip("-*").strip()
    m = re.search(r"`([^`]+)`", stripped) or re.search(r"([\w~./\\-]+\.[\w.]+)", stripped)
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

# ---- name-tolerant column -------------------------------------------------
# 100 devs produce 100 namings for the same artifact; the agent cannot be
# docked for `RoQMaxNum` vs `RoQMaximumNumberOfImage` (user ruling
# 2026-07-10). Pair each FP with the FN it most plausibly IS — same
# extension plus either high stem similarity or a long shared prefix —
# and report a second F1 where those pairs count as hits. Pairs are
# printed for eyeball verification; the exact column stays primary.
from difflib import SequenceMatcher


def stem_ext(b):
    if "." in b:
        s, e = b.split(".", 1)
    else:
        s, e = b, ""
    return s, e


def common_prefix_len(a, b):
    n = 0
    for x, y in zip(a, b):
        if x != y:
            break
        n += 1
    return n


candidates = []
for f in fp:
    fs, fe = stem_ext(f)
    for g in fn:
        gs, ge = stem_ext(g)
        if fe != ge:
            continue
        ratio = SequenceMatcher(None, fs, gs).ratio()
        if ratio >= 0.6 or common_prefix_len(fs, gs) >= 6:
            candidates.append((ratio, f, g))

pairs = []
used_fp, used_fn = set(), set()
for ratio, f, g in sorted(candidates, reverse=True):
    if f in used_fp or g in used_fn:
        continue
    pairs.append((f, g, ratio))
    used_fp.add(f)
    used_fn.add(g)

if pairs:
    tp2 = len(tp) + len(pairs)
    p2 = tp2 / len(proposed) if proposed else 0.0
    r2 = tp2 / len(real) if real else 0.0
    f12 = 2 * p2 * r2 / (p2 + r2) if p2 + r2 else 0.0
    print("\nNAME-VARIANT pairs (proposed ≈ real, credited below):")
    for f, g, ratio in pairs:
        print(f"  {f}  ≈  {g}  (sim {ratio:.2f})")
    print(
        f"\nname-tolerant: TP={tp2} FP={len(fp) - len(pairs)} FN={len(fn) - len(pairs)}"
        f"  P={p2 * 100:.1f} R={r2 * 100:.1f} F1={f12 * 100:.1f}"
    )
else:
    print("\nname-tolerant: no variant pairs found — F1 unchanged")
