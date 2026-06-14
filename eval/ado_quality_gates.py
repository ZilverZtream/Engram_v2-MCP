"""Build the Stage-3 "what to avoid" corpus from OciusX Azure DevOps.

Pulls TEAM KNOWLEDGE that any developer has BEFORE starting work — NOT the
target PR's solution. Two sources, both normalized to the simple JSON shape
engram_index::quality_gates::parse_findings_json reads (message/description,
severity, file, rule, line):

  qg_board.json      <- "PR Feedback Learning" work items (recurring-issues board)
  qg_coderabbit.json <- historical CodeRabbit file-scoped findings

LEAKAGE GUARD: the eval target PRs (pilot_ids) are EXCLUDED from both sources,
so the corpus never contains findings about the very PRs we score against.
Findings on *other* PRs are fair team knowledge.

Pure GET. PAT from ADO_PAT env or eval/.secrets/ado_pat.txt. Nothing written to ADO.
"""
import argparse
import io
import json
import os
import re
import sys

import requests

# UTF-8 stdout (CodeRabbit comments contain emoji; cp1252 chokes).
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")

ORG = "patric0375"
PROJECT = "OciusX"
REPO = "OciusX"
API = "7.1"
BASE = f"https://dev.azure.com/{ORG}/{PROJECT}/_apis"

# The eval target PRs — their review findings would leak the target solution.
PILOT_PRS = {1937, 1941, 1961, 1965, 1967, 1908, 1925, 1917, 1933, 1913, 1920, 1974, 1977}


def _pat():
    pat = os.environ.get("ADO_PAT")
    if pat:
        return pat.strip()
    here = os.path.dirname(os.path.abspath(__file__))
    f = os.path.join(here, ".secrets", "ado_pat.txt")
    if os.path.exists(f):
        with open(f, encoding="utf-8") as fh:
            return fh.read().strip()
    print("ERROR: set ADO_PAT or create eval/.secrets/ado_pat.txt", file=sys.stderr)
    sys.exit(2)


SESS = requests.Session()
SESS.auth = ("", _pat())


def get(url, **params):
    params.setdefault("api-version", API)
    r = SESS.get(url, params=params, timeout=90)
    r.raise_for_status()
    return r.json()


def post(url, body, **params):
    params.setdefault("api-version", API)
    r = SESS.post(url, params=params, json=body, timeout=90)
    r.raise_for_status()
    return r.json()


def strip_html(s):
    if not s:
        return ""
    s = re.sub(r"<!--.*?-->", " ", s, flags=re.S)        # html comments
    s = re.sub(r"<details>.*?</details>", " ", s, flags=re.S)  # collapsed blocks
    s = re.sub(r"<br\s*/?>", "\n", s, flags=re.I)
    s = re.sub(r"</p>|</div>|</li>", "\n", s, flags=re.I)
    s = re.sub(r"<[^>]+>", " ", s)
    s = (s.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<")
           .replace("&gt;", ">").replace("&quot;", '"').replace("&#39;", "'"))
    s = re.sub(r"[ \t]+", " ", s)
    return "\n".join(ln.strip() for ln in s.splitlines() if ln.strip()).strip()


# ───────────────────────────── board ─────────────────────────────

def fetch_board(out_path):
    j = post(BASE + "/wit/wiql",
             {"query": "SELECT [System.Id] FROM WorkItems "
                       "WHERE [System.WorkItemType] = 'PR Feedback Learning'"})
    ids = [w["id"] for w in j.get("workItems", [])]
    findings = []
    skipped_pilot = 0
    for wid in ids:
        w = get(BASE + f"/wit/workitems/{wid}")
        f = w.get("fields", {})
        title = f.get("System.Title", "")
        desc_raw = f.get("System.Description", "") or ""
        # exclude board items that summarize a PILOT PR (leakage)
        linked = {int(m) for m in re.findall(r"/pullrequest/(\d+)", desc_raw)}
        if linked & PILOT_PRS:
            skipped_pilot += 1
            continue
        desc = strip_html(desc_raw)
        # The reviewer's actual issues are the ">"-quoted lines; the rest is
        # boilerplate ("Description is summary...", the PR link, "Discussion").
        quotes = []
        for ln in desc.splitlines():
            q = ln.lstrip("> ").replace("*", "").strip()  # drop quote + bold markdown
            low = q.lower()
            if not q or len(q) < 10:
                continue
            # skip reviewer-attribution lines ("Marcus Torvang :", "Dennis Östling:")
            if re.fullmatch(r"[A-Za-zÀ-ÿ.\- ]{3,40}\s*:", q):
                continue
            if (low.startswith("description is summary") or low.startswith("pull request")
                    or low.startswith("discussion") or "dev.azure.com" in low
                    or low in ("issues / risks found", "issues/risks found")):
                continue
            # skip reviewer-attribution lines ("Marcus Torvang :", "Dennis Östling:")
            if re.fullmatch(r"[A-Za-zÀ-ÿ.\- ]{3,40}\s*:?", q) and ":" in ln:
                continue
            quotes.append(q)
        clean_title = re.sub(r"^\[Address PR Comments\]\s*\+?", "", title).strip()
        clean_title = re.sub(r"^\+?\[(Feature|Bug|Fix|Chore)\]\s*", "", clean_title).strip()
        issues = " | ".join(dict.fromkeys(quotes))[:1100]  # dedup, keep order
        message = clean_title if not issues else f"{clean_title} — {issues}"
        if len(message) < 20:
            continue
        findings.append({
            "title": clean_title,
            "description": issues[:1100],
            "message": message[:1400],
            "severity": "medium",
            "source_pr": sorted(linked),
        })
    with open(out_path, "w", encoding="utf-8") as fh:
        json.dump(findings, fh, indent=2, ensure_ascii=False)
    print(f"board: {len(findings)} learning items -> {out_path} "
          f"({skipped_pilot} pilot-linked items excluded)")
    return findings


# ─────────────────────────── coderabbit ───────────────────────────

CR_SEV = [
    (re.compile(r"potential issue|critical|🔴|⚠️", re.I), "high"),
    (re.compile(r"refactor|🛠️|warning|🟡", re.I), "medium"),
    (re.compile(r"nitpick|🧹|🔵|minor", re.I), "low"),
]


def cr_severity(content):
    head = content[:200]
    for rx, sev in CR_SEV:
        if rx.search(head):
            return sev
    return "medium"


def clean_cr(content):
    """Reduce a CodeRabbit comment to its actionable core."""
    txt = strip_html(content)
    # drop the prompt-type decoration line(s) and AI-tooling boilerplate
    lines = []
    for ln in txt.splitlines():
        low = ln.lower()
        if any(b in low for b in (
            "committable suggestion", "prompt for ai agents", "🤖", "📝",
            "ai-generated", "auto-generated", "addressed in commit",
            "verify", "<details", "</details", "sourcemap")):
            continue
        lines.append(ln)
    txt = " ".join(lines)
    txt = re.sub(r"_[^_]{1,40}_\s*\|\s*_[^_]{1,40}_", "", txt)  # "_⚠️ Potential issue_ | _🔴 Critical_"
    txt = re.sub(r"\s+", " ", txt).strip()
    return txt


def fetch_coderabbit(out_path, max_prs):
    j = get(BASE + f"/git/repositories/{REPO}/pullrequests",
            **{"searchCriteria.status": "completed",
               "searchCriteria.targetRefName": "refs/heads/master",
               "$top": max_prs + len(PILOT_PRS) + 10})
    prs = [p["pullRequestId"] for p in j.get("value", []) if p["pullRequestId"] not in PILOT_PRS]
    prs = prs[:max_prs]
    findings = []
    seen = set()
    for pid in prs:
        try:
            th = get(BASE + f"/git/repositories/{REPO}/pullRequests/{pid}/threads")
        except Exception as e:
            print(f"  PR {pid} threads FAILED: {e}", file=sys.stderr)
            continue
        for t in th.get("value", []):
            tc = t.get("threadContext") or {}
            fpath = tc.get("filePath")
            line = ((tc.get("rightFileStart") or {}) or {}).get("line")
            # Only file-scoped threads carry findings; PR-level summary threads don't.
            if not fpath:
                continue
            # Take only the FIRST CodeRabbit comment in the thread — the finding.
            # Later CodeRabbit comments are replies/acks ("✅ addressed", "looks good").
            raw = None
            for c in t.get("comments", []):
                if (c.get("author") or {}).get("displayName") == "CodeRabbit":
                    raw = c.get("content", "") or ""
                    break
            if raw is None:
                continue
            if "walkthrough" in raw[:120].lower() or "actionable comments posted" in raw.lower():
                continue
            text = clean_cr(raw)
            if len(text) < 25:
                continue
            # Drop conversational acks / replies / bot errors that aren't findings.
            low = text.lower()
            if (low.startswith("@") or text[:2] in ("✅", "👍", "🎉")
                    or low.startswith("oops")
                    or "confirmed as addressed" in low or "addressed in commit" in low
                    or low.startswith("you're absolutely right")
                    or low.startswith("understood") or low.startswith("thanks")):
                continue
            # dedup near-identical findings (lowercased prefix)
            key = re.sub(r"[^a-z0-9 ]", "", low)[:90]
            if key in seen:
                continue
            seen.add(key)
            findings.append({
                "message": text[:600],
                "file": fpath.lstrip("/"),
                "line": line,
                "severity": cr_severity(raw),
                "source_pr": pid,
            })
    with open(out_path, "w", encoding="utf-8") as fh:
        json.dump(findings, fh, indent=2, ensure_ascii=False)
    by_sev = {}
    for f in findings:
        by_sev[f["severity"]] = by_sev.get(f["severity"], 0) + 1
    print(f"coderabbit: {len(findings)} deduped file-scoped findings from {len(prs)} non-pilot PRs "
          f"-> {out_path}  sev={by_sev}")
    return findings


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--board", default="eval/data/qg_board.json")
    ap.add_argument("--coderabbit", default="eval/data/qg_coderabbit.json")
    ap.add_argument("--max-prs", type=int, default=50)
    ap.add_argument("--only", choices=["board", "coderabbit"], help="fetch just one source")
    args = ap.parse_args()
    os.makedirs("eval/data", exist_ok=True)
    if args.only in (None, "board"):
        fetch_board(args.board)
    if args.only in (None, "coderabbit"):
        fetch_coderabbit(args.coderabbit, args.max_prs)
