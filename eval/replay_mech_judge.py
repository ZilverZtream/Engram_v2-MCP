"""Mechanism-level judging of replay calibration matches.

File-level pre-catch (replay_calibrate.py) is an UPPER bound: a temporal
gate warning about a coupled resx file "matches" a reviewer's null-check
finding on the same file without describing the same issue. For every
(real finding, same-file gate findings) pair this asks a free LLM judge:
does any gate finding describe the SAME underlying issue the reviewer
raised? Produces the honest mechanism pre-catch fraction plus per-gate
true-catch counts.

Usage:
  python eval/replay_mech_judge.py            # all replay_pr*.json
  python eval/replay_mech_judge.py 1939       # one PR
Judgments cached in eval/data/p2/replay_mech_cache.json (keyed by
pr/file/text-hash) so reruns are free.
"""
import glob
import hashlib
import json
import os
import re
import sys
import time
import urllib.request

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG = r"C:\Users\Dennis\AppData\Roaming\engram\engram\config\engram_mcp.yaml"
MODEL = os.environ.get("REPLAY_JUDGE_MODEL", "tencent/hy3:free")
CACHE_PATH = os.path.join(HERE, "data", "p2", "replay_mech_cache.json")


def api_key():
    for line in open(CONFIG, encoding="utf-8"):
        m = re.match(r"\s*llm_openai_api_key:\s*[\"']?([^\"'\s]+)", line)
        if m:
            return m.group(1)
    raise SystemExit("no OpenRouter key in engram config")


KEY = api_key()


def ask(prompt, retries=4):
    body = json.dumps({
        "model": MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0,
    }).encode()
    for i in range(retries):
        try:
            req = urllib.request.Request(
                "https://openrouter.ai/api/v1/chat/completions", data=body,
                headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=120) as r:
                j = json.load(r)
            txt = j["choices"][0]["message"]["content"]
            m = re.search(r"\{.*\}", txt, re.S)
            if m:
                return json.loads(m.group(0))
        except Exception as e:
            if i == retries - 1:
                print(f"  judge error (final): {e}", file=sys.stderr)
            time.sleep(3 * (i + 1))
    return None


def judge_pair(real, gates):
    gate_lines = "\n".join(
        f"- [{g.get('gate')}/{g.get('severity')}] {g.get('title','')}: {(g.get('detail') or '')[:200]}"
        for g in gates[:12])
    prompt = f"""A human/CodeRabbit reviewer raised this finding on a pull request file:

FILE: {real.get('file')}
REVIEWER FINDING: {(real.get('text') or '')[:500]}

An automated pre-push audit produced these findings on the SAME file (before the reviewer ever saw the code):

{gate_lines}

Question: does ANY audit finding above describe the SAME underlying issue the reviewer raised (same defect/risk, not merely the same file)? Near-equivalent phrasing counts; a different issue on the same file does NOT.

Answer strict JSON only: {{"same_issue": true/false, "matching_gate": "<gate name or null>", "rationale": "<=30 words"}}"""
    return ask(prompt)


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else None
    cache = {}
    if os.path.exists(CACHE_PATH):
        cache = json.load(open(CACHE_PATH, encoding="utf-8"))
    files = sorted(glob.glob(os.path.join(HERE, "data", "p2", "replay_pr*.json")))
    total_pairs, mech_caught = 0, 0
    per_gate = {}
    total_real = 0
    for fp in files:
        d = json.load(open(fp, encoding="utf-8"))
        pr = d["pr_id"]
        if only and str(pr) != only:
            continue
        total_real += d["real_findings"]
        for c in d.get("caught", []):
            real, gates = c["real"], c.get("gates", [])
            key = hashlib.sha1(
                f"{pr}|{real.get('file')}|{real.get('text','')[:200]}".encode()).hexdigest()
            if key not in cache:
                v = judge_pair(real, gates)
                if v is None:
                    continue
                cache[key] = v
                json.dump(cache, open(CACHE_PATH, "w", encoding="utf-8"), indent=0)
            v = cache[key]
            total_pairs += 1
            if v.get("same_issue"):
                mech_caught += 1
                g = v.get("matching_gate") or "?"
                per_gate[g] = per_gate.get(g, 0) + 1
        print(f"PR{pr}: judged (cumulative mech-caught {mech_caught}/{total_pairs})", flush=True)

    print(f"\nfile-level candidates judged: {total_pairs}")
    print(f"MECHANISM pre-catch: {mech_caught}/{total_real} real findings = "
          f"{100 * mech_caught / total_real:.1f}%  (of candidates: {100 * mech_caught / max(total_pairs, 1):.1f}%)")
    print("\ntrue catches by gate:")
    for g, n in sorted(per_gate.items(), key=lambda x: -x[1]):
        print(f"{n:4d}  {g}")


if __name__ == "__main__":
    main()
