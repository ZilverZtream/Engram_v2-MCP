"""Aggregate replay_pr*.json into the iteration-tax gap report.

Sums file-level pre-catch across all replayed PRs and clusters the
MISSED real findings into rough mechanism classes (keyword heuristics
over the reviewer text + embedded Sonar rule keys) so the next gate /
distilled rule is chosen by frequency, not anecdote. Also reports gate
noise (gate findings on files no reviewer commented on) as a precision
proxy.

Usage: python eval/replay_aggregate.py
"""
import collections
import glob
import io
import json
import os
import re
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
HERE = os.path.dirname(os.path.abspath(__file__))

CLASSES = [
    ("null-safety", r"null|nothing\b|nullreference|nre\b|guard against"),
    ("permissions/guards", r"permission|access|checkwrite|checkread|role|visibilit|guard"),
    ("naming/convention", r"naming|rename|convention|camel|pascal|prefix|suffix"),
    ("complexity/structure", r"complex|refactor|extract|too (long|many)|cognitive|nesting|duplicate"),
    ("resource/disposal", r"dispose|using\b|leak|connection|transaction"),
    ("sql/injection/query", r"sql|injection|parameteri[sz]|query|linq"),
    ("error-handling", r"catch|exception|swallow|error handling|throw|log(ging)?\b"),
    ("localization/resx", r"resx|resource|localiz|translat|hardcod.*string|string.*hardcod"),
    ("async/threading", r"async|await|thread|race|deadlock"),
    ("dead-code/cleanup", r"unused|dead code|remove (unused|commented)|leftover|debug"),
    ("docs/comments", r"comment|documentation|xml doc|summary tag"),
    ("validation/input", r"validat|sanit|bounds|range check|empty check"),
    ("magic-values", r"magic (number|string)|hardcoded (value|number|id)|constant"),
]


def classify(text):
    t = (text or "").lower()
    for name, pat in CLASSES:
        if re.search(pat, t):
            return name
    return "other"


def main():
    files = sorted(glob.glob(os.path.join(HERE, "data", "p2", "replay_pr*.json")))
    total_real, total_caught, rows = 0, 0, []
    miss_class = collections.Counter()
    miss_examples = collections.defaultdict(list)
    caught_class = collections.Counter()
    gate_hits = collections.Counter()
    noise_total, gate_total = 0, 0
    no_file = 0
    for fp in files:
        d = json.load(open(fp, encoding="utf-8"))
        pr = d["pr_id"]
        total_real += d["real_findings"]
        total_caught += d["file_level_caught"]
        rows.append((pr, d["file_level_caught"], d["real_findings"],
                     d.get("gate_findings_total", 0), d.get("iterations", "?")))
        real_files = set()
        for c in d.get("caught", []):
            cls = classify(c["real"].get("text"))
            caught_class[cls] += 1
            real_files.add((c["real"].get("file") or "").lower())
            for g in c.get("gates", []):
                gate_hits[g.get("gate", "?")] += 1
        for m in d.get("missed", []):
            cls = classify(m.get("text"))
            miss_class[cls] += 1
            if len(miss_examples[cls]) < 4:
                miss_examples[cls].append((pr, (m.get("text") or "")[:130]))
            if not (m.get("file") or "").strip():
                no_file += 1
        gate_total += d.get("gate_findings_total", 0)

    print(f"replayed PRs: {len(files)}")
    print("PR     caught/real  gates  iterations")
    for pr, c, r, g, it in rows:
        print(f"{pr}   {c:3d}/{r:<4d}     {g:4d}   {it}")
    pct = 100 * total_caught / total_real if total_real else 0
    print(f"\nTOTAL file-level pre-catch: {total_caught}/{total_real} = {pct:.1f}%")
    print(f"(real findings with NO file path — unmatchable, often CTO-judgment class: {no_file})")

    print("\n== MISSED classes (ranked — each is a gate/rule candidate) ==")
    for cls, n in miss_class.most_common():
        print(f"{n:4d}  {cls}")
        for pr, ex in miss_examples[cls][:3]:
            print(f"        PR{pr}: {ex}")
    print("\n== CAUGHT classes ==")
    for cls, n in caught_class.most_common():
        print(f"{n:4d}  {cls}")
    print("\n== which gates did the catching ==")
    for g, n in gate_hits.most_common():
        print(f"{n:4d}  {g}")
    print(f"\ngate findings total across replays: {gate_total} "
          f"(precision proxy: compare against caught volume above)")


if __name__ == "__main__":
    main()
