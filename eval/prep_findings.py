"""Prepare the raw finding corpus for distillation: light dedup (same source+lang+
text-prefix), attach recurrence frequency, truncate text, and KEEP source +
resolution + sonar_rule so the distiller can treat them correctly:
  - fixed/active/closed  -> extract as a rule
  - wontFix/byDesign     -> team REJECTED it; not a "do this" rule (Sonar
                            modern-syntax rejections reinforce the ES5 override)
"""
import io
import json
import re
import sys
from collections import Counter

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")

f = json.load(open("eval/data/qg_findings_raw.json", encoding="utf-8"))
seen, uniq = {}, []
for x in f:
    key = (x.get("source", "cr"), x["lang"], re.sub(r"[^a-z0-9]", "", x["text"].lower())[:60])
    if key in seen:
        seen[key] += 1
        continue
    seen[key] = 1
    uniq.append(x)
for x in uniq:
    key = (x.get("source", "cr"), x["lang"], re.sub(r"[^a-z0-9]", "", x["text"].lower())[:60])
    x["freq"] = seen[key]
    x["text"] = x["text"][:240]
    x.pop("pr_id", None)

json.dump(uniq, open("eval/data/qg_findings_dedup.json", "w", encoding="utf-8"), ensure_ascii=False)
size = len(json.dumps(uniq, ensure_ascii=False))
print(f"raw={len(f)} -> dedup={len(uniq)}  size={size / 1024:.0f}KB")
print("by source:", dict(Counter(x.get("source", "?") for x in uniq).most_common()))
print("by resolution:", dict(Counter(x.get("resolution", "?") for x in uniq).most_common()))
print("by lang:", dict(Counter(x["lang"] for x in uniq).most_common()))
n_sonar = sum(1 for x in uniq if x.get("source") == "sonar")
n_sonar_legit = sum(1 for x in uniq if x.get("source") == "sonar"
                    and x.get("resolution") in ("fixed", "active", "closed", ""))
print(f"sonar: {n_sonar} ({n_sonar_legit} fixed/active/closed, rest wontFix/byDesign)")
