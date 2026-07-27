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
import re
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import (  # noqa: E402
    Engram, add_worktree, add_snapshot, remove_worktree, canon, extract_paths, WORKTREE_ROOT,
)
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


# Co-change/history is the most predictive signal (it won the hard stories), so
# treat it as golden in ranking — the OLD signal-COUNT sort buried single-signal
# co-change files under multi-signal concept noise.
_GOLDEN = {"history", "cochange"}


def signal_rank_key(path, sigs):
    s = set(sigs)
    g = bool(s & _GOLDEN)
    if g and len(s) >= 2:
        tier = 0                       # co-change + corroboration
    elif s <= _GOLDEN:
        tier = 1                       # co-change/history alone — trust it
    elif len(s) >= 2:
        tier = 2                       # multi-arm (concept+graph)
    elif s == {"concept"}:
        tier = 3
    elif s == {"graph"}:
        tier = 4
    else:
        tier = 5
    return (tier, path.count("/"), path)   # shallower paths (local pages) first


def render_dossier(title, rows, per_layer_cap=18):
    """rows = list of (path, signals_set). FLAT, layer-grouped checklist with
    anti-anchoring framing. Cap is PER-LAYER (not a global top-N) so a flood of
    concept-found server files can't evict the entire .resx/.sql/.ts layer — the
    exact mechanism that dropped real companions past the old global cap=40."""
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
        "## Completeness checklist — satisfy ALL that apply before you finish",
        "A partial implementation scores as a failure. For the change you make:",
        "- **Every page touched:** edit BOTH the `.aspx`/`.ascx` markup AND its "
        "`.aspx.vb`/`.ascx.vb` code-behind (and `.designer.vb` if present) — not just one.",
        "- **Every user-facing string added/changed:** update the `.resx` resource in "
        "EVERY language present (e.g. `.resx` + `.en/.de/.es/.no/.pt/.sl`), not only the default.",
        "- **Every schema / setting / column change:** include the SQL migration script.",
        "- **Every `.ts` you change that compiles into a committed bundle:** update the bundle.",
        "- Before finishing, re-scan the candidate list and confirm you addressed each file "
        "the change genuinely requires. Do not submit a core-only implementation.",
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
        items = sorted(items, key=lambda r: signal_rank_key(r[0], r[1]))
        # ALWAYS keep golden (co-change/history, tier<=1) — they are high-precision
        # and must never be evicted; cap only the concept/graph tail per layer.
        golden = [r for r in items if signal_rank_key(r[0], r[1])[0] <= 1]
        tail = [r for r in items if signal_rank_key(r[0], r[1])[0] >= 2][:per_layer_cap]
        md.append("")
        md.append(f"**{layer}:**")
        for p, arms in golden + tail:
            md.append(f"- `{p}`  [{'|'.join(sorted(arms))}]")
    return "\n".join(md)


# ── noise + family expansion ────────────────────────────────────────────────
_VENDOR = ("bower_components/", "node_modules/", "/vendor/", "vendor/",
           "/lib/", "/libs/", "/dist/", "scripts/lib/")


def _is_noise(p):
    pl = p.lower()
    if any(v in pl for v in _VENDOR):
        return True
    if pl.endswith((".min.js", ".min.css", ".map")):
        return True
    if pl.endswith(".css"):       # third-party styling rarely the change target
        return True
    return False


def _wt_real(wt, canon_path):
    """Map a canon path back to an existing worktree file (Windows FS is
    case-insensitive, so the lowercase canon resolves). Try Site/ then root."""
    for prefix in ("Site", ""):
        cand = os.path.join(wt, prefix, *canon_path.split("/"))
        if os.path.isfile(cand):
            return cand
    return None


def expand_cochange(eng, pid, prov, raw_of):
    """GENERIC precision+recall mechanism (no per-repo hardcoding): from a small
    high-confidence seed (files corroborated by >=2 arms or by git history),
    ask which files HISTORICALLY CO-CHANGED with them. Co-change is what tells a
    real companion (the settings store, the resx set, the compiled bundle, the
    other side of a bug) apart from concept-flood that never shipped together.
    Matched files are promoted to the 'cochange' (golden) tier. This is the stage
    to port into Engram itself as a change-set capability."""
    seed = sorted({raw_of[c] for c, s in prov.items()
                   if len(s) >= 2 or ("history" in s)})[:12]
    if len(seed) < 4:                       # thin seed -> widen to top arm hits
        seed = sorted(set(raw_of.values()))[:10]
    if not seed:
        return 0
    added = 0
    calls = [("find_similar_changes", {"files": seed, "top": 8, "max_commits": 800}),
             ("detect_incomplete_changes", {"edited_files": seed, "max_partners": 8})]
    for tool, extra in calls:
        try:
            out = eng.tool(tool, {"project_id": pid, **extra})
            for p in extract_paths(out):
                c = canon(p)
                if _is_noise(c):
                    continue
                if c not in prov:
                    prov[c] = {"cochange"}
                    raw_of[c] = p
                    added += 1
                else:
                    prov[c].add("cochange")   # promote concept/graph hit to golden
        except Exception as e:
            print(f"  cochange {tool} error: {str(e)[:100]}", file=sys.stderr)
    return added


def expand_families(prov, wt):
    """Framework-generic companion expansion (not OciusX-specific) against files
    that EXIST in the base worktree: .NET WebForms code-behind/designer siblings
    of any surfaced page (high precision), and the full .resx localization set —
    but the resx-set ONLY from a co-change/history-confirmed anchor, so a noisy
    concept-only resx doesn't drag in every language. Siblings inherit signals."""
    import glob
    add = {}

    def put(sib, sigs):
        if sib not in prov and _wt_real(wt, sib):
            add.setdefault(sib, set()).update(sigs)

    for p, sigs in list(prov.items()):
        pl = p.lower()
        golden = bool(sigs & _GOLDEN)
        if pl.endswith((".aspx", ".ascx")):
            for ext in (".vb", ".cs", ".designer.vb", ".designer.cs"):
                put(p + ext, sigs)
        elif pl.endswith((".aspx.vb", ".aspx.cs", ".ascx.vb", ".ascx.cs")):
            put(p.rsplit(".", 1)[0], sigs)        # the markup shell
        if pl.endswith(".resx") and golden:
            real = _wt_real(wt, p)
            if real:
                d = os.path.dirname(real)
                stem = os.path.basename(p).split(".")[0]   # text.en.resx -> text
                for f in glob.glob(os.path.join(d, stem + "*.resx")):
                    rel = os.path.relpath(f, wt).replace("\\", "/").lower()
                    put(canon(rel), sigs)
    for k, v in add.items():
        prov.setdefault(k, set()).update(v)
    return len(add)


_PROV_CACHE_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data", "wi_provenance.json")


def _ac_provenance(wid):
    """AC/description authorship for a work item, cached to disk so
    --reuse reruns are deterministic and offline. Returns None on any
    failure (no PAT, network down) — provenance is an annotation, never
    a hard dependency."""
    cache = {}
    if os.path.exists(_PROV_CACHE_PATH):
        try:
            cache = json.load(open(_PROV_CACHE_PATH, encoding="utf-8"))
        except Exception:
            cache = {}
    key = str(wid)
    if key not in cache:
        try:
            import importlib.util
            spec = importlib.util.spec_from_file_location(
                "ado_fetch", os.path.join(os.path.dirname(os.path.abspath(__file__)), "ado_fetch.py"))
            m = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(m)
            cache[key] = m.workitem_field_provenance(wid)
            json.dump(cache, open(_PROV_CACHE_PATH, "w", encoding="utf-8"), indent=1)
        except (Exception, SystemExit):  # ado_fetch sys.exits without a PAT
            return None
    return cache.get(key)


def _acceptance_label(wi):
    """Header for the AC block, carrying provenance when known.

    User ruling 2026-07-10: one-liner stories sometimes get ACs
    back-filled by the implementer (often AI-assisted) after the team
    wrote the story — the two biggest implementer-authored AC blobs in
    the eval set sit on exactly the two crater scores (1938=47.6,
    1913=66.7). Team-authored ACs are spec; back-filled ACs are hints
    the plan must verify against the description and merged exemplars."""
    label = "Acceptance"
    p = _ac_provenance(wi.get("id")) if wi.get("id") else None
    if p and p.get("acceptance_history"):
        first = p["acceptance_history"][0]
        label = f"Acceptance (written by {first['by']} on {first['date']}"
        if p.get("created_by") and first["by"] != p["created_by"]:
            label += (f"; the story itself was created by {p['created_by']} — "
                      "these criteria were back-filled later, treat them as "
                      "implementer notes to verify against the description and "
                      "existing merged work, NOT team-committed spec")
        label += ")"
    return label


def build_dossier(eng, pid, rec, wt):
    """Single source of truth: the native get_change_set tool — the exact
    artifact an agent calls in production. Its markdown IS the dossier the
    engram arm reads (concept + history + co-change + .NET family expansion +
    vendor denoise, co-change-first ranked, completeness checklist), so the A/B
    measures the real product and inherits every get_change_set fix (greedy
    path extraction, case-insensitive co-change, broad self-filtering seed).
    Parse its output into prov(canon->signals) for the prov.json dump.
    Returns (md, prov)."""
    s = rec["story"]
    # ALL linked work items, not just [0]: PR1937 was linked to TWO bugs
    # (691 + 817) describing different symptoms of the same defect cluster;
    # the convenience fields dropped the second and the missing symptom cost
    # the eval agent the file-set match. Input parity = everything the dev
    # could see on the PR's work items.
    parts = []
    for wi in s.get("work_items") or [s]:
        if wi.get("title"):
            parts.append(wi["title"])
        if wi.get("description"):
            parts.append(wi["description"])
        if wi.get("acceptance"):
            parts.append(_acceptance_label(wi) + ":\n" + wi["acceptance"])
        for li in wi.get("linked_items", []) or []:
            parts.append(f"[linked {li.get('type','item')} {li.get('id','')}] "
                         f"{li.get('title','')}\n{li.get('description','')}")
    story = "\n\n".join(p for p in parts if p) or s["title"]
    args = {"project_id": pid, "story": story}
    cd = (rec.get("closed_date") or "")[:10]
    if cd:
        args["merged_before"] = cd  # leak-free: the PR under test is excluded
    md = eng.tool("get_change_set", args)
    if "TOOL_ERROR" in md:
        print(f"  get_change_set error: {md[:160]}", file=sys.stderr)
        return md, {}
    # LEAK-SAFETY (2026-07-10): get_change_set's '## Review rules' section
    # renders fix-exemplar ‹house fix› ```diff blocks pulled from the FULL
    # antipattern corpus, which is NOT bounded by merged_before — so in an
    # eval it can leak the PR-under-test's OWN merged fix. Strip those diff
    # blocks from the saved eval dossier (the one-line rules themselves are
    # kept; only the concrete leaked hunks are removed). Production is
    # unaffected — this strip is eval-only. See the iteration-delta-mining
    # memory's leak-safety caveat.
    before = md
    md = re.sub(r"\n\s*‹house fix›\n```diff\n.*?\n```\n", "\n", md, flags=re.S)
    n_stripped = before.count("‹house fix›") - md.count("‹house fix›")
    if n_stripped:
        print(f"  stripped {n_stripped} leaked fix-exemplar hunk(s) from eval dossier")
    prov = {}
    for m in re.finditer(r"^- `([^`]+)`\s*\[([^\]]*)\]", md, re.M):
        c = canon(m.group(1))
        if not _is_noise(c):
            prov.setdefault(c, set()).update(t for t in m.group(2).split("|") if t)
    n_golden = sum(1 for sig in prov.values() if sig & {"history", "cochange"})
    print(f"  change_set: {len(prov)} files ({n_golden} golden co-change/history)")
    return md, prov


INDEX_MAP = os.path.join(P2_DATA, "index_map.json")


def _load_map():
    try:
        return json.load(open(INDEX_MAP, encoding="utf-8"))
    except Exception:
        return {}


def _save_map(m):
    json.dump(m, open(INDEX_MAP, "w", encoding="utf-8"), indent=2)


def _project_valid(eng, pid):
    """True if a kept index still exists AND has content (not a partial build)."""
    if not pid:
        return False
    h = eng.tool("project_health", {"project_id": pid})
    return ("graph_nodes" in h and "lancedb_vectors" in h
            and "TOOL_ERROR" not in h and "❌" not in h)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pr", type=int, required=True)
    ap.add_argument("--reuse", action="store_true",
                    help="reuse a kept index if valid (skips the ~230s re-index — "
                         "use for dossier-logic experiments)")
    args = ap.parse_args()
    os.makedirs(P2_DATA, exist_ok=True)
    os.makedirs(P2_WT_ROOT, exist_ok=True)

    rec = {r["pr_id"]: r for r in json.load(open(rp.CORPUS, encoding="utf-8"))}[args.pr]
    base = rec["base_commit"]
    print(f"PR {args.pr} @ base {base[:8]} — {rec['story']['title'][:60]}")

    # Agent base trees — what the implementing agents read. These MUST be plain
    # snapshots (no .git): a linked git worktree lets an agent `git log --all` the
    # merged target PR and copy the answer (leakage). (re)create as a snapshot if
    # missing OR if a stale leaky .git worktree is present.
    wt_alone = os.path.join(P2_WT_ROOT, f"pr{args.pr}_alone")
    wt_engram = os.path.join(P2_WT_ROOT, f"pr{args.pr}_engram")
    for wtp in (wt_engram, wt_alone):
        has_site = os.path.isdir(os.path.join(wtp, "Site"))
        is_leaky = os.path.exists(os.path.join(wtp, ".git"))
        if not has_site or is_leaky:
            if is_leaky:
                remove_worktree(wtp)  # detach the linked worktree before re-snapshotting
            add_snapshot(base, wtp)

    eng = Engram()
    imap = _load_map()
    try:
        pid = imap.get(str(args.pr))
        if args.reuse and _project_valid(eng, pid):
            print(f"  REUSE kept index {pid} (skipped re-index)")
        else:
            pid, idx_wt, idx_secs, _ = rp.setup_index(eng, rec)
            print(f"  indexed in {idx_secs:.0f}s")
            # KEEP the index worktree: co-change tools (find_similar_changes,
            # search_history, detect_incomplete_changes) re-walk the git repo at
            # rec.directory at QUERY time — removing it silently zeroes co-change.
            imap[str(args.pr)] = pid
            _save_map(imap)                   # KEEP the project — do NOT delete (reusable)
        t0 = time.time()
        dossier_md, prov = build_dossier(eng, pid, rec, wt_engram)
        print(f"  dossier built in {time.time() - t0:.1f}s")
        with open(os.path.join(P2_DATA, f"pr{args.pr}_prov.json"), "w", encoding="utf-8") as fh:
            json.dump({k: sorted(v) for k, v in prov.items()}, fh, indent=2)
    finally:
        eng.close()                          # NOTE: index kept for --reuse

    # write dossier + manifest
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
