"""Forward-prediction experiment — feature 891 (Change Requests from map markers).

Index the pre-feature OciusX master worktree (commit bc587b289, already created at
%TEMP%/engram_eval_wt/fwd891) as a standalone Engram project, then generate one
planning dossier per user story 892..897 via the native get_change_set tool —
mirroring eval/phase2_prep.py (same eval server config, same index recipe, the
tool's markdown IS the dossier). No merged_before: the worktree's git history is
reachable-from-HEAD only, so it cannot contain unmerged/future PRs.
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram, WORKTREE_ROOT  # noqa: E402
import run_phase1 as rp  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "data", "forward891")
WT = os.path.join(WORKTREE_ROOT, "fwd891")
BASE = "bc587b289"
INDEX_MAP = os.path.join(DATA, "index_map.json")
US_IDS = ["892", "893", "894", "895", "896", "897"]


def _load_map():
    try:
        return json.load(open(INDEX_MAP, encoding="utf-8"))
    except Exception:
        return {}


def _project_valid(eng, pid):
    """Same contract as phase2_prep: kept index exists AND has content."""
    if not pid:
        return False
    h = eng.tool("project_health", {"project_id": pid})
    return ("graph_nodes" in h and "lancedb_vectors" in h
            and "TOOL_ERROR" not in h and "❌" not in h)


def compose_story(stories, us_id):
    """title + description + acceptance, with the parent feature's description
    prepended once as 'Feature context:' (truncated to ~1200 chars)."""
    s = stories[us_id]
    parts = [s.get("title", "")]
    if s.get("description"):
        parts.append(s["description"])
    if s.get("acceptance"):
        parts.append("Acceptance criteria:\n" + s["acceptance"])
    story = "\n\n".join(p for p in parts if p).strip()
    feat = (stories["891"].get("description") or "").strip()[:1200]
    return f"Feature context: {feat}\n\n{story}"


def main():
    stories = json.load(open(os.path.join(DATA, "stories.json"), encoding="utf-8"))
    if not os.path.isdir(os.path.join(WT, "Site")):
        raise RuntimeError(f"worktree missing or incomplete: {WT}")

    print(f"[fwd891] spawning eval engram server (prod data_dir, multi_client off)...",
          flush=True)
    t0 = time.time()
    eng = Engram(stderr_path=os.path.join(DATA, "server_stderr.log"))
    print(f"[fwd891] server up in {time.time() - t0:.0f}s", flush=True)

    try:
        imap = _load_map()
        pid = imap.get("fwd891")
        if _project_valid(eng, pid):
            print(f"[fwd891] REUSE kept index {pid}", flush=True)
        else:
            print(f"[fwd891] indexing {WT} ...", flush=True)
            t0 = time.time()
            out = eng.tool("index_project", {
                "directory": WT, "project_name": f"fwd891_{BASE[:8]}",
                "project_type": "dotnetwebformsvb", "wait": True,
                "dedupe_by_directory": False,
            })
            m = rp._PID_RE.search(out)
            if not m:
                raise RuntimeError(f"no project_id in index_project output:\n{out[:800]}")
            pid = m.group(1)
            print(f"[fwd891] indexed as {pid} in {time.time() - t0:.0f}s", flush=True)
            t0 = time.time()
            eng.tool("index_git_history", {"project_id": pid, "max_commits": 500,
                                           "wait": True})
            print(f"[fwd891] git history indexed in {time.time() - t0:.0f}s", flush=True)
            json.dump({"fwd891": pid}, open(INDEX_MAP, "w", encoding="utf-8"), indent=2)
        health = eng.tool("project_health", {"project_id": pid})
        print(f"[fwd891] health:\n{health[:600]}", flush=True)

        summaries = []
        for us in US_IDS:
            story = compose_story(stories, us)
            print(f"[us{us}] get_change_set ({len(story)} chars story)...", flush=True)
            t0 = time.time()
            md = eng.tool("get_change_set", {"project_id": pid, "story": story})
            secs = time.time() - t0
            if "TOOL_ERROR" in md:
                print(f"[us{us}] ERROR: {md[:300]}", flush=True)
                summaries.append(f"us{us}: FAILED ({md[:120]})")
                continue
            path = os.path.join(DATA, f"us{us}_dossier.md")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(md)
            nbytes = os.path.getsize(path)
            low = md.lower()
            perm = "permission gate" in low
            cand = "candidate" in low
            line = (f"us{us}: {nbytes} bytes, Permission gates={'YES' if perm else 'NO'}, "
                    f"candidate section={'YES' if cand else 'NO'} ({secs:.0f}s)")
            print(f"[us{us}] {line}", flush=True)
            summaries.append(line)

        manifest = {
            "feature": "891",
            "base_commit": "bc587b289702d8a5e1853a18a6bddec928504938",
            "worktree": WT,
            "project_id": pid,
            "us_ids": US_IDS,
            "story_texts": {us: compose_story(stories, us) for us in US_IDS},
            "dossiers": {us: os.path.join(DATA, f"us{us}_dossier.md") for us in US_IDS},
        }
        with open(os.path.join(DATA, "manifest.json"), "w", encoding="utf-8") as fh:
            json.dump(manifest, fh, indent=2)

        print("\n=== SUMMARY ===", flush=True)
        for line in summaries:
            print(line, flush=True)
    finally:
        eng.close()  # index kept for reuse via index_map.json


if __name__ == "__main__":
    main()
