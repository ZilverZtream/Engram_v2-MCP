"""Prototype 2: CHANGE-PATTERN dossier = the native get_change_set file list PLUS
"how similar changes were made before" — the clean git diffs of the top historical
commits most similar to this story (via find_similar_changes). Unlike the structural
map (which anchored on a page's structure), this guides the APPROACH/mechanism
without dictating scope, targeting the impl failures that were reasoning errors
(wrong mechanism / over-engineering / misdiagnosis).

Leakage-safe: find_similar_changes scans only the indexed history (ancestors of the
leakage-free base_commit), so every surfaced commit predates the PR under test.

Writes eval/data/p2/pr{PR}_dossier_pattern.md. Read-only. Generic.

Usage: python eval/change_pattern_dossier.py 1937 1974 1967 ...
"""
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram  # noqa: E402

P2 = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "p2")
REPO = r"C:\Users\Dennis\source\repos\OciusX"
MAX_COMMITS = 3        # approach precedents to include
MAX_DIFF_CHARS = 5000  # per-commit diff cap


def golden_seed(cs_md, n=3):
    """A TIGHT seed (top few golden CODE files) yields higher-precision similar
    commits than a broad one. Prefer code files (.vb/.cs/.ts) over resx/sql."""
    code, other = [], []
    for m in re.finditer(r"^- `([^`]+)`\s*\[([^\]]*)\]", cs_md, re.M):
        tags = set(t for t in m.group(2).split("|") if t)
        if not ({"cochange", "history"} & tags):
            continue
        p = m.group(1).lower()
        (code if p.endswith((".vb", ".cs", ".ts", ".tsx", ".js")) else other).append(m.group(1))
    return (code + other)[:n]


def git_show(h):
    # OciusX history is Azure DevOps PR merges (merge commits). `git show` on a
    # merge shows no file diff; diff against the first parent to get the PR's net
    # changes. For a normal commit, ^1 is its sole parent (== git show).
    try:
        return subprocess.run(
            ["git", "-C", REPO, "diff", f"{h}^1", h, "--stat=200", "--patch",
             "--no-color", "-M"],
            capture_output=True, text=True, encoding="utf-8", errors="replace",
            timeout=30).stdout
    except Exception as ex:
        return f"(git diff {h}^1 {h} failed: {ex})"


def main():
    prs = [int(x) for x in sys.argv[1:]] or [1937]
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
            seed = golden_seed(cs)
            commits = []
            if seed:
                sim = e.tool("find_similar_changes",
                             {"project_id": pid, "files": seed,
                              "max_commits": 800, "top": MAX_COMMITS + 2})
                # parse "## #N <hash> (similarity X) — <title>"
                for m in re.finditer(r"## #\d+ ([0-9a-f]{6,}) \(similarity ([\d.]+)\) — (.+)", sim):
                    commits.append((m.group(1), m.group(2), m.group(3).strip()))
        finally:
            e.close()

        blocks = []
        for h, sim, title in commits[:MAX_COMMITS]:
            diff = git_show(h)[:MAX_DIFF_CHARS]
            blocks.append(f"### Precedent (similarity {sim}): {title}\n\n```diff\n{diff}\n```")

        enriched = cs
        if blocks:
            enriched += (
                "\n\n---\n\n# How similar changes were made before (approach "
                "precedent — these are PAST commits closest to this story; use them "
                "to choose the right mechanism/layering, then adapt — do not copy "
                "scope blindly)\n\n" + "\n\n".join(blocks)
            )

        outp = os.path.join(P2, f"pr{pr}_dossier_pattern.md")
        open(outp, "w", encoding="utf-8").write(enriched)
        print(f"PR {pr}: {len(commits[:MAX_COMMITS])} precedents | "
              f"enriched {len(enriched)} chars -> {os.path.basename(outp)}")


if __name__ == "__main__":
    main()
