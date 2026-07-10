"""Mechanism-matched scoring prototype: an LLM judge that decides whether an
arm-B plan fixes/implements the SAME MECHANISM as the merged PR, independent
of layer/file choices - the blind spot of file-set F1 (live: runs 15/17
chose a defensible different layer and were F1-punished for it).

Uses OpenRouter (key from the engram server config) with a FREE judge model
by default, so batch judging costs nothing.

Usage: python eval/_mech_judge.py <pr_id> <result_md_path> [model]
"""
import json
import re
import sys
import urllib.request

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

CONFIG = r"C:\Users\Dennis\AppData\Roaming\engram\engram\config\engram_mcp.yaml"
DEFAULT_MODEL = "tencent/hy3:free"


def api_key():
    for line in open(CONFIG, encoding="utf-8"):
        m = re.match(r"\s*llm_openai_api_key:\s*[\"']?([^\"'\s]+)", line)
        if m:
            return m.group(1)
    raise SystemExit("no OpenRouter key in engram config")


pr, path = sys.argv[1], sys.argv[2]
model = sys.argv[3] if len(sys.argv) > 3 else DEFAULT_MODEL

recs = {str(r["pr_id"]): r for r in json.load(open("eval/data/ociusx_prs.json", encoding="utf-8"))}
rec = recs[pr]
gt = rec["ground_truth"]
if isinstance(gt, str):
    import ast

    gt = ast.literal_eval(gt)
gt_files = [f["path"] if isinstance(f, dict) else f for f in gt.get("changed_files", [])]
plan = open(path, encoding="utf-8").read()[:12000]

prompt = f"""You are judging whether an implementation PLAN solves the same problem as the team's actually-merged PR, at the MECHANISM level. Different file/layer choices that solve the same defect/story equally well must NOT be penalized.

## The story/bug
{rec['story'].get('title','')}
{rec['story'].get('description','')[:800]}

## The team's merged PR (ground truth)
Title: {gt.get('pr_title','')}
Description: {gt.get('pr_description','')[:800]}
Changed files: {json.dumps(gt_files, indent=0)}

## The plan being judged
{plan}

Answer in strict JSON only:
{{"mechanism_match": 0-100, "same_defect_understood": true/false, "layer_fork": true/false, "would_fix_or_implement": true/false, "rationale": "<=60 words"}}
Scoring guide: 90-100 = same understanding + equivalent complete fix (any layer); 60-89 = same defect, partially complete or riskier; 30-59 = partial understanding; <30 = wrong problem."""

req = urllib.request.Request(
    "https://openrouter.ai/api/v1/chat/completions",
    data=json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0,
        }
    ).encode(),
    headers={"Authorization": f"Bearer {api_key()}", "Content-Type": "application/json"},
)
with urllib.request.urlopen(req, timeout=180) as r:
    out = json.load(r)
text = out["choices"][0]["message"]["content"]
m = re.search(r"\{.*\}", text, re.S)
print(f"model: {model}")
print(m.group(0) if m else text)
