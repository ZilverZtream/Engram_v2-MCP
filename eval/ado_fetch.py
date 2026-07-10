"""Read-only Azure DevOps fetcher for the Engram↔OciusX evaluation.

Pulls COMPLETED pull requests merged into main, each with its linked work
item (the user story), the changed-file set, and the pre-merge base commit
(so the eval can index OciusX as of BEFORE the PR — avoiding the leakage
where Engram's index already contains the answer).

NOTHING is written to OciusX. All calls are GET. The PAT is read from the
ADO_PAT env var or eval/.secrets/ado_pat.txt (gitignored); never hardcoded.

Usage:
  python eval/ado_fetch.py --pr 1906          # inspect one PR (sanity check)
  python eval/ado_fetch.py --top 50           # build dataset of N completed PRs
  python eval/ado_fetch.py --top 50 --out eval/data/ociusx_prs.json
"""
import argparse
import json
import os
import sys
import requests

ORG = "patric0375"
PROJECT = "OciusX"
REPO = "OciusX"
API = "7.1"
BASE = f"https://dev.azure.com/{ORG}/{PROJECT}/_apis"


def _pat():
    pat = os.environ.get("ADO_PAT")
    if pat:
        return pat.strip()
    here = os.path.dirname(os.path.abspath(__file__))
    f = os.path.join(here, ".secrets", "ado_pat.txt")
    if os.path.exists(f):
        with open(f, encoding="utf-8") as fh:
            return fh.read().strip()
    print("ERROR: set ADO_PAT env var or create eval/.secrets/ado_pat.txt", file=sys.stderr)
    sys.exit(2)


SESS = requests.Session()
SESS.auth = ("", _pat())
WITH_LINKED = False


def get(url, **params):
    params.setdefault("api-version", API)
    r = SESS.get(url, params=params, timeout=60)
    r.raise_for_status()
    return r.json()


def list_completed_prs(top):
    j = get(
        f"{BASE}/git/repositories/{REPO}/pullrequests",
        **{
            "searchCriteria.status": "completed",
            "searchCriteria.targetRefName": "refs/heads/master",
            "$top": top,
        },
    )
    return j.get("value", [])


def pr_workitems(pr_id):
    j = get(f"{BASE}/git/repositories/{REPO}/pullRequests/{pr_id}/workitems")
    return [w["id"] for w in j.get("value", [])]


def workitem(wid, with_linked=False):
    """with_linked: also pull the text of RELATED work items (support
    tickets, duplicates, parents) - dev-visible input the bare item's
    description under-represents. Live case: PR1937's merged fix covered a
    THREE-symptom support-ticket cluster while the item text carried one
    symptom; the missing 2/3 cost the eval agent the file-set match.
    Leak-safe: linked items were visible to the dev BEFORE implementing."""
    expand = {"$expand": "relations"} if with_linked else {}
    j = get(f"{BASE}/wit/workitems/{wid}", **expand)
    f = j.get("fields", {})
    out = {
        "id": wid,
        "type": f.get("System.WorkItemType", ""),
        "title": f.get("System.Title", ""),
        "description": _strip_html(f.get("System.Description", "")),
        "acceptance": _strip_html(f.get("Microsoft.VSTS.Common.AcceptanceCriteria", "")),
        "state": f.get("System.State", ""),
    }
    if with_linked:
        linked = []
        for rel in j.get("relations", []):
            if not rel.get("rel", "").startswith("System.LinkTypes"):
                continue
            url = rel.get("url", "")
            lid = url.rsplit("/", 1)[-1]
            if not lid.isdigit() or int(lid) == wid:
                continue
            try:
                lj = get(f"{BASE}/wit/workitems/{lid}")
                lf = lj.get("fields", {})
                text = _strip_html(lf.get("System.Description", ""))
                if text:
                    linked.append({
                        "id": int(lid),
                        "type": lf.get("System.WorkItemType", ""),
                        "title": lf.get("System.Title", ""),
                        "description": text[:1500],
                    })
            except Exception:
                continue  # missing/permission-restricted links are non-fatal
            if len(linked) >= 5:
                break
        if linked:
            out["linked_items"] = linked
    return out


def pr_changed_files(pr_id):
    """Canonical changed-file list = the last iteration's changes."""
    iters = get(
        f"{BASE}/git/repositories/{REPO}/pullRequests/{pr_id}/iterations"
    ).get("value", [])
    if not iters:
        return []
    last = iters[-1]["id"]
    ch = get(
        f"{BASE}/git/repositories/{REPO}/pullRequests/{pr_id}/iterations/{last}/changes"
    )
    out = []
    for c in ch.get("changeEntries", ch.get("value", [])):
        item = c.get("item", {})
        path = item.get("path")
        if path and not item.get("isFolder"):
            out.append({"path": path, "change": c.get("changeType", "")})
    return out


def _strip_html(s):
    if not s:
        return ""
    import re
    s = re.sub(r"<br\s*/?>", "\n", s, flags=re.I)
    s = re.sub(r"</p>|</div>|</li>", "\n", s, flags=re.I)
    s = re.sub(r"<li>", "- ", s, flags=re.I)
    s = re.sub(r"<[^>]+>", "", s)
    s = (s.replace("&nbsp;", " ").replace("&amp;", "&")
           .replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", '"'))
    return "\n".join(ln.rstrip() for ln in s.splitlines() if ln.strip()).strip()


def pr_record(pr):
    pr_id = pr["pullRequestId"]
    base = (pr.get("lastMergeTargetCommit") or {}).get("commitId", "")
    source = (pr.get("lastMergeSourceCommit") or {}).get("commitId", "")
    merge = (pr.get("lastMergeCommit") or {}).get("commitId", "")
    wids = pr_workitems(pr_id)
    stories = [workitem(w, with_linked=WITH_LINKED) for w in wids]
    files = pr_changed_files(pr_id)
    # ── HARD WALL ──────────────────────────────────────────────────────────
    # `story` is the ONLY thing a developer (and thus the model + Engram) gets.
    # Everything PR-side leaks the implementation and lives under `ground_truth`,
    # which the eval runner must NEVER pass to the model/Engram — scoring only.
    return {
        "pr_id": pr_id,
        "author": (pr.get("createdBy") or {}).get("displayName", ""),
        "closed_date": pr.get("closedDate", ""),
        "base_commit": base,        # master BEFORE this PR — index here (no leakage)
        # INPUT (developer-visible): the user story only.
        "story": {
            "work_items": stories,
            # convenience: the primary story's text the model is given.
            "title": stories[0]["title"] if stories else "",
            "description": stories[0]["description"] if stories else "",
            "acceptance": stories[0]["acceptance"] if stories else "",
        },
        # SCORING ONLY — withheld from the model/Engram.
        "ground_truth": {
            "pr_title": pr.get("title", ""),
            "pr_description": _strip_html(pr.get("description", "")),
            "source_commit": source,
            "merge_commit": merge,
            "changed_files": files,
        },
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pr", type=int, help="inspect a single PR id")
    ap.add_argument("--top", type=int, default=50, help="number of completed PRs")
    ap.add_argument("--author", default="", help="only keep PRs whose author contains this (e.g. Torvang)")
    ap.add_argument("--out", default="eval/data/ociusx_prs.json")
    ap.add_argument("--with-linked", action="store_true",
                    help="also pull linked work items' text (support tickets) into each story - input parity for cluster-fix PRs; do NOT mix corpora built with and without this flag in one campaign")
    args = ap.parse_args()
    global WITH_LINKED
    WITH_LINKED = args.with_linked

    if args.pr:
        pr = get(f"{BASE}/git/repositories/{REPO}/pullRequests/{args.pr}")
        rec = pr_record(pr)
        print(json.dumps(rec, indent=2)[:4000])
        return

    prs = list_completed_prs(args.top)
    if args.author:
        prs = [p for p in prs
               if args.author.lower() in ((p.get("createdBy") or {}).get("displayName", "").lower())]
    print(f"fetched {len(prs)} completed PRs"
          f"{f' by ~{args.author}' if args.author else ''}; resolving work items + changes ...")
    recs = []
    for i, pr in enumerate(prs, 1):
        try:
            rec = pr_record(pr)
            recs.append(rec)
            witems = rec["story"]["work_items"]
            wi = witems[0]["id"] if witems else "-"
            print(f"  [{i}/{len(prs)}] PR {rec['pr_id']} ({rec['author']}) -> WI {wi} "
                  f"({len(rec['ground_truth']['changed_files'])} files)")
        except Exception as e:
            print(f"  [{i}] PR {pr.get('pullRequestId')} FAILED: {e}")
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(recs, fh, indent=2)
    linked = sum(1 for r in recs if r["story"]["work_items"])
    print(f"\nwrote {len(recs)} PRs to {args.out} ({linked} have a linked work item / US)")


if __name__ == "__main__":
    main()
