# Engram "brain" audit program (2026-08-28)

External audit verdict: Engram has the right ingredients for a project brain,
but the OciusX integration is not reliable enough to trust autonomously. The
largest problems are broken agent wiring, incomplete scope discovery, noisy
evidence, and safety tools that can silently lose evidence.

This directory holds one improvement document per vital CAPABILITY (not per
tool: tools in a row share a substrate, so per-tool docs would duplicate
findings and fixes). Each document is written before its implementation
starts and is the auditor's checklist afterwards.

## Document rules (learned the expensive way — see blast-radius rounds 1-5)

1. Every defect claim is grep-verified against live code with `file:line`.
   Claims from the external audit are re-verified, not copied.
2. Live evidence is the ACTUAL tool output on the OciusX index (project
   `5a35e8e0-d37a-41b3-a250-a26957e7aedb`), not a paraphrase.
3. The redesign section is split: **A. defects to fix now** (cheap, each
   with a test that fails first) vs **B. redesign that needs an A/B or
   golden-suite result first** — engines are not rebuilt on assertion.
4. A measurable acceptance gate, stated before implementation.
5. A disposition table filled at implementation time: every item is
   `fixed@mechanism` or `deferred@reason`; nothing falls off the list.
6. Definition of done for the implementation: full
   `cargo test -p engram_server --tests --lib --no-fail-fast` green,
   commit + push, release build, deploy to the live daemon, live re-run of
   the evidence section, memory updated.

## Rows and status

| # | Capability | Main tools | Doc | Status |
|---|---|---|---|---|
| 0 | Integration P0 block | generate_agent_integration, produce_claude_md, tool surface | (this file, below) | **DONE + deployed + live-verified 2026-08-28** (fef66ca) — see disposition |
| 1 | Story-to-change scope | get_change_set (+ detect_incomplete_changes, find_similar_changes) | [`03-story-to-change-scope.md`](03-story-to-change-scope.md) | **slices 1-3 IMPLEMENTED + deployed 2026-08-29 05:06** (8ea94c9 D10: permission-gates cut stated in markdown + JSON — live). Open: A3/D4 precision — blocked on the user's A/B opt-in (see below) |
| 2 | Follow the code before editing | get_method_edit_context, check_edit_safety, get_page_context | [`02-edit-context-and-edit-safety.md`](02-edit-context-and-edit-safety.md) | **IMPLEMENTED + deployed + live-verified 2026-08-28** (604f488; §6 disposition, §7 live gates 20/20) |
| 3 | Pre-commit defect prevention | pre_commit_review, pre_push_audit | [`05-pre-commit-gates.md`](05-pre-commit-gates.md) | **slices 1-5 IMPLEMENTED + deployed 2026-08-29 04:31** (777c951 gate outcomes; 9da8522 DEGRADED; 6f149c8 in-gate caps live; af737da unwired failed-lookup ⇒ skip; d1252ca pre_push_audit INACTIVE on an empty rule namespace — live). Remaining: OciusX rule ingestion is a user action (ADO PAT) |
| 4 | Exact entity/consumer discovery | get_concept_footprint, find_symbol_references | [`04-concept-and-consumer-discovery.md`](04-concept-and-consumer-discovery.md) | **slices 1-11 IMPLEMENTED + deployed 2026-08-29 05:38** (… a6825ce consumer roles; 05ec8fb Swedish stems; edfe856 + c84b407 export-role fixes from two live findings — live: redovisningskategori export 4 / read 2). A4 alias layer deferred on evidence |
| 5 | House pattern + UI conformance | find_implementation_pattern, analyze_file_coding_style, get_ui_conformance (M0 A/B) | [`07-house-pattern-and-ui-conformance.md`](07-house-pattern-and-ui-conformance.md) | **slices 1-4 IMPLEMENTED + deployed; M0 A/B RUN 2026-08-29 — NEGATIVE (1937: 61.9 → 41.9 file-F1, n=3; 1908 saturated) ⇒ the UI Family Catalog / get_ui_conformance is dropped per the spec's kill-switch (07 §7c)** |
| 6 | NL project understanding | ask_codebase | — | healthy (M1 shipped 2026-08-21); larger golden suite later |
| 7 | Causal UI/data tracing | trace_ui_event, trace_data_flow, find_connection_path | [`06-causal-trace-engine.md`](06-causal-trace-engine.md) | **slices 1-3 IMPLEMENTED + deployed 2026-08-29** (b0369f6 method-node attribution; e400653 bounded follow + qualified callee resolution; release11 qualified ENTRY resolution + find_connection_path kind set). Live: no false step, 7/10 calls resolve; follow re-verified after release11 (§7c) |
| 8 | Security / settings / durable laws | map_guards_and_settings, repo rules, immune_check | [`08-guards-and-settings.md`](08-guards-and-settings.md) | **slices 1-5 IMPLEMENTED + deployed 2026-08-29 04:31** (95ac7d8, 8ff187f, 195d3c8, f4bc30f, 1134950 bare in-class helpers + DAL object scoping). LIVE: ROLE-ONLY 21 → 2; bulk endpoints `via CanUserBulkUpdate [object]`. Open: B policy model |
| 9 | "You forgot the other side" | edit sessions, detect_incomplete_changes, find_similar_changes | folded into row 1 doc | queued |
| 10 | Change exposure / edit risk | impact_analysis, compute_blast_radius, check_edit_safety | `docs/AUDIT_R5.md` + ImpactEngine one-hop slice | round 5 accepted 2026-08-24; orientation tool, not authority |

## Blocked on the user (2026-08-29) — everything else in rows 0-8 is fixed@mechanism or deferred@evidence

| Item | Needs | Why it cannot proceed autonomously |
|---|---|---|
| Row 1 precision (A3 / D4): weak-tier policy vs implementation score | opt-in to the in-session Opus/Sonnet A/B via the Workflow tool (or the OciusX `/story` dry-run) | the 5-PR gate shows 5 % precision is the defect; no retrieval knob moves it (03 §7) — only an implementation-score A/B can decide the weak-tier cut |
| ~~Row 5 M0 A/B~~ | done 2026-08-29 (user opted in) | NEGATIVE — idea dropped (07 §7c) |
| Row 3 rule ingestion on OciusX | a fresh Azure DevOps PAT + `ingest_quality_gates` | `pre_push_audit` is honest now (INACTIVE) but checks nothing until rules are ingested (05 §7d) |


Implementation order (auditor's, adopted): 0 → 2 → 1 → 4 → 3 → 7 → 5 → 10/dream. Row 0b (index GC) was inserted when found live.

## Row 0b — Index layer: hourly GC deleted every incrementally indexed file (found live 2026-08-28)

Not on the auditor's list; found while re-running row-2 evidence after a
deploy. Symptom: `get_codebase_overview` 18,144 → 9,904 functions between
two runs the same day, `query_graph_nodes` blank for `Check_pr_id`,
`get_page_context` 0 methods — while `get_index_freshness` said "index is
current and the watcher is active". Mechanism (daemon log): the hourly GC
purged the GRAPH against `last_full_index_generation` with the
KeepLatestOnly policy read as `generation != baseline`; every node an
incremental update had written at a NEWER generation was deleted, the
watcher re-indexed the same 175 files as `[node_missing]`, the next tick
deleted them again. The GC's own comment stated the right invariant
("never the incremental counter"); the store call did not implement it.

| Item | Disposition |
|---|---|
| Hourly GC deletes nodes newer than the last full index | **fixed** — `GraphStore::purge_generations_below` (stale = OLDER than the baseline); GC uses it and logs counts; test `hourly_gc_keeps_nodes_newer_than_the_last_full_index` reproduces the live deletion and fails on the old code |
| Manual `incremental_indexing_gc` defaulted the graph purge to `active_generation` | **fixed** — defaults to the full-index baseline, skips with a message when missing; test `manual_gc_defaults_to_the_full_index_baseline` |
| "Index is current" while the graph oscillates | **open (row 10 / freshness)** — `get_index_freshness` reports file mtimes, not graph integrity; a node-count-vs-file-count consistency check belongs in `project_health` |
| VB sidecar does not support incremental `invalidate` (daemon log: "redeploy the sidecar") | **open** — incremental VB updates fall back to a full `begin_project`; correct but slow; redeploy the sidecar build |

## Row 0 — Integration P0 block (disposition)

All six audit claims re-verified before fixing (grep evidence in the commit).
Live verification after deploy (2026-08-28, OciusX): `generate_agent_integration`
wrote the rules file (live id + directory + recovery path) and `AGENTS.md`, skipped the
existing `settings.json` with a hooks block that now says `diff=staged`, emitted the
`.mcp.json` entry; `produce_claude_md` (splice) collapsed the two engram blocks to one
(backup `CLAUDE.md.<ts>.bak`); `pre_commit_review` header reads `Gates run: 17/17`.
New customer-repo artifacts (`AGENTS.md`, `CLAUDE.md.*.bak`) added to `.git/info/exclude`.

| Item | Disposition |
|---|---|
| Generated `detect_incomplete_changes(files=…)` (3 sites) — real field is `edited_files` | **fixed** — text corrected; new contract test `agent_integration_tests::*_only_name_real_tools_and_fields` binds every `tool(param=` mention in generated text (rules, hooks, plan_user_story checklist, concept-footprint footer, AGENTS.md) to the live tool registry's input schemas (`Engram::tool_registry()`), so this class cannot recur |
| Generated `pre_commit_review(diff_source="staged")` (3 sites) — real field is `diff` | **fixed** — same mechanism |
| Rules text said "eleven gates"; header said `Gates run: N/10`; registry has 17 | **fixed** — both derive from `gates::all_gates().len()`; tests `workflow_rules_report_the_registered_gate_count`, `header_reports_the_registered_gate_total_not_a_literal` |
| Stale project id baked into OciusX files after the 2026-07-19 data-dir reset | **fixed** — rules now carry the indexed directory and a recovery path (`list_projects`, match directory, regenerate); OciusX `.claude/orchestration/config.md` + `.claude/settings.json` hook text corrected by hand; rules file + AGENTS.md regenerated from the deployed binary |
| No AGENTS.md; `.mcp.json` exposes only DevOps | **fixed** — `generate_agent_integration` now writes `AGENTS.md` (never clobbers an existing one) and EMITS a mergeable `.mcp.json` entry for this binary (never written: repo-committed file, machine-specific path). OciusX `.mcp.json` is Visual Studio format (`servers`), left for the user to merge |
| Duplicate `engram:begin/end` blocks in OciusX CLAUDE.md | **fixed by rerun** — current splicer already collapses first-begin..last-end (`produce_claude_md_service.rs:1880-1925`); the duplicate came from an older splicer. Re-run with `overwrite_existing=true, merge_mode=splice` |
| "Encoding corruption" in OciusX CLAUDE.md | **not reproduced** — file is valid UTF-8, zero mojibake/U+FFFD lines (python scan); likely a cp1252 terminal on the auditor's side. Left as-is |
| 143 tools advertised (`advertise_all_tools` default true) | **deferred, with reason** — the only existing knob hides the 33 `[.NET legacy]` tools, which are the RELEVANT ones for a VB.NET WebForms shop; flipping it hurts OciusX. The real fix is a tiered surface (core decision surface ≈ the 10 rows' tools, extended, legacy) — designed in row 2/3 docs once the vital-tool list is stable. The generated CLAUDE.md/rules ARE the decision surface today |
