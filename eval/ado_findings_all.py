"""Pull the ENTIRE CodeRabbit/Sonar review history from OciusX DevOps, raw, for
DISTILLATION into generic project rules (NOT file/line lookup).

Walks every completed PR (paginated), takes the first CodeRabbit comment in each
file-scoped thread (the finding), and captures the raw signal a generalizer needs:
  text, rule_key (embedded Sonar S-keys / CR category), severity, lang, pr_id, file

No dedup here — clustering + generalization happens downstream (an LLM distills
the thousands of findings into a compact, deduplicated, GENERIC ruleset that
applies to ANY change, regardless of which file once tripped it).

Pure GET. PAT from ADO_PAT env or eval/.secrets/ado_pat.txt. Nothing written to ADO.
Output: eval/data/qg_findings_raw.json
"""
import argparse
import io
import json
import os
import re
import sys

import requests

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")

ORG = "patric0375"
PROJECT = "OciusX"
REPO = "OciusX"
API = "7.1"
BASE = f"https://dev.azure.com/{ORG}/{PROJECT}/_apis"
# Exclude the eval target PRs (defence-in-depth; generic rules aren't leakage, but
# never let a pilot PR's file/line finding into the raw set either).
PILOT_PRS = {1937, 1941, 1961, 1965, 1967, 1908, 1925, 1917, 1933, 1913, 1920, 1974, 1977}

SONAR_RE = re.compile(r"\b([a-z]+:S\d{3,5}|S\d{3,5})\b")
# SonarCloud PR-decoration comments (posted under a user identity, not CodeRabbit):
# they carry a rule key in parens — e.g. "(Web:ImgWithoutAltCheck)", "(javascript:S6582)",
# "(csharpsquid:S1118)" — and link "See it in SonarQube Cloud".
SONAR_SIG = re.compile(r"sonarqube|sonarcloud|sonar\.?cloud", re.I)
SONAR_KEY = re.compile(r"\(([A-Za-z][\w.]*:[A-Za-z0-9_]+)\)")


def _pat():
    pat = os.environ.get("ADO_PAT")
    if pat:
        return pat.strip()
    f = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".secrets", "ado_pat.txt")
    if os.path.exists(f):
        return open(f, encoding="utf-8").read().strip()
    print("ERROR: set ADO_PAT or create eval/.secrets/ado_pat.txt", file=sys.stderr)
    sys.exit(2)


SESS = requests.Session()
SESS.auth = ("", _pat())


def get(url, **params):
    params.setdefault("api-version", API)
    r = SESS.get(url, params=params, timeout=120)
    r.raise_for_status()
    return r.json()


def strip_html(s):
    if not s:
        return ""
    s = re.sub(r"<!--.*?-->", " ", s, flags=re.S)
    s = re.sub(r"<details>.*?</details>", " ", s, flags=re.S)
    s = re.sub(r"<br\s*/?>", "\n", s, flags=re.I)
    s = re.sub(r"</p>|</div>|</li>", "\n", s, flags=re.I)
    s = re.sub(r"<[^>]+>", " ", s)
    s = (s.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<")
           .replace("&gt;", ">").replace("&quot;", '"').replace("&#39;", "'"))
    s = re.sub(r"[ \t]+", " ", s)
    return "\n".join(ln.strip() for ln in s.splitlines() if ln.strip()).strip()


def clean_cr(content):
    txt = strip_html(content)
    lines = []
    for ln in txt.splitlines():
        low = ln.lower()
        if any(b in low for b in (
            "committable suggestion", "prompt for ai agents", "🤖", "📝",
            "ai-generated", "auto-generated", "addressed in commit", "sourcemap")):
            continue
        lines.append(ln)
    txt = " ".join(lines)
    txt = re.sub(r"_[^_]{1,40}_\s*\|\s*_[^_]{1,40}_", "", txt)
    return re.sub(r"\s+", " ", txt).strip()


def severity_of(raw):
    head = raw[:200].lower()
    if any(k in head for k in ("potential issue", "critical", "🔴", "⚠️")):
        return "high"
    if any(k in head for k in ("refactor", "🛠️", "warning", "🟡")):
        return "medium"
    if any(k in head for k in ("nitpick", "🧹", "🔵", "minor")):
        return "low"
    return "medium"


EXT_LANG = {".vb": "vbnet", ".cs": "csharp", ".ts": "typescript", ".tsx": "typescript",
            ".js": "javascript", ".jsx": "javascript", ".sql": "sql", ".aspx": "webforms",
            ".ascx": "webforms", ".vbhtml": "razor", ".css": "css", ".resx": "resx",
            ".master": "webforms", ".config": "config"}


def lang_of(fpath):
    fl = (fpath or "").lower()
    for ext, lang in EXT_LANG.items():
        if fl.endswith(ext):
            return lang
    return "other"


def list_all_completed_prs(max_total):
    prs, skip = [], 0
    while len(prs) < max_total:
        batch = get(f"{BASE}/git/repositories/{REPO}/pullrequests",
                    **{"searchCriteria.status": "completed",
                       "searchCriteria.targetRefName": "refs/heads/master",
                       "$top": 100, "$skip": skip}).get("value", [])
        if not batch:
            break
        prs.extend(batch)
        skip += 100
        print(f"  ...{len(prs)} completed PRs listed", flush=True)
    return [p["pullRequestId"] for p in prs if p["pullRequestId"] not in PILOT_PRS]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="eval/data/qg_findings_raw.json")
    ap.add_argument("--max-prs", type=int, default=600)
    args = ap.parse_args()

    pr_ids = list_all_completed_prs(args.max_prs)
    print(f"pulling CodeRabbit findings from {len(pr_ids)} non-pilot completed PRs ...", flush=True)
    findings = []
    for i, pid in enumerate(pr_ids, 1):
        try:
            th = get(f"{BASE}/git/repositories/{REPO}/pullRequests/{pid}/threads")
        except Exception as e:
            print(f"  PR {pid} threads FAILED: {e}", file=sys.stderr)
            continue
        n_pr = 0
        for t in th.get("value", []):
            tc = t.get("threadContext") or {}
            fpath = tc.get("filePath")
            if not fpath:
                continue
            # Resolution status travels with each finding (do NOT drop Won't-fix
            # threads — that is where most SonarQube findings live, and many are
            # legit). The distiller uses it: fixed/active/closed -> extract as a
            # rule; wontFix/byDesign -> the team REJECTED it (often an analyzer
            # modern-syntax suggestion that breaks the ES5/WebGrease bundle), so it
            # is NOT a "do this" rule and instead reinforces the ES5 override.
            status = (t.get("status") or "").lower()
            comments = t.get("comments", [])
            # (a) CodeRabbit finding = first CodeRabbit-authored comment in the thread.
            cr_raw = None
            for c in comments:
                if (c.get("author") or {}).get("displayName") == "CodeRabbit":
                    cr_raw = c.get("content", "") or ""
                    break
            if cr_raw is not None and not (
                "walkthrough" in cr_raw[:120].lower()
                or "actionable comments posted" in cr_raw.lower()
            ):
                text = clean_cr(cr_raw)
                low = text.lower()
                if len(text) >= 25 and not (
                    low.startswith("@") or text[:2] in ("✅", "👍", "🎉")
                    or low.startswith("oops") or "confirmed as addressed" in low
                    or low.startswith("you're absolutely right") or low.startswith("understood")
                    or low.startswith("thanks") or low.startswith("good catch")
                ):
                    sonar = SONAR_RE.findall(cr_raw) or SONAR_RE.findall(text)
                    findings.append({
                        "text": text[:700], "source": "coderabbit",
                        "sonar_rule": sonar[0] if sonar else None,
                        "severity": severity_of(cr_raw), "lang": lang_of(fpath), "pr_id": pid, "resolution": status,
                    })
                    n_pr += 1
            # (b) SonarQube findings = ANY comment carrying a Sonar rule key /
            #     "SonarQube Cloud" link (posted under a user identity, not CodeRabbit).
            for c in comments:
                if (c.get("author") or {}).get("displayName") == "CodeRabbit":
                    continue
                raw = c.get("content", "") or ""
                key_m = SONAR_KEY.search(raw)
                if not (key_m or SONAR_SIG.search(raw)):
                    continue
                text = strip_html(raw)
                text = re.sub(r"\s+", " ", text).strip()
                if len(text) < 12:
                    continue
                findings.append({
                    "text": text[:700], "source": "sonar",
                    "sonar_rule": key_m.group(1) if key_m else None,
                    "severity": "medium", "lang": lang_of(fpath), "pr_id": pid, "resolution": status,
                })
                n_pr += 1
        if i % 25 == 0 or n_pr:
            print(f"  [{i}/{len(pr_ids)}] PR {pid}: +{n_pr}  (total {len(findings)})", flush=True)

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(findings, fh, indent=2, ensure_ascii=False)
    by_lang, by_src, n_key = {}, {}, 0
    for f in findings:
        by_lang[f["lang"]] = by_lang.get(f["lang"], 0) + 1
        by_src[f.get("source", "?")] = by_src.get(f.get("source", "?"), 0) + 1
        n_key += 1 if f.get("sonar_rule") else 0
    print(f"\nwrote {len(findings)} raw findings -> {args.out}")
    print(f"  by source: {by_src}")
    print(f"  by lang: {dict(sorted(by_lang.items(), key=lambda x:-x[1]))}")
    print(f"  with sonar rule-key: {n_key}")


if __name__ == "__main__":
    main()
