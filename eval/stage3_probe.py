"""Stage-3 retrieval probe: does the team's quality-gate corpus CONTAIN the
developer decisions the user story omits (the thing that caps parity)?

Cheap gate before the expensive impl A/B:
  1. Build a self-contained quality-gate corpus project (copilot-instructions +
     board + CodeRabbit findings — all GLOBAL team knowledge, leakage-free; the
     pilot PRs' own findings were excluded upstream).
  2. Index it + ingest the three sources into the `quality_gate` namespace.
  3. For each pilot story, run pre_push_audit(story_text) and show the rules it
     surfaces — then check whether they touch the files / concepts the developer
     actually changed (ground_truth), i.e. the omitted context becomes visible.

Output: eval/data/stage3_probe.json + a console summary.
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
CORPUS_DIR = os.path.join(DATA, "qg_corpus")
COPILOT_SRC = r"C:\Users\Dennis\source\repos\OciusX\.github\copilot-instructions.md"
CORPUS = os.path.join(DATA, "ociusx_prs.json")
PILOT = [1937, 1941, 1961, 1965, 1967, 1908, 1925, 1917, 1933, 1913, 1920, 1974, 1977]


def build_corpus_dir():
    os.makedirs(CORPUS_DIR, exist_ok=True)
    shutil.copyfile(COPILOT_SRC, os.path.join(CORPUS_DIR, "copilot-instructions.md"))
    for f in ("qg_board.json", "qg_coderabbit.json"):
        shutil.copyfile(os.path.join(DATA, f), os.path.join(CORPUS_DIR, f))
    print(f"corpus dir: {CORPUS_DIR}")


def story_text(rec):
    s = rec["story"]
    t = s["title"]
    if s.get("description"):
        t += "\n\n" + s["description"]
    if s.get("acceptance"):
        t += "\n\nAcceptance:\n" + s["acceptance"]
    return t


def gt_files(rec):
    return {canon(c["path"]) for c in rec["ground_truth"]["changed_files"]}


_PID_RE = re.compile(r"project[_ ]?id[\"']?\s*[:=]\s*[\"']?([0-9a-f-]{36})", re.I)


def main():
    build_corpus_dir()
    corpus = {r["pr_id"]: r for r in json.load(open(CORPUS, encoding="utf-8"))}

    eng = Engram(stderr_path=os.path.join(DATA, "stage3_probe.stderr.log"))
    try:
        out = eng.tool("index_project", {
            "directory": CORPUS_DIR, "project_name": "ociusx_qg_corpus",
            "project_type": "general", "wait": True, "dedupe_by_directory": False,
        })
        m = _PID_RE.search(out)
        if not m:
            print("INDEX FAILED:\n", out[:800]); return
        pid = m.group(1)
        print(f"qg project: {pid}")

        for stype, sfile in [("copilot", "copilot-instructions.md"),
                             ("board", "qg_board.json"),
                             ("coderabbit", "qg_coderabbit.json")]:
            r = eng.tool("ingest_quality_gates",
                         {"project_id": pid, "source_path": sfile, "source_type": stype})
            print(f"  ingest {stype}: {r.strip()[:140]}")

        results = []
        for pr in PILOT:
            rec = corpus.get(pr)
            if not rec:
                continue
            gtf = gt_files(rec)
            audit = eng.tool("pre_push_audit",
                             {"project_id": pid, "code": story_text(rec), "top_k": 10})
            # which rules are file-scoped to a ground-truth-changed file?
            scoped_hits = []
            for line in audit.splitlines():
                mm = re.match(r"- \[([^\]]+)\]\s*(.*)", line.strip())
                if not mm:
                    continue
                rule_path, rule_text = canon(mm.group(1)), mm.group(2)
                hit_gt = any(rule_path and (rule_path in g or g in rule_path) for g in gtf)
                scoped_hits.append({"path": mm.group(1), "text": rule_text[:240],
                                    "touches_gt_file": hit_gt})
            n_gt = sum(1 for h in scoped_hits if h["touches_gt_file"])
            results.append({"pr": pr, "title": rec["story"]["title"][:80],
                            "rules": scoped_hits, "rules_touching_gt": n_gt})
            print(f"\n=== PR {pr}: {rec['story']['title'][:70]} ===")
            print(f"  ground-truth files: {len(gtf)}; rules surfaced: {len(scoped_hits)}; "
                  f"rules scoped to a changed file: {n_gt}")
            for h in scoped_hits[:6]:
                mark = "★GT" if h["touches_gt_file"] else "  "
                print(f"   {mark} [{h['path']}] {h['text'][:150]}")

        with open(os.path.join(DATA, "stage3_probe.json"), "w", encoding="utf-8") as fh:
            json.dump(results, fh, indent=2, ensure_ascii=False)
        tot = sum(r["rules_touching_gt"] for r in results)
        print(f"\nSUMMARY: {tot} rules across {len(results)} stories scoped to an "
              f"actually-changed file. (probe of whether the omitted context is retrievable)")
    finally:
        eng.close()


if __name__ == "__main__":
    main()
