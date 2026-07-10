"""Replay-calibrate pre_commit_review against a PR's REAL review findings.

The iteration-tax KPI: OciusX PRs bounce 3-6 times through CodeRabbit/
SonarQube before merge (PR1955: 6 iterations). If pre_commit_review's
gates catch a finding BEFORE push, that round-trip disappears. This
script measures the pre-catch fraction on real history:

  1. Take the PR's ITERATION 1 diff (the pre-review state) — requires the
     iteration-1 source commit to exist in the local clone (additive PRs;
     force-pushed/rebased PRs lose it server-side, ADO refuses SHA fetch).
  2. Collect the PR's real CodeRabbit findings (eval/data/qg_findings_raw.json,
     built by ado_findings_all.py).
  3. Run pre_commit_review (all 16 gates) on that exact diff.
  4. Report side-by-side per file: what the reviewer said vs what the gates
     said. File-level pre-catch fraction is the automatic number; the
     mechanism-level judgment (same ISSUE, not just same file) is printed
     for eyeball/LLM classification.

Caveat: gates read companion evidence from the LIVE tree, not the PR-era
tree — drift is possible on old PRs; prefer recent ones.

Usage:
  python eval/replay_calibrate.py --pr 1955
  python eval/replay_calibrate.py --scan          # list replayable PRs w/ findings
"""
import argparse
import io
import json
import os
import subprocess
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OCIUSX = r"C:\Users\Dennis\source\repos\OciusX"
PROJECT_ID = "664003e4-2ac5-4902-a0ce-6382b6026fe5"
RAW_FINDINGS = os.path.join(HERE, "data", "qg_findings_raw.json")

sys.path.insert(0, HERE)
import importlib.util

spec = importlib.util.spec_from_file_location("ado_fetch", os.path.join(HERE, "ado_fetch.py"))
ado = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ado)


def git(*args):
    r = subprocess.run(["git", "-C", OCIUSX, *args], capture_output=True,
                       text=True, encoding="utf-8", errors="replace")
    return r.returncode, r.stdout


def commit_exists(sha):
    code, _ = git("cat-file", "-e", f"{sha}^{{commit}}")
    return code == 0


def iteration1(pr_id):
    j = ado.get(f"{ado.BASE}/git/repositories/{ado.REPO}/pullRequests/{pr_id}/iterations")
    its = j.get("value", [])
    if not its:
        return None
    it1 = its[0]
    src = (it1.get("sourceRefCommit") or {}).get("commitId", "")
    base = ((it1.get("commonRefCommit") or {}) or (it1.get("targetRefCommit") or {})).get("commitId", "")
    return {"src": src, "base": base, "n_iterations": len(its)}


def real_findings(pr_id):
    """File-scoped review findings straight from the PR's threads.

    qg_findings_raw.json (ado_findings_all.py) deliberately drops file
    paths — it was built for rule distillation, and replaying against it
    produced a fake 0% pre-catch. The threads API carries
    threadContext.filePath + rightFileStart.line, which is what
    file-level matching needs.
    """
    j = ado.get(f"{ado.BASE}/git/repositories/{ado.REPO}/pullRequests/{pr_id}/threads")
    out = []
    for th in j.get("value", []):
        tc = th.get("threadContext") or {}
        fp = tc.get("filePath", "")
        if not fp:
            continue  # PR-level meta threads (summaries, walkthroughs)
        comments = [c for c in th.get("comments", []) if c.get("commentType") != "system"]
        if not comments:
            continue
        first = comments[0]
        text = (first.get("content") or "").strip()
        if not text:
            continue
        out.append({
            "file": fp,
            "line": (tc.get("rightFileStart") or {}).get("line"),
            "text": text[:600],
            "status": th.get("status", ""),
            "author": (first.get("author") or {}).get("displayName", ""),
        })
    return out


def canon_base(p):
    return (p or "").replace("\\", "/").lower().rstrip("/").split("/")[-1]


def run_review(diff_path):
    args = {"project_id": PROJECT_ID, "diff": diff_path,
            "max_findings": 60, "output_json": True}
    r = subprocess.run(
        [sys.executable, os.path.join(ROOT, "target", "engram_drive.py"),
         "tool", "pre_commit_review", json.dumps(args), "60000"],
        capture_output=True, text=True, timeout=900, encoding="utf-8", errors="replace",
    )
    out = r.stdout
    # engram_drive prints the tool's text content; the JSON payload is that text.
    start = out.find("{")
    if start < 0:
        raise RuntimeError(f"no JSON in review output: {out[:400]}")
    return json.loads(out[start:out.rfind("}") + 1])


def scan(top=60):
    d = json.load(open(RAW_FINDINGS, encoding="utf-8"))
    import collections
    by_pr = collections.Counter(f["pr_id"] for f in d)
    print("PR    findings  iter1-local  iterations")
    for pr_id, n in sorted(by_pr.items(), reverse=True)[:top]:
        it = iteration1(pr_id)
        if not it:
            continue
        ok = commit_exists(it["src"]) and commit_exists(it["base"])
        print(f"{pr_id}  {n:8d}  {'YES' if ok else 'no ':>10}  {it['n_iterations']}")


def replay(pr_id):
    it = iteration1(pr_id)
    if not it:
        print(f"PR{pr_id}: no iterations found")
        return 2
    if not (commit_exists(it["src"]) and commit_exists(it["base"])):
        print(f"PR{pr_id}: iteration-1 commits not in local clone (force-pushed?) — pick another PR (--scan)")
        return 2
    code, diff_text = git("diff", it["base"], it["src"])
    if code != 0 or not diff_text.strip():
        print(f"PR{pr_id}: empty diff")
        return 2
    diff_path = os.path.join(HERE, "data", "p2", f"replay_pr{pr_id}_iter1.patch")
    io.open(diff_path, "w", encoding="utf-8", newline="\n").write(diff_text)
    reals = real_findings(pr_id)
    print(f"PR{pr_id}: iter1 diff {len(diff_text) // 1024}KB, {it['n_iterations']} iterations, "
          f"{len(reals)} real review findings")

    review = run_review(diff_path)
    gate_findings = review.get("findings", [])
    print(f"pre_commit_review: {len(gate_findings)} gate findings")

    gate_by_file = {}
    for g in gate_findings:
        gate_by_file.setdefault(canon_base(g.get("file_path")), []).append(g)

    caught, missed = [], []
    for f in reals:
        fb = canon_base(f.get("file"))
        hits = gate_by_file.get(fb, [])
        (caught if hits else missed).append((f, hits))

    print(f"\n== file-level pre-catch: {len(caught)}/{len(reals)} ==\n")
    for f, hits in caught:
        print(f"REAL [{f.get('rule_key') or f.get('severity') or '?'}] {canon_base(f.get('file'))}: "
              f"{(f.get('text') or '')[:140]}")
        for g in hits:
            print(f"   GATE {g.get('gate')}/{g.get('severity')}: {(g.get('title') or '')[:120]}")
        print()
    print("== NOT pre-caught (the gap list — each is a candidate gate/rule) ==\n")
    for f, _ in missed:
        print(f"MISS [{f.get('rule_key') or f.get('severity') or '?'}] {canon_base(f.get('file'))}: "
              f"{(f.get('text') or '')[:160]}")
    out_path = os.path.join(HERE, "data", "p2", f"replay_pr{pr_id}.json")
    json.dump({"pr_id": pr_id, "iterations": it["n_iterations"],
               "real_findings": len(reals), "file_level_caught": len(caught),
               "caught": [{"real": f, "gates": h} for f, h in caught],
               "missed": [f for f, _ in missed],
               "gate_findings_total": len(gate_findings),
               "gate_findings": gate_findings},
              open(out_path, "w", encoding="utf-8"), indent=1)
    print(f"\nwrote {out_path}")
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pr", type=int)
    ap.add_argument("--scan", action="store_true")
    args = ap.parse_args()
    if args.scan:
        scan()
        return
    if not args.pr:
        ap.error("--pr or --scan required")
    sys.exit(replay(args.pr))


if __name__ == "__main__":
    main()
