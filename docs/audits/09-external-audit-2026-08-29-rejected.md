# External audit 2026-08-29 — verdict REJECTED — the GOLD-STANDARD remediation checklist

Source: the external auditor's report (verbatim findings below, paths as cited). Mandate from the
product owner (2026-08-29): loop until every item is gold standard, or show with evidence that it
is impossible; 80/20 on the items that are genuinely out of reach; every decision raised as a
multiple-choice question; nothing deferred by the implementer; never commit into the OciusX repo.

## Verdict (auditor): REJECTED

"A substantial amount was genuinely implemented and tested, but 'everything above is fixed,
implemented, and tested' is demonstrably false. Four reopened P0s, several deferred capabilities,
and a serious live index-corruption problem affecting OciusX right now."

## P0-1 — OciusX's searchable generation is corrupted

project_health: Health OK, generation 831, 49,471 nodes, 1,349,696 edges, 142,982 indexed
documents — but the active searchable corpus contains only 56 VB.NET chunks and 49 ASPX chunks.
`rg` finds `redovisningskategori` in 25 .vb/.aspx/.ascx files; grep_project finds four RDL files;
get_concept_footprint reports 4 literal matches (documented 25/25); ask_codebase misses the
implementation family.

Mechanism (GC/watcher race):
1. Tokio's interval ticks immediately at daemon start — `actors/gc.rs:12`.
2. GC only avoids indexing work recorded in `active_indexing_count` — `actors/gc.rs:29`.
3. Watcher updates call `update_project_impl` without registering — `actors/watcher.rs:248`.
4. Synchronous `update_project(wait=true)` has the same hole — `handlers/project_tools.rs:1789`.
5. Incremental updates copy unchanged documents into the new generation — `project_tools.rs:1885`.
6. GC concurrently removes every search generation except the published one — `actors/gc.rs:137`.

Log: GC starts 06:00:57 (engram.log:305228); watcher starts OciusX generation 830 (:305415); GC
deletes everything except 829 while 830 is being built (:308230); later purges publish the
incomplete generations 830 and 831 (:308682, :309150). The graph purge fix is valid; the search
generation lifecycle remains unsafe.

## P0-2 — Health and freshness falsely authorize broken evidence

project_health initializes "Health: OK" and converts provider failures to zeros/defaults
(`project_tools.rs:2159`, `:2173`). get_index_freshness checks timestamps and modified files but
never validates active-generation completeness (`project_tools.rs:2252`) — "index is current and
watcher active" while most production code is absent. Invalidates downstream "green" results from
search-backed gates and tools.

## P0-3 — get_change_set still fails the reference OciusX story

Main-reporting-category story: 149 candidates, 18 omissions; still extracts "main, reporting,
category"; does not render ProductionCodeListMainCategory.aspx/.vb, rk_redovisningskategorier.sql,
Redovisningskategorier.vb, api-redovisning.vb, iFalt.dbml. expand_concepts=true → 158 candidates /
34 omissions, still misses central files. Re-evaluate after the index repair, but the
TaskSpec/concept problem is independently documented (03 §6 A1). Also: co-change still walks git
interactively (`planning_tools.rs:5957`); expansion is opt-in (recall 89.2 → 86.5); precision ~5 %;
live execution ~11.7 s.

## P0-4 — Pre-commit still has silent fail-open paths

StateGate turns graph provider failures into "no readers/writers" (`gates.rs:1160`, `:1166`).
Per-hit search failures discarded: antipattern (`:1455`), product intent (`:2846`), co-added family
(`:3048`). A structurally incomplete index does not return an error, so search-backed gates can
pass against 5–10 % of the corpus unless pre-commit depends on a real integrity result.

## Integration regression

OciusX AGENTS.md is correct (AGENTS.md:30) but CLAUDE.md:55 is invalid: the generator emits
`detect_incomplete_changes(files=[...])` (`produce_claude_md_service.rs:445`) while the request
accepts only `edited_files` (`requests.rs:824`). The "cannot recur" contract test tests the
AGENTS renderer, not produce_claude_md.

## Capability scorecard (auditor)

| Capability | Verdict |
|---|---|
| Integration pack | Partial; Claude generator still emits an invalid request |
| get_change_set | Failed live reference story |
| Edit context / edit safety | Verified |
| Pre-commit prevention | Strong improvement, but still fail-open |
| Concept footprint | Implementation improved; operationally broken by index corruption |
| Pattern/style/UI | Pattern/style improved; UI catalog intentionally dropped |
| ask_codebase | Degraded by missing searchable evidence |
| Causal tracing | Verified |
| Guards/settings | Verified |
| Change completeness/history | Explicitly queued (row 9) |
| Impact/blast | Still advisory one-hop; no shared ImpactEngine/ChangeSpec |
| Dream | No retrieval-bias ablation or effectiveness evaluation |
| 143-tool surface | Deferred |

61 focused tests passed; no test reproduces "daemon starts → immediate GC → watcher incremental
update → copy-forward overlaps purge → incomplete generation published".

## The 10 vital capabilities (product owner, 2026-08-29) — each must reach gold standard

| # | Capability | Main tools | Auditor's ruling | Gold-standard target (measurable) |
|---|---|---|---|---|
| 1 | Story-to-change scope | get_change_set | best proven concept; live output needs P0 work | reference story renders the implementation family; concept extraction resolves domain entities (EN↔SV); co-change precomputed; ≤ 5 s |
| 2 | Follow the code before editing | get_method_edit_context, get_page_context | P0 honesty/completeness | verified by auditor; keep parity 20/20 on every release |
| 3 | Pre-commit defect prevention | pre_commit_review, pre_push_audit, validation tools | P0: failed gates can still render green | no fail-open path: every provider/per-hit failure → Degraded; index-integrity dependency |
| 4 | Exact entity/consumer discovery | get_concept_footprint, find_symbol_references | P0: materially incomplete on OciusX | 25/25 literal on the reference concept after corpus repair; freshness reports generation completeness |
| 5 | House implementation + UI conformance | find_implementation_pattern, analyze_file_coding_style, get_ui_conformance | major missing capability | pattern/style verified; get_ui_conformance: decision by the owner (question) |
| 6 | NL project understanding | ask_codebase | best current; expand evaluation | golden suite ≥ 30 questions on OciusX with abstain/status metrics |
| 7 | Causal UI/data tracing | trace_ui_event, trace_data_flow, find_connection_path | P0 for bug diagnosis | verified by auditor; keep |
| 8 | Security, settings, durable laws | map_guards_and_settings, repo rules, immune_check | valuable, incomplete enforcement | rules enforced in gates (not advisory), negative-path evidence |
| 9 | "You forgot the other side" | edit sessions, detect_incomplete_changes, find_similar_changes | proven value; needs denoising + precomputation | precomputed co-change/family cohorts; measured false-positive rate |
| 10 | Change exposure and edit risk | impact_analysis, compute_blast_radius, check_edit_safety | advisory, not an authority | shared ImpactEngine/ChangeSpec; one authority for the count |

## Disposition (filled as work lands — every row ends fixed@commit+live, or impossible@evidence)

| Item | Status |
|---|---|
| P0-1 GC/watcher race + OciusX corpus repair | **fixed@2f6fc31 (sweep24 131/0); release 19 + in-place OciusX repair pending** — GREEN locally 2026-08-29 09:35 — 3 race reproductions (engine purge semantics, slot registration, the auditor's race) + delayed first sweep; fix: KeepLatestOnly purges only generations OLDER than the published one, `ActiveIndexingSlot` held by every update, `purge_project_old_gens` guards itself, first sweep after 1 h; sweep24 running → commit → release 19 → deploy → full OciusX re-index → 25/25 check |
| P0-2 health/freshness integrity | RED test + patch prepared (generation completeness = code chunks in the published generation vs tracked files; health verdict computed; freshness `generation_complete`) — runs after sweep24 |
| P0-3 change-set reference story | **re-evaluate after the corpus repair** (the lexical arm was querying a 5 % corpus). Concept lever prepared: OciusX's own localization pairs bridge English story terms to the Swedish code vocabulary (`text.en.resx` "Main code category" ↔ `text.resx` "Huvudkodkategori", 11 such pairs for the category family) — an EN↔SV resource bridge in `resolve_story_concepts` is the generic mechanism; the parenthesized gloss `huvudredovisningskategori` → `redovisningskategori` split already exists (opt-in) |
| P0-4 pre-commit fail-open | RED test + patch prepared (review computes completeness once; antipattern / product_intent / co_added_family degrade on INCOMPLETE; StateGate + per-hit fetch errors degrade) — runs after sweep24 |
| Integration: produce_claude_md invalid request + real contract test | RED test + patch prepared (`edited_files`; contract test validates every `tool(param=` in the generated CLAUDE.md against the live tool router schemas) — runs after sweep24 |
| Row 9 change completeness/history (+ P0-3 latency: co-change walks git at call time) | open — design: `find_similar_changes` already keeps an incremental snapshot (`co_change_cache` + `data_dir/co_change/<pid>.bin`, keyed by walked commit oids); extract it into `services::co_change::refresh_snapshot(state, pid)` and warm it at the end of `index_project` / `update_project_impl` (spawned), so `get_change_set` and `find_similar_changes` only READ at call time (target ≤ 5 s live); denoising = report the false-positive rate of `detect_incomplete_changes` companions on the 5-PR gate |
| Impact/blast ImpactEngine/ChangeSpec (row 10) | open — evidence: `handlers::incoming_caller_edges_checked` (cap-exact, error-propagating, dedup by caller) is ALREADY the authority for check_edit_safety / get_method_edit_context and the unwired gate; `find_symbol_references` re-implements the rule locally (search_tools.rs:995) and `impact_analysis` counts per edge kind with a silent 100 cap (cognitive_tools.rs:198). Slice: route both through the shared function (moved to `services::impact_engine::incoming`), keep `compute_blast_radius` as the causal-transitive view that CITES the same one-hop number, and add a four-tool parity test on one fixture node |
| Row 6 ask_codebase golden suite | **authored 2026-08-29 09:47**: 35 OciusX questions (4 must-abstain, exact/usage/multi-hop/impact/rationale/compound/history/bug/requirements/ambiguous) in `eval/data/ask_golden_ociusx.jsonl` (git-ignored; customer strings stay out of source); run after the corpus repair with `python eval/ask_engine_golden.py <pid> eval/data/ask_golden_ociusx.jsonl` — gate: abstain 100 %, status-match ≥ 80 %, must-cite hit rate reported |
| Row 8 enforcement | open — evidence: `RepoRule {rule_id, file_pattern, rule_text, priority}`; ImmuneGate attaches matching rules to touched files as advice, AddedConventionsGate reads two rule DEMANDS by text (docs-on-API-change, null-guard). Slice: a checkable clause in rule text — `[check: forbid=<regex>]` / `[check: require=<regex> when=<regex>]` evaluated on the diff's ADDED lines of files matching `file_pattern` — turns a high-priority rule into a real finding (severity from priority; verdict can go red); the promoted quality-gate mandates get clauses at promotion time where the text is mechanical (SQL concatenation, missing check_pr_id on a client pr_id read, empty catch); negative-path test per clause kind |
| Dream ablation | **DECIDED by the owner 2026-08-29 09:32: run the on/off ablation on the 5-PR gate** — design: the dreamer writes `CoOccurrence` chunk↔chunk edges + file↔chunk `Dependency` edges into the graph; consumers are impact/blast (non-causal "often searched with" lines) and cognitive tools — NOT get_change_set. Ablation = (a) count dream-origin edges on OciusX, (b) render the 5 gate dossiers with those edges present vs removed (expected identical), (c) measure the impact/blast noise they add. Runs after the corpus repair |
| 143-tool surface | **DECIDED by the owner 2026-08-29 09:32: tiered surface (~20 core tools advertised, the rest behind an opt-in flag)** — open |
| Row 5 get_ui_conformance | **DECIDED by the owner 2026-08-29 09:32: build M1 (WebForms extractor + catalog) + M2 (`get_ui_conformance(region)`), then re-A/B with n ≥ 5 stories** — open; design: the index already stores `ui_container` / `control_layout` / control nodes with `container_type`, `layout_style`, `logical_grouping`, `css_class` metadata (what `get_ui_blueprint` reads). M1 = cluster those nodes into families (container type + css-class SET + control-type sequence, normalized per the spec's Layer 0), derive per-axis contracts with evidence counts, persist the catalog as docs in the `insights` namespace at index time; M2 = `get_ui_conformance(region)` matches the region's nodes to families → contract + every deviation, honest caps/coverage; then the n ≥ 5 arm-B replay decides the `/story` wiring |
