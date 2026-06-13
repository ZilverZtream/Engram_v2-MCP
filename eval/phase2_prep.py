"""Phase 2a — prepare a story for the Model vs Model+Engram A/B.

For one PR: index OciusX at base_commit, run the validated 3-arm ensemble
(concept S3 + history S7 + graph S6), and emit:
  - eval/data/p2/pr<id>_dossier.md   : the Engram context the Model+Engram agent gets
  - eval/data/p2/pr<id>.json         : manifest (story, worktree paths, ground_truth for JUDGE only)
  - two detached OciusX worktrees at base_commit for the agents to edit:
      %TEMP%/engram_p2_wt/pr<id>_alone   (Model-alone arm)
      %TEMP%/engram_p2_wt/pr<id>_engram  (Model+Engram arm)

US-only / read-only: the dossier is built from the user story alone; ground_truth
(the merged PR) is written ONLY to the manifest for the judge, never to a dossier.
"""
import argparse
import json
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram, add_worktree, remove_worktree, WORKTREE_ROOT  # noqa: E402
import run_phase1 as rp  # noqa: E402

P2_WT_ROOT = os.path.join(tempfile.gettempdir(), "engram_p2_wt")
P2_DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "p2")

# The validated ensemble arms (name -> strategy fn) and a short provenance label.
ARMS = [
    ("concept", rp.s3_full),
    ("history", rp.s7_history_expansion),
    ("graph", rp.s6_node_traversal),
]


_LAYERS = [
    ("Server (VB / code-behind / markup)", (".vb", ".cs", ".aspx", ".ascx",
        ".master", ".asmx", ".ashx", ".svc", ".asax")),
    ("Client (TypeScript / JavaScript)", (".ts", ".tsx", ".js", ".jsx")),
    ("Resources (.resx — translate EVERY language)", (".resx",)),
    ("Data (SQL)", (".sql",)),
    ("Markup / styles / config", (".html", ".css", ".config", ".vbhtml", ".cshtml")),
]


def _layer_of(path):
    pl = path.lower()
    for name, exts in _LAYERS:
        if pl.endswith(exts):
            return name
    return "Other"


def render_dossier(title, rows, cap=40):
    """rows = list of (path, signals_set). Render a FLAT, layer-grouped checklist
    (no rank numbers) with strong anti-anchoring framing: this is a non-exhaustive
    starting map, implement EVERY layer, follow companions, don't stop early.
    (Ranked numbering anchored agents to the top file and suppressed exploration.)"""
    rows = sorted(rows, key=lambda r: (-len(r[1]), r[0]))[:cap]
    md = [
        "# Engram analysis — candidate touchpoints",
        "",
        "Engram cross-referenced this user story against the codebase using THREE "
        "independent signals: concept-footprint (semantic + lexical), git co-change "
        "history, and the structural call/contains graph.",
        "",
        f"**Story:** {title}",
        "",
        "## How to use this (read first)",
        "- This is a **starting map, NOT a complete or prioritized answer.** A real "
        "change in this app usually spans MULTIPLE layers at once: server code-behind "
        "(`.aspx.vb`), client TypeScript/JavaScript (`.ts`/`.js`, **including compiled "
        "bundles**), resource files (`.resx` in every language), SQL scripts, and "
        "settings. Implement EVERY layer the story needs.",
        "- **Do NOT stop at the first or top file.** Verify each candidate against the "
        "real code, follow the change through all affected layers, and **ADD files "
        "these miss** — code-behind/designer siblings, the compiled `.js` for any `.ts` "
        "you change, the `.resx` for any new string, the SQL for any schema/setting.",
        "- Files flagged by **co-change history** are 'usually changed together' with "
        "this kind of work — treat them as strong hints you'll likely need them too.",
        "- Signals per file are shown as `[concept|history|graph]`; more signals = more "
        "corroboration, but a single-signal file can still be essential.",
        "",
        "## Candidate files (grouped by layer — order within a group is NOT priority)",
    ]
    by_layer = {}
    for p, arms in rows:
        by_layer.setdefault(_layer_of(p), []).append((p, arms))
    order = [n for n, _ in _LAYERS] + ["Other"]
    for layer in order:
        items = by_layer.get(layer)
        if not items:
            continue
        md.append("")
        md.append(f"**{layer}:**")
        for p, arms in items:
            md.append(f"- `{p}`  [{'|'.join(sorted(arms))}]")
    return "\n".join(md)


def build_dossier(eng, pid, rec):
    """Run the 3-arm ensemble; return (markdown, ranked_files). Each file is
    tagged with which arms surfaced it."""
    from engram_client import canon
    prov = {}  # canon path -> set of arm labels
    for label, fn in ARMS:
        try:
            for p in fn(eng, pid, rec):
                prov.setdefault(canon(p), set()).add(label)
        except Exception as e:
            print(f"  arm {label} error: {str(e)[:120]}", file=sys.stderr)
    rows = list(prov.items())
    md = render_dossier(rec["story"]["title"], rows)
    ranked = [p for p, _ in sorted(rows, key=lambda r: (-len(r[1]), r[0]))]
    return md, ranked


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pr", type=int, required=True)
    args = ap.parse_args()
    os.makedirs(P2_DATA, exist_ok=True)
    os.makedirs(P2_WT_ROOT, exist_ok=True)

    rec = {r["pr_id"]: r for r in json.load(open(rp.CORPUS, encoding="utf-8"))}[args.pr]
    base = rec["base_commit"]
    print(f"PR {args.pr} @ base {base[:8]} — {rec['story']['title'][:60]}")

    # 1. index at base + run ensemble -> dossier
    eng = Engram()
    pid = wt = None
    try:
        pid, wt, idx_secs, health = rp.setup_index(eng, rec)
        print(f"  indexed in {idx_secs:.0f}s")
        dossier_md, ranked = build_dossier(eng, pid, rec)
        print(f"  dossier: {len(ranked)} ranked files")
    finally:
        if pid:
            eng.tool("delete_project", {"project_id": pid})
        if wt:
            remove_worktree(wt)
        eng.close()

    # 2. create agent worktrees at base_commit (persist for the agents)
    wt_alone = os.path.join(P2_WT_ROOT, f"pr{args.pr}_alone")
    wt_engram = os.path.join(P2_WT_ROOT, f"pr{args.pr}_engram")
    add_worktree(base, wt_alone)
    add_worktree(base, wt_engram)
    print(f"  worktrees: {wt_alone}\n             {wt_engram}")

    # 3. write dossier + manifest
    dossier_path = os.path.join(P2_DATA, f"pr{args.pr}_dossier.md")
    with open(dossier_path, "w", encoding="utf-8") as fh:
        fh.write(dossier_md)
    manifest = {
        "pr_id": args.pr,
        "base_commit": base,
        "story": rec["story"],                 # code-gen agents see this
        "worktree_alone": wt_alone,
        "worktree_engram": wt_engram,
        "dossier_path": dossier_path,
        "ground_truth": rec["ground_truth"],   # JUDGE ONLY — never to code-gen agents
    }
    with open(os.path.join(P2_DATA, f"pr{args.pr}.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2)
    print(f"  wrote {dossier_path} and manifest pr{args.pr}.json")


if __name__ == "__main__":
    main()
