"""Read-only discovery of OciusX Azure DevOps quality-gate sources.

Probes what's available to build the Stage-3 "what to avoid" corpus:
  - work-item types + counts (the recurring-issues board)
  - distinct PR-thread comment authors (to spot CodeRabbit/SonarQube bots)
  - any SonarQube service connection / build tags

Pure GET. PAT from ADO_PAT env or eval/.secrets/ado_pat.txt. Nothing written to ADO.
"""
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
    print("ERROR: set ADO_PAT or create eval/.secrets/ado_pat.txt", file=sys.stderr)
    sys.exit(2)


SESS = requests.Session()
SESS.auth = ("", _pat())


def get(url, **params):
    params.setdefault("api-version", API)
    r = SESS.get(url, params=params, timeout=60)
    r.raise_for_status()
    return r.json()


def post(url, body, **params):
    params.setdefault("api-version", API)
    r = SESS.post(url, params=params, json=body, timeout=60)
    r.raise_for_status()
    return r.json()


def probe_connectivity():
    print("=== connectivity ===")
    try:
        j = get(f"https://dev.azure.com/{ORG}/_apis/projects/{PROJECT}")
        print(f"  OK  project={j.get('name')} id={j.get('id')[:8]} state={j.get('state')}")
        return True
    except Exception as e:
        print(f"  FAIL {e}")
        return False


def probe_workitem_types():
    print("=== work-item types ===")
    try:
        j = get(f"{BASE}/wit/workitemtypes")
        for t in j.get("value", []):
            print(f"  - {t.get('name')}")
    except Exception as e:
        print(f"  FAIL {e}")


def probe_workitem_counts():
    print("=== work-item counts by type (WIQL) ===")
    for wtype in ["Bug", "Issue", "Task", "User Story", "Product Backlog Item", "Impediment"]:
        body = {"query": f"SELECT [System.Id] FROM WorkItems WHERE [System.WorkItemType] = '{wtype}'"}
        try:
            j = post(f"{BASE}/wit/wiql", body)
            print(f"  {wtype:24s} {len(j.get('workItems', []))}")
        except Exception as e:
            print(f"  {wtype:24s} ERR {str(e)[:80]}")


def probe_tags():
    print("=== distinct tags on work items (sample 200) ===")
    body = {"query": "SELECT [System.Id], [System.Tags] FROM WorkItems"}
    try:
        j = post(f"{BASE}/wit/wiql", body)
        ids = [w["id"] for w in j.get("workItems", [])][:200]
        if not ids:
            print("  (none)")
            return
        tags = {}
        # batch
        for i in range(0, len(ids), 200):
            chunk = ids[i:i + 200]
            jj = get(f"{BASE}/wit/workitems", ids=",".join(map(str, chunk)),
                     fields="System.Tags")
            for w in jj.get("value", []):
                t = (w.get("fields", {}) or {}).get("System.Tags", "")
                for tag in [x.strip() for x in t.split(";") if x.strip()]:
                    tags[tag] = tags.get(tag, 0) + 1
        for tag, n in sorted(tags.items(), key=lambda x: -x[1]):
            print(f"  {n:4d}  {tag}")
    except Exception as e:
        print(f"  FAIL {e}")


def probe_pr_comment_authors(sample_prs=25):
    print(f"=== PR-thread comment authors (last {sample_prs} completed PRs) ===")
    try:
        j = get(f"{BASE}/git/repositories/{REPO}/pullrequests",
                **{"searchCriteria.status": "completed",
                   "searchCriteria.targetRefName": "refs/heads/master",
                   "$top": sample_prs})
        prs = [p["pullRequestId"] for p in j.get("value", [])]
        authors = {}
        for pid in prs:
            try:
                th = get(f"{BASE}/git/repositories/{REPO}/pullRequests/{pid}/threads")
                for t in th.get("value", []):
                    for c in t.get("comments", []):
                        a = (c.get("author") or {}).get("displayName", "?")
                        authors[a] = authors.get(a, 0) + 1
            except Exception:
                pass
        for a, n in sorted(authors.items(), key=lambda x: -x[1]):
            print(f"  {n:5d}  {a}")
    except Exception as e:
        print(f"  FAIL {e}")


def probe_service_connections():
    print("=== service endpoints (look for SonarQube) ===")
    try:
        j = get(f"https://dev.azure.com/{ORG}/{PROJECT}/_apis/serviceendpoint/endpoints")
        for e in j.get("value", []):
            print(f"  - {e.get('name')}  type={e.get('type')}")
    except Exception as e:
        print(f"  FAIL {e}")


if __name__ == "__main__":
    if not probe_connectivity():
        sys.exit(1)
    probe_workitem_types()
    probe_workitem_counts()
    probe_tags()
    probe_pr_comment_authors()
    probe_service_connections()
