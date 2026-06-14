"""Generate the per-PR Stage-3 quality-gate context (ctx_qualitygate.md) the A/B
feeds to the team-memory planner + final pre-push audit.

It is the "context the developer had but the user story omits":
  1. copilot-instructions.md  — the team's rulebook (verbatim; small, high-signal)
  2. the recurring-issues board lessons (verbatim)
  3. file-scoped CodeRabbit/Sonar findings for the files this change is predicted
     to touch (from the dossier prov), via pre_push_audit — the change-specific
     review history the developer would have seen.

Leakage-free: the corpus excluded the pilot PRs' own findings upstream, and the
predicted-file list comes from Engram's dossier (story-derived), not the PR diff.

Uses a FRESH data_dir (ENGRAM_EVAL_DATA_DIR) so it never touches the 29 GB store.
"""
import io
import json
import os
import re
import shutil
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram, canon  # noqa: E402

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "data")
P2 = os.path.join(DATA, "p2")
CORPUS_DIR = os.path.join(DATA, "qg_corpus")
COPILOT_SRC = r"C:\Users\Dennis\source\repos\OciusX\.github\copilot-instructions.md"
CORPUS = os.path.join(DATA, "ociusx_prs.json")
PREPPED = [1937, 1965, 1967, 1908, 1933, 1913, 1974]
_PID_RE = re.compile(r"project[_ ]?id[\"']?\s*[:=]\s*[\"']?([0-9a-f-]{36})", re.I)


def build_corpus_dir():
    os.makedirs(CORPUS_DIR, exist_ok=True)
    shutil.copyfile(COPILOT_SRC, os.path.join(CORPUS_DIR, "copilot-instructions.md"))
    for f in ("qg_board.json", "qg_coderabbit.json"):
        shutil.copyfile(os.path.join(DATA, f), os.path.join(CORPUS_DIR, f))


def story_text(rec):
    s = rec["story"]
    t = s["title"]
    if s.get("description"):
        t += "\n\n" + s["description"]
    if s.get("acceptance"):
        t += "\n\nAcceptance:\n" + s["acceptance"]
    return t


def predicted_files(pr):
    """Files the dossier predicts this change touches (golden co-change/history first)."""
    try:
        prov = json.load(open(os.path.join(P2, f"pr{pr}_prov.json"), encoding="utf-8"))
    except Exception:
        return []
    golden = [p for p, sig in prov.items() if set(sig) & {"history", "cochange"}]
    rest = [p for p in prov if p not in golden]
    return (golden + rest)[:12]


def main():
    build_corpus_dir()
    corpus = {r["pr_id"]: r for r in json.load(open(CORPUS, encoding="utf-8"))}
    copilot_md = open(COPILOT_SRC, encoding="utf-8").read()
    board = json.load(open(os.path.join(DATA, "qg_board.json"), encoding="utf-8"))
    board_block = "\n".join(f"- {b['message']}" for b in board)

    eng = Engram(stderr_path=os.path.join(DATA, "stage3_ctx.stderr.log"))
    try:
        out = eng.tool("index_project", {
            "directory": CORPUS_DIR, "project_name": "ociusx_qg_ctx",
            "project_type": "general", "wait": True, "dedupe_by_directory": False,
        })
        m = _PID_RE.search(out)
        if not m:
            print("INDEX FAILED:\n", out[:800]); return
        pid = m.group(1)
        for stype, sfile in [("copilot", "copilot-instructions.md"),
                             ("board", "qg_board.json"),
                             ("coderabbit", "qg_coderabbit.json")]:
            eng.tool("ingest_quality_gates",
                     {"project_id": pid, "source_path": sfile, "source_type": stype})
        print(f"qg ctx project {pid} ready")

        for pr in PREPPED:
            rec = corpus.get(pr)
            if not rec:
                print(f"  PR {pr}: no corpus rec, skip"); continue
            pf = predicted_files(pr)
            # Query the corpus with story + predicted file basenames for change-aware findings.
            q = story_text(rec) + "\n\nFiles likely touched:\n" + "\n".join(
                p.rsplit("/", 1)[-1] for p in pf)
            audit = eng.tool("pre_push_audit", {"project_id": pid, "code": q, "top_k": 14})
            # keep only the CodeRabbit/Sonar finding lines (drop copilot/board dupes — we add those verbatim)
            findings = []
            for line in audit.splitlines():
                mm = re.match(r"- \[([^\]]+)\]\s*(.*)", line.strip())
                if not mm:
                    continue
                src = mm.group(1)
                if src.endswith(".md") or src.endswith("qg_board.json"):
                    continue
                findings.append(f"- `{src}` — {mm.group(2)[:300]}")
            ctx = (
                f"# Team quality gates for this change — the context a developer has, but the story omits\n\n"
                f"Use these BEFORE and DURING implementation. They are this team's actual rulebook, "
                f"recurring-mistake board, and prior code-review findings on the files this change is "
                f"likely to touch. Match the conventions; do NOT repeat the flagged mistakes.\n\n"
                f"## 1. Coding & agent rules (copilot-instructions.md)\n\n{copilot_md}\n\n"
                f"## 2. Recurring-issues board — mistakes the team keeps making (avoid these)\n\n{board_block}\n\n"
                f"## 3. Prior review findings on the files this change is predicted to touch\n\n"
                + ("\n".join(findings) if findings else "_(none retrieved)_") + "\n"
            )
            outp = os.path.join(P2, f"pr{pr}_ctx_qualitygate.md")
            open(outp, "w", encoding="utf-8").write(ctx)
            print(f"  PR {pr}: ctx_qualitygate.md  ({len(findings)} file-scoped findings, "
                  f"{len(ctx)} chars)")
    finally:
        eng.close()


if __name__ == "__main__":
    main()
