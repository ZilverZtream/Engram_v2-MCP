"""Phase 1 — Engram strategy tournament against OciusX merged PRs.

For each pilot story: index OciusX at the PR's `base_commit` (leakage-free —
master *before* the PR), run several candidate Engram tool sequences using ONLY
the user story as input, take the source files each sequence surfaces as its
prediction, and score them against the PR's actually-changed files.

The winning sequence = the empirically optimal `engram-workflow.md` for "given a
US, find the files to change". US-only input + read-only worktrees are enforced
here exactly as in ado_fetch.py's dataset split (`story` vs `ground_truth`).

Usage:
  python eval/run_phase1.py --pr 1906                  # validate pipeline on 1 PR
  python eval/run_phase1.py --pilot                    # full 13-PR pilot
  python eval/run_phase1.py --ids 1937,1974 --out eval/data/phase1.json
"""
import argparse
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import (  # noqa: E402
    Engram, add_worktree, remove_worktree, extract_paths, extract_paths_ordered,
    extract_node_ids, norm_path, canon_set, basename_set, page_stem_set,
    WORKTREE_ROOT,
)

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "data", "ociusx_prs.json")
PILOT_IDS = os.path.join(HERE, "data", "pilot_ids.json")

_PID_RE = re.compile(r"project_id:\s*([0-9a-fA-F-]{36})")
# Words that carry no retrieval signal in a user-story title.
_STOP = set((
    "as a an the of for to be able would like i we our my your you it its and or "
    "not is are want need do does developer user admin power chore bug task this "
    "that with on in at by able some have has get set new add make able when then "
    "should could able feature story given so they them their"
).split())


# ── story helpers (US-ONLY — never touch rec['ground_truth']) ────────────────
def story_text(rec):
    s = rec["story"]
    parts = [s.get("title", "")]
    if s.get("description"):
        parts.append(s["description"])
    if s.get("acceptance"):
        parts.append("Acceptance criteria:\n" + s["acceptance"])
    return "\n".join(p for p in parts if p).strip()


def concepts_from_story(rec, n=3):
    """Best-effort domain concepts from the title (S3 needs explicit concepts)."""
    title = rec["story"].get("title", "")
    words = re.findall(r"[A-Za-z][A-Za-z0-9]{2,}", title)
    seen = []
    for w in words:
        if w.lower() in _STOP:
            continue
        if w.lower() not in {s.lower() for s in seen}:
            seen.append(w)
    seen.sort(key=len, reverse=True)
    return seen[:n] or words[:n]


# ── indexing (leakage-free: worktree at base_commit, pre-PR history only) ────
def setup_index(eng, rec, max_commits=500):
    pr, base = rec["pr_id"], rec["base_commit"]
    wt = os.path.join(WORKTREE_ROOT, f"pr{pr}")
    add_worktree(base, wt)
    name = f"eval_pr{pr}_{base[:8]}"
    t0 = time.time()
    out = eng.tool("index_project", {
        "directory": wt, "project_name": name,
        "project_type": "dotnetwebformsvb", "wait": True,
        "dedupe_by_directory": False,
    })
    m = _PID_RE.search(out)
    if not m:
        raise RuntimeError(f"no project_id in index_project output:\n{out[:600]}")
    pid = m.group(1)
    eng.tool("index_git_history", {"project_id": pid, "max_commits": max_commits, "wait": True})
    # Merged-PR exemplar corpus (pr: docs in the history namespace). Without
    # this the dossier's "Approved exemplars" section silently vanishes on
    # fresh eval indexes (live 2026-07-10: PR1913 regen lost IRoqEntryService
    # because the baseline project HAD the corpus and the fresh one didn't).
    # Leak-freedom is preserved at QUERY time: get_change_set filters
    # exemplars by merged_before < the PR's closed_date.
    eng.tool("ingest_merged_prs", {"project_id": pid, "max_commits": max_commits})
    health = eng.tool("project_health", {"project_id": pid})
    return pid, wt, time.time() - t0, health


def teardown(eng, pid, wt):
    if pid:
        eng.tool("delete_project", {"project_id": pid})
    remove_worktree(wt)


# ── strategies: (eng, pid, rec) -> set[normalized path] ──────────────────────
def s0_search(eng, pid, rec):
    """Baseline ≈ no-Engram: plain keyword code search. A developer without
    Engram greps key terms; we search each story concept and union (code is in
    the default "memory" namespace). Per-keyword because search_memory returns
    nothing for a full multi-word NL title."""
    paths = set()
    for c in concepts_from_story(rec, n=3):
        paths |= extract_paths(_kw_search_memory(eng, pid, c, n=15))
    return paths


def s1_ask(eng, pid, rec):
    """The single NL entry point."""
    out = eng.tool("ask_codebase", {"project_id": pid, "question": story_text(rec)})
    return extract_paths(out)


def s2_plan(eng, pid, rec):
    """The dedicated planning tool."""
    out = eng.tool("plan_user_story", {"project_id": pid, "story": story_text(rec)})
    return extract_paths(out)


def s3_full(eng, pid, rec):
    """Full mandated flow: concept footprint -> implementation pattern ->
    similar changes -> incomplete-change companions."""
    paths = set()
    for c in concepts_from_story(rec):
        paths |= extract_paths(eng.tool("get_concept_footprint", {
            "project_id": pid, "concept": c, "max_per_group": 12}))
    paths |= extract_paths(eng.tool("find_implementation_pattern", {
        "project_id": pid, "pattern_query": rec["story"]["title"], "max_examples": 4}))
    seed = sorted(paths)[:10]
    if seed:
        paths |= extract_paths(eng.tool("find_similar_changes", {
            "project_id": pid, "files": seed, "top": 5}))
        paths |= extract_paths(eng.tool("detect_incomplete_changes", {
            "project_id": pid, "edited_files": seed, "max_partners": 5}))
    return paths


def s4_retrieval(eng, pid, rec):
    """Retrieval-heavy: semantic (vector on title) + lexical (per concept) + grep."""
    paths = set()
    paths |= extract_paths(eng.tool("vector_search", {
        "project_id": pid, "query": story_text(rec), "top_k": 20}))
    for c in concepts_from_story(rec, n=3):
        paths |= extract_paths(_kw_search_memory(eng, pid, c, n=12))
    for kw in concepts_from_story(rec, n=2):
        paths |= extract_paths(eng.tool("grep_project", {
            "project_id": pid, "pattern": kw, "regex": False}))
    return paths


# ── new candidate sequences (from the adversarial design panel) ──────────────
# These target the observed failure mode (conceptually-but-not-lexically related
# files): walk Engram's structural + co-change graph outward from a precise seed.

def _kw_search_memory(eng, pid, kw, n=8):
    """search_memory on a single keyword. NB: search_memory returns 0 for full
    multi-word NL titles (a known engram limitation — see eval/README), but works
    per-keyword; so all lexical seeding goes term-by-term."""
    return eng.tool("search_memory", {
        "project_id": pid, "query": kw, "max_results": n,
        "include_content": False, "fts_mode": "loose"})


def _seed_search(eng, pid, rec, n=8):
    """Precise anchor harvest: lexical (per concept keyword — emits node ids) +
    semantic (vector_search on the full title — works where search_memory does
    not). Returns ordered paths + node ids."""
    paths, nids = [], []
    for c in concepts_from_story(rec, n=3):
        out = _kw_search_memory(eng, pid, c, n=n)
        for p in extract_paths_ordered(out):
            if p not in paths:
                paths.append(p)
        for nid in extract_node_ids(out):
            if nid not in nids:
                nids.append(nid)
    vout = eng.tool("vector_search", {
        "project_id": pid, "query": rec["story"]["title"], "top_k": max(n, 10)})
    for p in extract_paths_ordered(vout):
        if p not in paths:
            paths.append(p)
    return paths, nids


def s5_graph_fanout(eng, pid, rec):
    """GRAPH-FIRST: tiny seed, then impact_analysis + symbol refs + co-change."""
    seed_paths, _ = _seed_search(eng, pid, rec, n=8)
    paths = set(seed_paths)
    for p in seed_paths[:5]:
        paths |= extract_paths(eng.tool("impact_analysis", {
            "project_id": pid, "file_path": p, "limit": 40}))
        paths |= extract_paths(eng.tool("analyze_temporal_couplings", {
            "project_id": pid, "file_path": p, "min_frequency": 2,
            "limit": 15, "inject_edges": False}))
    for c in concepts_from_story(rec, n=3):
        paths |= extract_paths(eng.tool("find_symbol_references", {
            "project_id": pid, "symbol_name": c,
            "max_incoming": 60, "max_outgoing_per_kind": 25}))
    return paths


def s6_node_traversal(eng, pid, rec):
    """GRAPH-FIRST traversal: node-id-seeded find_references + multi-hop BFS."""
    seed_paths, seed_nids = _seed_search(eng, pid, rec, n=8)
    paths = set(seed_paths)
    for nid in seed_nids[:6]:
        paths |= extract_paths(eng.tool("find_references", {
            "project_id": pid, "node_id": nid, "direction": "both"}))
    for nid in seed_nids[:5]:
        paths |= extract_paths(eng.tool("traverse_graph", {
            "project_id": pid, "node_id": nid, "max_hops": 2,
            "edge_kinds": ["contains", "dependency", "imports", "calls"],
            "direction": "both"}))
    for p in seed_paths[:5]:
        paths |= extract_paths(eng.tool("analyze_temporal_couplings", {
            "project_id": pid, "file_path": p, "min_frequency": 2,
            "limit": 12, "inject_edges": False}))
    return paths


def _history_seed(eng, pid, rec, hist_limit=12):
    """Commit-message-led seed (robust to vague stories) + lexical backfill."""
    seed = extract_paths_ordered(eng.tool("search_history", {
        "project_id": pid, "query": story_text(rec),
        "limit": hist_limit, "fts_mode": "loose"}))
    if len(seed) < 5:
        bk, _ = _seed_search(eng, pid, rec, n=12)
        for p in bk:
            if p not in seed:
                seed.append(p)
    return seed


def s7_history_expansion(eng, pid, rec):
    """CHANGE-HISTORY-FIRST: history seed amplified via similar-changes + couplings."""
    seed = _history_seed(eng, pid, rec, 12)
    paths = set(seed)
    cap = seed[:10]
    if cap:
        paths |= extract_paths(eng.tool("find_similar_changes", {
            "project_id": pid, "files": cap, "top": 6, "max_commits": 500}))
        paths |= extract_paths(eng.tool("detect_incomplete_changes", {
            "project_id": pid, "edited_files": cap, "max_partners": 6}))
        for p in cap[:6]:
            paths |= extract_paths(eng.tool("analyze_temporal_couplings", {
                "project_id": pid, "file_path": p, "min_frequency": 2,
                "limit": 8, "inject_edges": False}))
    return paths


def s8_coupling_2wave(eng, pid, rec):
    """CHANGE-HISTORY-FIRST: two-wave co-change breadth then commit-shape pass."""
    seed = _history_seed(eng, pid, rec, 15)
    wave1 = set()
    for p in seed[:12]:
        wave1 |= extract_paths(eng.tool("analyze_temporal_couplings", {
            "project_id": pid, "file_path": p, "min_frequency": 2,
            "limit": 10, "inject_edges": False}))
    enlarged = list(dict.fromkeys(seed + sorted(wave1)))[:15]
    paths = set(seed) | wave1
    if enlarged:
        paths |= extract_paths(eng.tool("detect_incomplete_changes", {
            "project_id": pid, "edited_files": enlarged, "max_partners": 6}))
        paths |= extract_paths(eng.tool("find_similar_changes", {
            "project_id": pid, "files": enlarged[:12], "top": 6, "max_commits": 500}))
    return paths


def s9_hybrid_funnel(eng, pid, rec):
    """Broad NL+concept seed -> co-change+graph expand -> centrality signal."""
    paths = set()
    paths |= extract_paths(eng.tool("plan_user_story", {
        "project_id": pid, "story": story_text(rec)}))
    for c in concepts_from_story(rec, n=3):
        paths |= extract_paths(eng.tool("get_concept_footprint", {
            "project_id": pid, "concept": c, "max_per_group": 12}))
    smem, _ = _seed_search(eng, pid, rec, n=15)
    paths |= set(smem)
    seed = sorted(paths)[:10]
    for p in seed:
        paths |= extract_paths(eng.tool("analyze_temporal_couplings", {
            "project_id": pid, "file_path": p, "min_frequency": 2,
            "limit": 8, "inject_edges": False}))
    if seed:
        paths |= extract_paths(eng.tool("find_similar_changes", {
            "project_id": pid, "files": seed, "top": 6, "max_commits": 500}))
    cs = concepts_from_story(rec, n=2)
    for c in cs:
        paths |= extract_paths(eng.tool("graph_search", {
            "project_id": pid, "query": c,
            "max_results": 15, "hop_depth": 2, "include_content": False}))
    paths |= extract_paths(eng.tool("graph_centrality_rerank", {
        "project_id": pid, "query": cs[0] if cs else rec["story"]["title"], "top_k": 30}))
    return paths


def s10_cochange_funnel(eng, pid, rec):
    """Leaner deterministic funnel: concept seed -> co-change companions -> rerank."""
    paths = set()
    for c in concepts_from_story(rec, n=2):
        paths |= extract_paths(eng.tool("get_concept_footprint", {
            "project_id": pid, "concept": c, "max_per_group": 15}))
    smem, _ = _seed_search(eng, pid, rec, n=12)
    paths |= set(smem)
    seed = sorted(paths)[:8]
    if seed:
        paths |= extract_paths(eng.tool("find_similar_changes", {
            "project_id": pid, "files": seed, "top": 6, "max_commits": 500}))
        for p in seed[:6]:
            paths |= extract_paths(eng.tool("analyze_temporal_couplings", {
                "project_id": pid, "file_path": p, "min_frequency": 2,
                "limit": 6, "inject_edges": False}))
        paths |= extract_paths(eng.tool("detect_incomplete_changes", {
            "project_id": pid, "edited_files": seed, "max_partners": 5}))
    cs = concepts_from_story(rec, n=1)
    paths |= extract_paths(eng.tool("graph_centrality_rerank", {
        "project_id": pid, "query": cs[0] if cs else rec["story"]["title"], "top_k": 25}))
    return paths


STRATEGIES = [
    ("S0_search", s0_search),
    ("S1_ask", s1_ask),
    ("S2_plan", s2_plan),
    ("S3_full", s3_full),
    ("S4_retrieval", s4_retrieval),
    ("S5_graph_fanout", s5_graph_fanout),
    ("S6_node_traversal", s6_node_traversal),
    ("S7_history_expansion", s7_history_expansion),
    ("S8_coupling_2wave", s8_coupling_2wave),
    ("S9_hybrid_funnel", s9_hybrid_funnel),
    ("S10_cochange_funnel", s10_cochange_funnel),
]


# ── scoring (vs ground_truth ONLY here, never fed to the tools above) ────────
def split_truth(rec):
    """Return (all, modified, added) normalized path sets from the PR.
    'added' files didn't exist at base_commit -> not retrievable by any index;
    'modified' (edit/delete/rename) are the fair retrieval target."""
    allp, mod, add = set(), set(), set()
    for cf in rec["ground_truth"]["changed_files"]:
        p = norm_path(cf["path"])
        allp.add(p)
        ct = (cf.get("change", "") or "").lower()
        # 'add' and 'rename' targets carry their POST-PR path, which does not
        # exist at base_commit -> unretrievable by any strategy. Both are
        # excluded from the fair retrieval metric (recall_modified).
        if "add" in ct or "rename" in ct:
            add.add(p)
        else:
            mod.add(p)
    return allp, mod, add


def score(predicted, truth):
    allp, mod, add = truth
    # Canonicalize both sides (strips a/ b/ git-diff prefixes, site/ web root,
    # DB root) so the SAME file matches regardless of which tool emitted it.
    pred_c, all_c, mod_c = canon_set(predicted), canon_set(allp), canon_set(mod)
    pred_bn, all_bn, mod_bn = basename_set(pred_c), basename_set(all_c), basename_set(mod_c)
    # Page-family level: collapses foo.aspx / foo.aspx.vb / foo.aspx.designer.vb
    # to one key — "did we find the right page?" (the faithful WebForms metric).
    pred_pg, mod_pg = page_stem_set(predicted), page_stem_set(mod)

    def rc(hit, tot):
        return round(len(hit) / len(tot), 3) if tot else None
    return {
        "n_predicted": len(pred_c),
        "n_truth": len(all_c), "n_modified": len(mod_c), "n_added": len(add),
        "recall_all": rc(pred_c & all_c, all_c),
        "recall_modified": rc(pred_c & mod_c, mod_c),
        "recall_modified_page": rc(pred_pg & mod_pg, mod_pg),
        "recall_all_basename": rc(pred_bn & all_bn, all_bn),
        "recall_modified_basename": rc(pred_bn & mod_bn, mod_bn),
        "precision": rc(pred_c & all_c, pred_c),
        "hits": sorted(pred_c & all_c),
        "page_hits": sorted(pred_pg & mod_pg),
        "missed_modified": sorted(mod_c - pred_c),
        "predicted_sample": sorted(pred_c)[:40],
    }


# ── driver ───────────────────────────────────────────────────────────────────
def load_records(ids):
    with open(CORPUS, encoding="utf-8") as fh:
        by_id = {r["pr_id"]: r for r in json.load(fh)}
    out = []
    for i in ids:
        if i not in by_id:
            print(f"  WARN: PR {i} not in corpus, skipping", file=sys.stderr)
            continue
        out.append(by_id[i])
    return out


def run(ids, out_path, max_commits=500):
    recs = load_records(ids)
    print(f"Phase 1: {len(recs)} stories, {len(STRATEGIES)} strategies\n")
    os.makedirs(WORKTREE_ROOT, exist_ok=True)
    eng = Engram(stderr_path=os.path.join(WORKTREE_ROOT, "server.stderr.log"))
    results = []
    try:
        for rec in recs:
            pr = rec["pr_id"]
            truth = split_truth(rec)
            print(f"── PR {pr} ({rec['author']}) — {rec['story']['title'][:60]}")
            print(f"   truth: {truth[0].__len__()} files "
                  f"({len(truth[1])} mod / {len(truth[2])} add) @ base {rec['base_commit'][:8]}")
            pid = wt = None
            row = {"pr_id": pr, "author": rec["author"],
                   "title": rec["story"]["title"],
                   "base_commit": rec["base_commit"],
                   "n_truth": len(truth[0]), "strategies": {}}
            try:
                pid, wt, idx_secs, health = setup_index(eng, rec, max_commits)
                row["index_secs"] = round(idx_secs, 1)
                row["health"] = _health_digest(health)
                print(f"   indexed in {idx_secs:.0f}s  {row['health']}")
                for name, fn in STRATEGIES:
                    t0 = time.time()
                    try:
                        pred = fn(eng, pid, rec)
                        sc = score(pred, truth)
                        sc["secs"] = round(time.time() - t0, 1)
                        row["strategies"][name] = sc
                        print(f"     {name:14s} mod={_fmt(sc['recall_modified'])} "
                              f"page={_fmt(sc['recall_modified_page'])} "
                              f"all={_fmt(sc['recall_all'])} "
                              f"bn={_fmt(sc['recall_all_basename'])} "
                              f"pred={sc['n_predicted']:3d} prec={_fmt(sc['precision'])} "
                              f"({sc['secs']}s)")
                    except Exception as e:
                        row["strategies"][name] = {"error": str(e)[:300]}
                        print(f"     {name:14s} ERROR: {str(e)[:160]}")
            except Exception as e:
                row["error"] = str(e)[:400]
                print(f"   SETUP ERROR: {str(e)[:200]}")
            finally:
                teardown(eng, pid, wt)
            results.append(row)
            print()
    finally:
        eng.close()

    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as fh:
        json.dump(results, fh, indent=2)
    _summary(results)
    print(f"\nwrote {out_path}")


def _health_digest(h):
    nodes = re.search(r"[Nn]odes?[:\s]+([\d,]+)", h)
    docs = re.search(r"[Dd]ocs?[:\s]+([\d,]+)", h)
    vecs = re.search(r"[Vv]ectors?[:\s]+([\d,]+)", h)
    return (f"nodes={nodes.group(1) if nodes else '?'} "
            f"docs={docs.group(1) if docs else '?'} "
            f"vecs={vecs.group(1) if vecs else '?'}")


def _fmt(v):
    return "  -  " if v is None else f"{v:.3f}"


def _summary(results):
    print("=" * 72)
    print("STRATEGY TOURNAMENT — mean over stories (higher recall = better)")
    print("=" * 72)
    agg = {}
    for row in results:
        for name, sc in row.get("strategies", {}).items():
            if "error" in sc:
                continue
            a = agg.setdefault(name, {"rm": [], "pg": [], "ra": [], "bn": [], "pr": [], "np": []})
            for k, dst in (("recall_modified", "rm"), ("recall_modified_page", "pg"),
                           ("recall_all", "ra"), ("recall_all_basename", "bn"),
                           ("precision", "pr")):
                if sc.get(k) is not None:
                    a[dst].append(sc[k])
            a["np"].append(sc["n_predicted"])

    def mean(xs):
        return sum(xs) / len(xs) if xs else 0.0
    rows = sorted(agg.items(), key=lambda kv: mean(kv[1]["pg"]), reverse=True)
    print(f"{'strategy':16s} {'recall_mod':>11s} {'recall_page':>11s} "
          f"{'recall_all':>11s} {'recall_bn':>11s} {'precision':>11s} {'avg_pred':>9s}")
    for name, a in rows:
        print(f"{name:16s} {mean(a['rm']):>11.3f} {mean(a['pg']):>11.3f} "
              f"{mean(a['ra']):>11.3f} {mean(a['bn']):>11.3f} "
              f"{mean(a['pr']):>11.3f} {mean(a['np']):>9.1f}")
    if rows:
        print(f"\nWINNER (by recall_modified_page): {rows[0][0]}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pr", type=int, help="single PR id (pipeline validation)")
    ap.add_argument("--ids", help="comma-separated PR ids")
    ap.add_argument("--pilot", action="store_true", help="use eval/data/pilot_ids.json")
    ap.add_argument("--max-commits", type=int, default=500)
    ap.add_argument("--out", default=os.path.join(HERE, "data", "phase1_results.json"))
    args = ap.parse_args()

    if args.pr:
        ids = [args.pr]
        args.out = os.path.join(HERE, "data", f"phase1_pr{args.pr}.json")
    elif args.ids:
        ids = [int(x) for x in args.ids.split(",") if x.strip()]
    elif args.pilot:
        with open(PILOT_IDS, encoding="utf-8") as fh:
            ids = json.load(fh)
    else:
        ap.error("specify --pr, --ids, or --pilot")
    run(ids, args.out, args.max_commits)


if __name__ == "__main__":
    main()
