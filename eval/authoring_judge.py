"""Judge an authoring-experiment arm against a PR's REAL review findings.

For each finding CodeRabbit/reviewers raised on the merged PR (from
replay_pr<id>.json, fetched from the live threads), ask an LLM judge
whether THIS arm's implementation exhibits the same issue — verdicts:
exhibits / avoided / not_applicable (arm factored the work so the issue's
surface doesn't exist). The armed-vs-unarmed delta in `exhibits` is the
authoring-time KPI: review round-trips the rules section removed.

Changed files are detected by mtime newer than the worktree's prep
snapshot (agents edit in place; snapshots carry the prep timestamp) and
compared against the base commit from the REAL repo (read-only `git show`).

Usage: python eval/authoring_judge.py <pr_id> <alone|engram>
Cache: eval/data/p2/authoring_verdicts_<pr>_<arm>.json
"""
import difflib
import io
import json
import os
import re
import subprocess
import sys
import time
import urllib.request

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
HERE = os.path.dirname(os.path.abspath(__file__))
OCIUSX = r"C:\Users\Dennis\source\repos\OciusX"
WT_ROOT = os.path.join(os.environ.get("TEMP", r"C:\Users\Dennis\AppData\Local\Temp"), "engram_p2_wt")
CONFIG = r"C:\Users\Dennis\AppData\Roaming\engram\engram\config\engram_mcp.yaml"
MODEL = os.environ.get("REPLAY_JUDGE_MODEL", "openai/gpt-oss-120b")


def api_key():
    for line in open(CONFIG, encoding="utf-8"):
        m = re.match(r"\s*llm_openai_api_key:\s*[\"']?([^\"'\s]+)", line)
        if m:
            return m.group(1)
    raise SystemExit("no OpenRouter key")


def ask(prompt, key, retries=3):
    body = json.dumps({"model": MODEL, "messages": [{"role": "user", "content": prompt}],
                       "temperature": 0}).encode()
    for i in range(retries):
        try:
            req = urllib.request.Request(
                "https://openrouter.ai/api/v1/chat/completions", data=body,
                headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=120) as r:
                txt = json.load(r)["choices"][0]["message"]["content"]
            m = re.search(r"\{.*\}", txt, re.S)
            if m:
                return json.loads(m.group(0))
        except Exception:
            time.sleep(2 * (i + 1))
    return None


def base_content(base_commit, rel):
    r = subprocess.run(["git", "-C", OCIUSX, "show", f"{base_commit}:{rel}"],
                       capture_output=True, text=True, encoding="utf-8", errors="replace")
    return r.stdout if r.returncode == 0 else None


def changed_files(wt, since_epoch):
    out = []
    for root, dirs, files in os.walk(wt):
        dirs[:] = [d for d in dirs if d not in (".vs", "packages", "node_modules", "bin", "obj")]
        for f in files:
            p = os.path.join(root, f)
            try:
                if os.path.getmtime(p) > since_epoch:
                    out.append(os.path.relpath(p, wt).replace("\\", "/"))
            except OSError:
                pass
    return out


def build_diff_bundle(pr_id, arm, base_commit, since_epoch, cap=90_000):
    wt = os.path.join(WT_ROOT, f"pr{pr_id}_{arm}")
    files = changed_files(wt, since_epoch)
    parts = []
    for rel in sorted(files):
        new = io.open(os.path.join(wt, rel), encoding="utf-8", errors="replace").read()
        old = base_content(base_commit, rel)
        if old is None:
            parts.append(f"=== NEW FILE {rel} ===\n{new[:6000]}\n")
        else:
            ud = "".join(difflib.unified_diff(
                old.splitlines(keepends=True), new.splitlines(keepends=True),
                fromfile=f"base/{rel}", tofile=f"{arm}/{rel}", n=3))
            parts.append(ud[:8000])
    bundle = "\n".join(parts)
    return files, bundle[:cap]


def main():
    pr_id, arm = int(sys.argv[1]), sys.argv[2]
    since_epoch = float(sys.argv[3]) if len(sys.argv) > 3 else None
    if since_epoch is None:
        raise SystemExit("pass agent-launch epoch as arg 3 (files newer than this = agent edits)")
    man = json.load(open(os.path.join(HERE, "data", "p2", f"pr{pr_id}.json"), encoding="utf-8"))
    base_commit = man.get("base_commit") or man.get("base") or ""
    replay = json.load(open(os.path.join(HERE, "data", "p2", f"replay_pr{pr_id}.json"), encoding="utf-8"))
    findings = [c["real"] for c in replay.get("caught", [])] + replay.get("missed", [])

    files, bundle = build_diff_bundle(pr_id, arm, base_commit, since_epoch)
    print(f"PR{pr_id}/{arm}: {len(files)} changed files, bundle {len(bundle) // 1024}KB, "
          f"{len(findings)} findings to judge")
    key = api_key()
    out_path = os.path.join(HERE, "data", "p2", f"authoring_verdicts_{pr_id}_{arm}.json")
    verdicts = json.load(open(out_path, encoding="utf-8")) if os.path.exists(out_path) else {}
    counts = {"exhibits": 0, "avoided": 0, "not_applicable": 0}
    for i, f in enumerate(findings):
        k = str(i)
        if k not in verdicts:
            v = ask(f"""A reviewer raised this finding on the TEAM'S implementation of a user story:

FILE: {f.get('file')}
FINDING: {(f.get('text') or '')[:450]}

Below is an INDEPENDENT implementation of the same story (unified diffs + new files). Judge whether THIS implementation exhibits the same issue.

{bundle}

Answer strict JSON: {{"verdict": "exhibits"|"avoided"|"not_applicable", "why": "<=25 words"}}
- exhibits: the same defect/risk is present in this code
- avoided: the code covers the same surface and does NOT have the issue
- not_applicable: this implementation has no surface where the issue could occur""", key)
            if v is None:
                continue
            verdicts[k] = {"finding": (f.get("text") or "")[:160], "file": f.get("file"), **v}
            json.dump(verdicts, open(out_path, "w", encoding="utf-8"), indent=1)
        counts[verdicts[k].get("verdict", "not_applicable")] = counts.get(verdicts[k].get("verdict", "not_applicable"), 0) + 1
    print(f"RESULT {pr_id}/{arm}: {counts} of {len(findings)} findings")


if __name__ == "__main__":
    main()
