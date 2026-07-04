"""Business-logic comprehension experiment (gpt-oss-120b via OpenRouter).

Tests the skipped lever: does feeding Engram's LLM business-logic comprehension
(powered by gpt-oss-120b) of the affected area into the dossier lift the
engram-arm functional result on the 5 worst PRs?

Leak-free: analyze_business_logic reads from the per-PR project's registered
directory = the base_commit worktree. Key read from the gitignored secrets file,
never printed.

  python eval/_bl_experiment.py smoke
  python eval/_bl_experiment.py analyze      # all 5 PRs -> bl/pr{pr}_bl.json + addendum
"""
import glob
import json
import os
import re
import sys
import threading
import time

import engram_client as ec

KEY = open(os.path.join("eval", ".secrets", "openrouter_key.txt"), encoding="utf-8").read().strip()
MODEL = os.environ.get("BL_MODEL", "openai/gpt-oss-120b")
SUFFIX = os.environ.get("BL_SUFFIX", "")  # e.g. "_bl2" to keep prior results
CONFIG_OUT = os.path.join(os.environ.get("TEMP", "/tmp"), "engram_bl_config.yaml")
BL_DIR = os.path.join("eval", "data", "p2", "bl")
PIDS = {
    "1933": "ad719aa8-d2e6-4eb1-bf95-c5fe98212521",
    "1967": "a526ede2-3bbe-438c-bba0-0e213799018c",
    "1913": "85583fd9-709f-4b9c-a763-258ae2acea50",
    "1955": "08f94e3c-3ad2-4fe9-83f5-1a48cc214df3",
    "1938": "6f38f41e-65cf-4090-9350-23f0942a3e08",
}
CODE_EXT = (".aspx.vb", ".ascx.vb", ".asmx.vb", ".ashx.vb", ".svc.vb", ".vb", ".cs")


def build_bl_config():
    with open(ec.PROD_CONFIG, encoding="utf-8") as fh:
        lines = fh.readlines()
    out, injected = [], False
    wt_yaml = ec.WORKTREE_ROOT.replace("'", "''")
    for ln in lines:
        s = ln.strip()
        if s.startswith("multi_client:"):
            out.append("multi_client: false\n"); continue
        if s.startswith("llm_model:"):
            out.append(f'llm_model: "{MODEL}"\n'); continue
        if s.startswith("llm_openai_api_key:"):
            out.append(f'llm_openai_api_key: "{KEY}"\n'); continue
        out.append(ln)
        if not injected and s == "allowed_roots:":
            out.append(f"  - '{wt_yaml}'\n"); injected = True
    if not injected:
        raise RuntimeError("no allowed_roots in prod config")
    with open(CONFIG_OUT, "w", encoding="utf-8") as fh:
        fh.writelines(out)
    return CONFIG_OUT


ec.build_eval_config = build_bl_config


def analyze_file(eng, pid, rel):
    """Strictly serial call (no overlap — overlap desyncs the JSON-RPC stream)."""
    txt = eng.tool("analyze_business_logic",
                   {"project_id": pid, "file_path": rel, "output_json": True, "max_concurrent": 6})
    if txt.startswith("__TOOL_ERROR__"):
        return {"file": rel, "error": txt[:200]}
    try:
        return json.loads(txt)
    except Exception as e:
        return {"file": rel, "parse_error": str(e), "raw": txt[:300]}


def _too_big(path):
    """Skip auto-generated / god-files so every analyzed file finishes fast
    (each method is one ~15s gpt-oss call; tight caps keep a file well under a
    minute, so no per-call timeout is needed)."""
    pl = path.lower()
    if ".designer." in pl:
        return True
    try:
        txt = open(path, encoding="utf-8", errors="ignore").read()
    except Exception:
        return True
    if txt.count("\n") > 900:
        return True
    if txt.lower().count("end sub") + txt.lower().count("end function") > 18:
        return True
    return False


def dossier_code_files(pr, wt, limit=3):
    """Top code-behind/.vb candidate files from the dossier (leak-free, US-derived),
    resolved to worktree-relative paths by basename match."""
    doss = open(f"eval/data/p2/pr{pr}_dossier.md", encoding="utf-8").read()
    # candidate section onward
    cut = doss.find("## Candidate files")
    body = doss[cut:] if cut >= 0 else doss
    paths = re.findall(r"`([^`]+)`", body)
    seen, picks = set(), []
    for p in paths:
        pl = p.lower()
        if not any(pl.endswith(e) for e in CODE_EXT):
            continue
        base = os.path.basename(pl)
        if base in seen or ".designer." in pl:
            continue
        # resolve in worktree by basename (handles Site/ web-root prefix differences)
        matches = glob.glob(os.path.join(wt, "**", os.path.basename(p)), recursive=True)
        if not matches:
            continue
        seen.add(base)
        if _too_big(matches[0]):
            continue
        rel = os.path.relpath(matches[0], wt).replace("\\", "/")
        picks.append(rel)
        if len(picks) >= limit:
            break
    return picks


def addendum_md(pr, files_logic):
    md = ["## Business logic of the affected area (Engram comprehension — gpt-oss-120b)",
          "These are the rules/guards ALREADY in the code you are about to change. Honor existing "
          "rules; check whether the story needs a NEW gate analogous to these.\n"]
    for fl in files_logic:
        if not isinstance(fl, dict) or "methods" not in fl:
            continue
        md.append(f"### {fl.get('class_name','?')} — {fl.get('file_path','?')}")
        if fl.get("file_purpose"):
            md.append(f"*{fl['file_purpose']}*")
        for m in fl.get("methods", []):
            rules = m.get("business_rules", [])
            if not rules and not m.get("purpose"):
                continue
            md.append(f"- **{m.get('method_name','?')}**: {m.get('purpose','')}")
            for r in rules:
                md.append(f"    - rule: {r}")
        md.append("")
    return "\n".join(md)


def analyze_all():
    os.makedirs(BL_DIR, exist_ok=True)
    for pr, pid in PIDS.items():
        if os.path.exists(os.path.join(BL_DIR, f"pr{pr}_dossier_bl.md")):
            print(f"=== PR{pr}: already done, skipping ===", flush=True)
            continue
        wt = os.path.join(ec.WORKTREE_ROOT, f"pr{pr}")
        files = dossier_code_files(pr, wt, limit=3)
        print(f"\n=== PR{pr}: analyzing {len(files)} files (fresh server) ===", flush=True)
        # Fresh server PER PR so a crash on one PR cannot poison the rest.
        eng = ec.Engram(stderr_path=os.path.join(ec.WORKTREE_ROOT, f"bl_{pr}.stderr.log"))
        results = []
        try:
            for rel in files:
                t = time.time()
                r = analyze_file(eng, pid, rel)
                n = len(r.get("methods", [])) if isinstance(r, dict) else 0
                nr = sum(len(m.get("business_rules", [])) for m in r.get("methods", [])) if n else 0
                print(f"   {rel}: {n} methods, {nr} rules ({time.time()-t:.0f}s)"
                      + (f"  ERR {r.get('error') or r.get('parse_error')}" if 'error' in r or 'parse_error' in r else ""),
                      flush=True)
                results.append(r)
        except Exception as e:
            print(f"   PR{pr} aborted: {e}", flush=True)
        finally:
            eng.close()
        json.dump(results, open(os.path.join(BL_DIR, f"pr{pr}_bl.json"), "w", encoding="utf-8"), indent=1)
        add = addendum_md(pr, results)
        open(os.path.join(BL_DIR, f"pr{pr}_bl_addendum.md"), "w", encoding="utf-8").write(add)
        base = open(f"eval/data/p2/pr{pr}_dossier.md", encoding="utf-8").read()
        open(os.path.join(BL_DIR, f"pr{pr}_dossier_bl.md"), "w", encoding="utf-8").write(base + "\n\n" + add)
    print("\nDONE. addenda + augmented dossiers in", BL_DIR)


DIRECT_PROMPT = """You are analyzing a VB.NET source file from a web application.
List ONLY the business rules, guards, permission/role checks, required settings,
validation conditions, and branch logic that GATE behavior — exactly as encoded
in THIS code. Do not invent rules that aren't present.

```vb.net
{body}
```

Respond with STRICT JSON only (no prose, no backticks):
{{"file_purpose":"<one sentence>","rules":["<rule actually in the code>", "..."]}}"""


def direct_call(body):
    """One gpt-oss-120b call per file via OpenRouter, hard timeout + 1 retry."""
    import urllib.request
    prompt = DIRECT_PROMPT.format(body=body[:8000])
    payload = json.dumps({"model": MODEL, "messages": [{"role": "user", "content": prompt}],
                          "max_tokens": 2000, "temperature": 0.2}).encode()
    for attempt in (1, 2):
        try:
            req = urllib.request.Request("https://openrouter.ai/api/v1/chat/completions",
                data=payload, headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})
            d = json.load(urllib.request.urlopen(req, timeout=120))
            content = (d["choices"][0]["message"].get("content") or "").strip()
            content = content.removeprefix("```json").removeprefix("```").removesuffix("```").strip()
            return json.loads(content)
        except Exception as e:
            if attempt == 2:
                return {"file_purpose": "", "rules": [], "error": str(e)[:120]}
            time.sleep(3)


def direct_all():
    """Robust per-file business-logic addenda via direct OpenRouter calls.
    Model from BL_MODEL, output suffix from SUFFIX (so runs don't overwrite)."""
    os.makedirs(BL_DIR, exist_ok=True)
    prs = os.environ.get("BL_PRS", "1913,1955,1938").split(",")
    print(f"model={MODEL}  suffix={SUFFIX!r}  prs={prs}", flush=True)
    for pr in prs:
        if os.path.exists(os.path.join(BL_DIR, f"pr{pr}_dossier{SUFFIX}.md")):
            print(f"PR{pr}: done, skip"); continue
        wt = os.path.join(ec.WORKTREE_ROOT, f"pr{pr}")
        # pick top code files from dossier (leak-free); designer excluded, but no method cap (1 call/file)
        doss = open(f"eval/data/p2/pr{pr}_dossier.md", encoding="utf-8").read()
        cut = doss.find("## Candidate files"); body = doss[cut:] if cut >= 0 else doss
        seen, files = set(), []
        for p in re.findall(r"`([^`]+)`", body):
            pl = p.lower()
            if not any(pl.endswith(e) for e in CODE_EXT) or ".designer." in pl:
                continue
            b = os.path.basename(pl)
            if b in seen:
                continue
            matches = glob.glob(os.path.join(wt, "**", os.path.basename(p)), recursive=True)
            if not matches:
                continue
            seen.add(b); files.append((os.path.relpath(matches[0], wt).replace("\\", "/"), matches[0]))
            if len(files) >= 3:
                break
        print(f"\n=== PR{pr}: {len(files)} files (direct gpt-oss) ===", flush=True)
        md = ["## Business logic of the affected area (Engram comprehension — gpt-oss-120b)",
              "Rules/guards ALREADY in the code you are about to change. Honor existing rules; "
              "check whether the story needs a NEW gate analogous to these.\n"]
        results = []
        for rel, full in files:
            t = time.time()
            src = open(full, encoding="utf-8", errors="ignore").read()
            r = direct_call(src)
            rules = r.get("rules", [])
            print(f"   {rel}: {len(rules)} rules ({time.time()-t:.0f}s){' ERR '+r['error'] if r.get('error') else ''}", flush=True)
            results.append({"file": rel, **r})
            md.append(f"### {rel}")
            if r.get("file_purpose"):
                md.append(f"*{r['file_purpose']}*")
            for rule in rules:
                md.append(f"- {rule}")
            md.append("")
        json.dump(results, open(os.path.join(BL_DIR, f"pr{pr}_bl{SUFFIX}.json"), "w", encoding="utf-8"), indent=1)
        add = "\n".join(md)
        open(os.path.join(BL_DIR, f"pr{pr}_bl_addendum{SUFFIX}.md"), "w", encoding="utf-8").write(add)
        base = open(f"eval/data/p2/pr{pr}_dossier.md", encoding="utf-8").read()
        open(os.path.join(BL_DIR, f"pr{pr}_dossier{SUFFIX}.md"), "w", encoding="utf-8").write(base + "\n\n" + add)
    print("\nDIRECT DONE.")


def smoke():
    eng = ec.Engram(stderr_path=os.path.join(ec.WORKTREE_ROOT, "bl_smoke.stderr.log"))
    try:
        r = analyze_file(eng, PIDS["1933"], "Site/language.aspx.vb")
        n = len(r.get("methods", [])) if isinstance(r, dict) else 0
        nr = sum(len(m.get("business_rules", [])) for m in r.get("methods", [])) if n else 0
        print(f"smoke: {n} methods, {nr} rules; err={r.get('error')}")
        for m in r.get("methods", [])[:3]:
            print(f"  {m.get('method_name')}: rules={m.get('business_rules')}")
    finally:
        eng.close()


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "smoke"
    {"smoke": smoke, "analyze": analyze_all, "direct": direct_all}.get(mode, smoke)()
