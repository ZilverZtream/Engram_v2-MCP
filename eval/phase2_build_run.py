"""Build a self-contained Phase-2b workflow run script with story data baked in
(the `args` global does not reliably reach scriptPath runs). For each PR: read the
manifest, write the real merged diff to a file (judge reads it), assemble the
STORIES entry, then inject STORIES into phase2_workflow.js -> eval/data/p2/_run.js.

Usage: python eval/phase2_build_run.py 1933 1908 1967 1937 1974 [--out _run_all.js]
"""
import argparse
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
P2 = os.path.join(HERE, "data", "p2")
REPO = r"C:\Users\Dennis\source\repos\OciusX"
TEMPLATE = os.path.join(HERE, "phase2_workflow.js")


def story_entry(pr):
    m = json.load(open(os.path.join(P2, f"pr{pr}.json"), encoding="utf-8"))
    base = m["base_commit"]
    merge = m["ground_truth"]["merge_commit"]
    mods = [cf["path"] for cf in m["ground_truth"]["changed_files"]
            if "add" not in (cf.get("change", "") or "").lower()
            and "rename" not in (cf.get("change", "") or "").lower()]
    diff = subprocess.run(["git", "-C", REPO, "diff", base, merge],
                          capture_output=True, text=True, encoding="utf-8",
                          errors="replace").stdout
    # Cap so the judge's Read stays focused; recall uses the full modified_files
    # list (passed separately), so a truncated diff only affects impl_score detail.
    CAP = 80000
    if len(diff) > CAP:
        diff = diff[:CAP] + f"\n\n... [diff truncated at {CAP} chars of {len(diff)} total]\n"
    diff_path = os.path.join(P2, f"pr{pr}_merged.diff")
    open(diff_path, "w", encoding="utf-8").write(diff)
    rich = os.path.join(P2, f"pr{pr}_dossier_rich.md")
    pat = os.path.join(P2, f"pr{pr}_dossier_pattern.md")
    return {
        "pr_id": m["pr_id"],
        "title": m["story"]["title"],
        "description": m["story"].get("description", ""),
        "acceptance": m["story"].get("acceptance", ""),
        "worktree": m["worktree_engram"],
        "worktree_alone": m["worktree_alone"],
        "dossier_path": m["dossier_path"],
        "dossier_rich_path": rich if os.path.exists(rich) else m["dossier_path"],
        "dossier_pattern_path": pat if os.path.exists(pat) else m["dossier_path"],
        "modified_files": mods,
        "merged_diff_path": diff_path,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("prs", nargs="+", type=int)
    ap.add_argument("--out", default="_run_all.js")
    ap.add_argument("--template", default=TEMPLATE,
                    help="workflow template .js (default 2-arm phase2_workflow.js)")
    args = ap.parse_args()
    stories = [story_entry(pr) for pr in args.prs]
    tpl = open(args.template, encoding="utf-8").read()
    marker = "let STORIES = null // INJECTED_STORIES"
    assert marker in tpl, "injection marker not found in template"
    out = tpl.replace(marker, "let STORIES = " + json.dumps(stories) + " // INJECTED_STORIES")
    out_path = os.path.join(P2, args.out)
    open(out_path, "w", encoding="utf-8").write(out)
    for s in stories:
        print(f"  PR {s['pr_id']}: {len(s['modified_files'])} modified files, "
              f"diff {os.path.getsize(s['merged_diff_path'])}B — {s['title'][:45]}")
    print(f"wrote {out_path} with {len(stories)} stories")


if __name__ == "__main__":
    main()
