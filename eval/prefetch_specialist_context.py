"""Pre-fetch per-domain Engram SPECIALIST context for the domain-planner pipeline.

Workflow subagents can't call Engram (stdio-driven), so we gather each
specialist's Engram output here and hand it to its planning agent. Uses ONLY the
Engram tools that actually work well (business_logic DocStore is empty unless
analyze_business_logic was run, so it's excluded):
  similar -> find_similar_changes + clean git diffs of the top precedents
  ui      -> get_page_context (structure: control table + method index) for the
             top few candidate page families (so a single wrong pick can't blind it)
  style   -> analyze_file_coding_style (now VB-language-aware) for the top code file

Writes eval/data/p2/pr{PR}_ctx_{similar,ui,style}.md. Read-only. Leakage-safe.

Usage: python eval/prefetch_specialist_context.py 1933 1937 ...
"""
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram  # noqa: E402
import enrich_dossier as ed  # strip_bodies  # noqa: E402

P2 = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "p2")
REPO = r"C:\Users\Dennis\source\repos\OciusX"
PAGE_EXT = (".aspx", ".ascx", ".master")
MAX_PAGES = 3
MAX_UI = 9000


def golden(cs_md):
    out = []
    for m in re.finditer(r"^- `([^`]+)`\s*\[([^\]]*)\]", cs_md, re.M):
        if {"cochange", "history"} & set(t for t in m.group(2).split("|") if t):
            out.append(m.group(1))
    return out


def candidate_pages(gold):
    """Page families from golden page-files AND golden code-behind, masters last."""
    regular, masters, seen = [], [], set()
    for p in gold:
        low = p.lower()
        if low.endswith((".aspx.vb", ".ascx.vb", ".master.vb")):
            page = p[:-3]
        elif low.endswith(PAGE_EXT):
            page = p
        else:
            continue
        stem = re.sub(r"\.(aspx|ascx|master)$", "", page.lower())
        if stem in seen:
            continue
        seen.add(stem)
        (masters if page.lower().endswith(".master") else regular).append(page)
    return (regular + masters)[:MAX_PAGES]


def top_code(gold):
    for p in gold:
        low = p.lower()
        if low.endswith((".vb", ".cs")) and not low.endswith((".designer.vb", ".designer.cs")):
            return p
    return None


def git_diff(h):
    try:
        return subprocess.run(["git", "-C", REPO, "diff", f"{h}^1", h, "--stat=200",
                               "--patch", "--no-color", "-M"], capture_output=True,
                              text=True, encoding="utf-8", errors="replace", timeout=30).stdout
    except Exception as ex:
        return f"(diff {h} failed: {ex})"


def main():
    prs = [int(x) for x in sys.argv[1:]] or [1933]
    imap = json.load(open(os.path.join(P2, "index_map.json"), encoding="utf-8"))
    for pr in prs:
        pid = imap.get(str(pr))
        man = json.load(open(os.path.join(P2, f"pr{pr}.json"), encoding="utf-8"))
        s = man["story"]
        story = s["title"]
        if s.get("description"):
            story += "\n\n" + s["description"]
        if s.get("acceptance"):
            story += "\n\nAcceptance:\n" + s["acceptance"]

        e = Engram()
        ctx = {}
        try:
            cs = e.tool("get_change_set", {"project_id": pid, "story": story})
            gold = golden(cs)
            pages, tc = candidate_pages(gold), top_code(gold)

            sim = e.tool("find_similar_changes", {"project_id": pid, "files": gold[:8],
                                                  "max_commits": 800, "top": 5})
            hashes = re.findall(r"## #\d+ ([0-9a-f]{6,}) \(similarity ([\d.]+)\) — (.+)", sim)
            blocks = [sim.split("##")[0]]
            for h, simv, title in hashes[:3]:
                blocks.append(f"### Precedent (sim {simv}): {title}\n```diff\n{git_diff(h)[:4500]}\n```")
            ctx["similar"] = "\n\n".join(blocks)

            ui_blocks = []
            for ap in pages:
                pc = e.tool("get_page_context", {"project_id": pid, "aspx_file": ap,
                                                 "include_method_bodies": True})
                if "TOOL_ERROR" not in pc:
                    ui_blocks.append(ed.strip_bodies(pc)[:MAX_UI])
            ctx["ui"] = ("\n\n---\n\n".join(ui_blocks)
                         if ui_blocks else "(no server page among golden files — likely client/TS-driven)")

            if tc:
                st = e.tool("analyze_file_coding_style", {"project_id": pid, "file_path": tc, "diff_limit": 30})
                ctx["style"] = st[:6000] if "TOOL_ERROR" not in st else "(no style profile)"
            else:
                ctx["style"] = "(no code file among golden files)"
        finally:
            e.close()

        for role, text in ctx.items():
            open(os.path.join(P2, f"pr{pr}_ctx_{role}.md"), "w", encoding="utf-8").write(text or "")
        print(f"PR {pr}: pages={len(pages)} code={tc} | "
              + " ".join(f"{r}={len(t or '')}" for r, t in ctx.items()))


if __name__ == "__main__":
    main()
