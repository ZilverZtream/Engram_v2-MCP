"""Prototype: ENRICHED dossier = the native get_change_set file list PLUS, for the
top-ranked page families, a compact STRUCTURAL MAP from get_page_context (control
table + method index with exact line ranges, method bodies stripped) and, for the
single strongest code anchor, the focused get_method_edit_context of its most
relevant method.

Hypothesis under test: file recall is maxed, but the agent still loses impl points
because a file LIST doesn't tell it WHICH method/control to touch or what the code
currently does. A structural map ("here are the handlers and their line ranges")
lets the agent Read the right code precisely instead of guessing. Read-only.

Writes eval/data/p2/pr{PR}_dossier_rich.md. Generic: no per-repo specifics.

Usage: python eval/enrich_dossier.py 1933 1967 ...
"""
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram  # noqa: E402

P2 = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "p2")
PAGE_EXT = (".aspx", ".ascx", ".master")
MAX_PAGES = 3          # top page families to map
MAX_MAP_CHARS = 13000  # per-page structural-map cap (fits a ~50-method page whole)
MAX_CONTROL_ROWS = 45  # trim very long control tables so the method index survives


def strip_bodies(page_md):
    """Keep get_page_context's structure (header, Server Controls table, Methods
    INDEX, state/traps) but drop the fenced code blocks (the big method bodies).
    The method index is the most valuable part (it maps the code), so a very long
    Server Controls table is trimmed to keep the index from being capped out."""
    out, in_fence, ctrl_rows = [], False, 0
    in_controls = False
    for ln in page_md.splitlines():
        if ln.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        if ln.startswith("## "):
            in_controls = ln.startswith("## Server Controls")
        if in_controls and ln.startswith("| `"):  # a control-table data row
            ctrl_rows += 1
            if ctrl_rows == MAX_CONTROL_ROWS:
                out.append(f"| … | (+{'more'} controls — see the page) | | | |")
            if ctrl_rows >= MAX_CONTROL_ROWS:
                continue
        out.append(ln)
    text = "\n".join(out)
    text = re.sub(r"\n{3,}", "\n\n", text)  # collapse blank runs left by removals
    return text


def top_files(change_set_md):
    """Ranked candidate paths from the change set, in listed order (already
    co-change-first). Returns [(path, tags)]."""
    out = []
    for m in re.finditer(r"^- `([^`]+)`\s*\[([^\]]*)\]", change_set_md, re.M):
        out.append((m.group(1), set(t for t in m.group(2).split("|") if t)))
    return out


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
        try:
            cs = e.tool("get_change_set", {"project_id": pid, "story": story})
            ranked = top_files(cs)
            # Map the page families where the WORK is: derive the page from each
            # golden page-file AND from each golden code-behind (.aspx.vb/.ascx.vb)
            # — the code-behind is usually the real edit target. Deprioritize
            # master pages: they co-change with everything and are rarely the work
            # site, so they'd crowd out the story's actual page.
            regular, masters, seen = [], [], set()
            for path, tags in ranked:
                if not ({"cochange", "history"} & tags):
                    continue
                low = path.lower()
                if low.endswith((".aspx.vb", ".ascx.vb", ".master.vb")):
                    page = path[:-3]          # strip code-behind ".vb"
                elif low.endswith(PAGE_EXT):
                    page = path
                else:
                    continue
                stem = re.sub(r"\.(aspx|ascx|master)$", "", page.lower())
                if stem in seen:
                    continue
                seen.add(stem)
                (masters if page.lower().endswith(".master") else regular).append(page)
            pages = (regular + masters)[:MAX_PAGES]

            blocks = []
            for ap in pages:
                pc = e.tool("get_page_context",
                            {"project_id": pid, "aspx_file": ap,
                             "include_method_bodies": True})
                if "TOOL_ERROR" in pc:
                    continue
                m = strip_bodies(pc)[:MAX_MAP_CHARS]
                blocks.append(m)

            enriched = cs
            if blocks:
                enriched += (
                    "\n\n---\n\n# Implementation map — current structure of the "
                    "top pages (read the exact methods/line-ranges below from the "
                    "checkout; bodies omitted for brevity)\n\n"
                    + "\n\n---\n\n".join(blocks)
                )
        finally:
            e.close()

        outp = os.path.join(P2, f"pr{pr}_dossier_rich.md")
        open(outp, "w", encoding="utf-8").write(enriched)
        print(f"PR {pr}: change_set {len(ranked)} files | mapped {len(pages)} pages "
              f"| enriched {len(enriched)} chars -> {os.path.basename(outp)}")


if __name__ == "__main__":
    main()
