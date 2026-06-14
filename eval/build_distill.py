"""Inject a language batch of findings into phase2_distill.js so the bulk corpus
rides in the workflow script (via scriptPath) and never enters the orchestrator's
context. Usage: python eval/build_distill.py <batch> lang1 lang2 ...
  e.g. python eval/build_distill.py vb vbnet
       python eval/build_distill.py web webforms resx sql razor config other
       python eval/build_distill.py client typescript javascript css
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "data")
TEMPLATE = os.path.join(HERE, "phase2_distill.js")

batch = sys.argv[1]
langs = set(sys.argv[2:])
findings = json.load(open(os.path.join(DATA, "qg_findings_dedup.json"), encoding="utf-8"))
# Distill rules ONLY from findings the team did NOT reject. A wontFix/byDesign
# finding was deliberately declined (often an analyzer modern-syntax suggestion
# that breaks the ES5/WebGrease bundle) — it must not become a "do this" rule.
# The ES5 override itself is carried authoritatively by .coderabbit.yaml/copilot.
REJECTED = {"wontfix", "bydesign"}
sub = [f for f in findings
       if f["lang"] in langs and (f.get("resolution") or "").lower() not in REJECTED]
n_rej = sum(1 for f in findings if f["lang"] in langs and (f.get("resolution") or "").lower() in REJECTED)
print(f"  (excluded {n_rej} rejected wontFix/byDesign findings in this batch)")
tpl = open(TEMPLATE, encoding="utf-8").read()
tpl = tpl.replace("let FINDINGS = null // INJECTED_FINDINGS",
                  "let FINDINGS = " + json.dumps(sub, ensure_ascii=False) + " // INJECTED_FINDINGS")
tpl = tpl.replace("let BATCH = 'all'   // INJECTED_BATCH",
                  "let BATCH = " + json.dumps(batch) + "   // INJECTED_BATCH")
out = os.path.join(DATA, "p2", f"_distill_{batch}.js")
os.makedirs(os.path.dirname(out), exist_ok=True)
open(out, "w", encoding="utf-8").write(tpl)
print(f"batch={batch} langs={sorted(langs)} findings={len(sub)} "
      f"script={os.path.getsize(out)/1024:.0f}KB -> {out}")
