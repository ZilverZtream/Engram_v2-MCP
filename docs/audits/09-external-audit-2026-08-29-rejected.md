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
| 143-tool surface | Deferred **patched 2026-08-29 10:1x**: `tool_surface::CORE_TOOLS` (32) + `list_advanced_tools` + `advertise_all_tools=false` default; `tests/tool_surface_tier_tests.rs` staged; sweep 26 running; lands in release 20. **fixed@4c4e9c9 (sweep26)**; **LIVE r20 11:15**: `tools/list` → 32 advertised (ask_codebase … list_advanced_tools); `list_advanced_tools` → 112 callable advanced tools (filter works). |

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
| P0-1 GC/watcher race + OciusX corpus repair | **fixed@2f6fc31 (sweep24 131/0) + LIVE 2026-08-29 09:59**: release 19 deployed; in-place `repair_project scope=full` (same project id) → generation 832, files=2274 chunks=31,073 (was 105), tantivy docs 142,982 → 174,055; footprint `redovisningskategori` lexical coverage back to 32 files ("lexical complete, literal complete"); the git-history re-walk runs in the background. GREEN locally 09:35 — 3 race reproductions (engine purge semantics, slot registration, the auditor's race) + delayed first sweep; fix: KeepLatestOnly purges only generations OLDER than the published one, `ActiveIndexingSlot` held by every update, `purge_project_old_gens` guards itself, first sweep after 1 h; sweep24 running → commit → release 19 → deploy → full OciusX re-index → 25/25 check |
| P0-2 health/freshness integrity | **RED observed 09:58** (health opens with OK on a generation holding 20 % of its chunks; freshness has no completeness line) — patch applied, GREEN + sweep25 running; prepared: (generation completeness = code chunks in the published generation vs tracked files; health verdict computed; freshness `generation_complete`) — runs after sweep24 **fixed@a345710 + 9e4214d (sweep26 137 suites / 3,507 tests / 0 failed)**: `project_health` computes its verdict from generation completeness (code chunks in the published generation vs graph file nodes; CORRUPT / DEGRADED / OK) and `get_index_freshness` prints `generation_complete:`; `tests/index_integrity_tests.rs`. Landing incident: the per-item re-application dropped `generation_completeness_for`, caught by the swept-stash comparison and restored in 9e4214d (the swept tree is what shipped). **LIVE r20 2026-08-29 11:10 (release 20 = 9e4214d)**: `project_health` → `Health: OK — generation completeness: 31073 code chunks in generation 832 for 2274 tracked files (1366.4 %) — complete`; `get_index_freshness` → `generation_complete: true (…)`. On the 20 % corpus the same code path says CORRUPT (RED test); the auditor's false-OK cannot recur silently. |
| P0-3 change-set reference story | **re-evaluated on the repaired index 2026-08-29 10:00**: 3 of the 5 named files render (ProductionCodeListMainCategory.aspx/.vb, Redovisningskategorier.vb); `api-redovisning.vb` is a candidate but CUT by the per-layer tail cap (18) — a rendering defect; `rk_redovisningskategorier.sql` and `iFalt.dbml` are not retrieved at all — concepts are still `main, reporting, category` (the gloss `huvudredovisningskategori` → `redovisningskategori` is opt-in) and no data-layer alias links the DAL class to its table/dbml. RED test staged (`change_set_gloss_tests.rs`: the reference-story fixture with class, API, .sql table, .dbml and 40 noise files must render all four family files) + patch prepared (p03: glosses retrieve by default, `gloss` signal = tier 1 and tail-cap exempt; dry-run OK); the 5-PR recall gate (`eval/_recall_subset.py`) is re-run on the new binary before the commit so the earlier expansion regression (89.2 → 86.5) cannot recur unnoticed. Earlier note: re-evaluate after the corpus repair (the lexical arm was querying a 5 % corpus). Concept lever prepared: OciusX's own localization pairs bridge English story terms to the Swedish code vocabulary (`text.en.resx` "Main code category" ↔ `text.resx` "Huvudkodkategori", 11 such pairs for the category family) — an EN↔SV resource bridge in `resolve_story_concepts` is the generic mechanism; the parenthesized gloss `huvudredovisningskategori` → `redovisningskategori` split already exists (opt-in) |
| P0-4 pre-commit fail-open | **RED observed 09:58** (antipattern / product_intent / co_added_family passed against a 20 % corpus) — patch applied, GREEN + sweep25 running; prepared: (review computes completeness once; antipattern / product_intent / co_added_family degrade on INCOMPLETE; StateGate + per-hit fetch errors degrade) — runs after sweep24 **fixed@b6c3cce + 9e4214d (sweep26)**: `GateContext.search_index_note` (set from generation completeness) makes the search-backed gates (AntiPattern / ProductIntent / CoAddedFamily) DEGRADE on an incomplete generation; StateGate and per-hit `get_doc_by_pk` errors degrade instead of passing; `tests/pre_commit_integrity_tests.rs`. `pre_push_audit` reviewed: search errors propagate as tool errors and an empty match reports the rule count + any count failure — no fail-open path (closed@code-read). **LIVE r20 11:11**: `pre_commit_review diff=head` → verdict yellow, 17 gates run, 0 degraded, 0 failed on the complete generation (no INCOMPLETE note — correct); the degrade path is exercised by the RED test on a 20 % corpus. |
| Integration: produce_claude_md invalid request + real contract test | **RED observed 09:58** (`detect_incomplete_changes(files=` fails the live-schema contract) — patch applied, GREEN + sweep25 running; prepared: (`edited_files`; contract test validates every `tool(param=` in the generated CLAUDE.md against the live tool router schemas) — runs after sweep24 **fixed@83b966a (sweep26)**: `produce_claude_md` emits `detect_incomplete_changes(edited_files=[...])` and `tests/claude_md_contract_tests.rs` validates every emitted tool-call example against the live request schemas (so a renamed parameter fails the build, not the customer). **LIVE r20 11:16-11:17**: `produce_claude_md write_to_disk=true` on OciusX → `.claude/rules/engram-workflow.md` and `CLAUDE.engram.md` now read `detect_incomplete_changes(edited_files=[...])`; the stale 2026-08-28 root `CLAUDE.md` (engram-generated, git-excluded) was regenerated with merge+overwrite (backup kept in scratch; `.git/info/exclude` extended with `CLAUDE.engram.md`, `AGENTS.md`; OciusX tree clean). The tool's own reply is the write-path notes, not the file — the first verify grepped the wrong thing. |
| Row 9 change completeness/history (+ P0-3 latency: co-change walks git at call time) | RED test + patch prepared (`co_change_warm_tests.rs`; `build_co_change_snapshot` extracted, warmed at index sync/async + update completion; dry-run OK) — runs in the next cycle; design: `find_similar_changes` already keeps an incremental snapshot (`co_change_cache` + `data_dir/co_change/<pid>.bin`, keyed by walked commit oids); extract it into `services::co_change::refresh_snapshot(state, pid)` and warm it at the end of `index_project` / `update_project_impl` (spawned), so `get_change_set` and `find_similar_changes` only READ at call time (target ≤ 5 s live); denoising = report the false-positive rate of `detect_incomplete_changes` companions on the 5-PR gate |
| Impact/blast ImpactEngine/ChangeSpec (row 10) | RED test + patch prepared (`caller_count_parity_tests.rs`: 60 callers, both tools print 60; find_symbol_references routed through `incoming_caller_edges_checked`, ceiling 5,000, failure stated; dry-run OK) — runs in the next cycle; evidence: `handlers::incoming_caller_edges_checked` (cap-exact, error-propagating, dedup by caller) is ALREADY the authority for check_edit_safety / get_method_edit_context and the unwired gate; `find_symbol_references` re-implements the rule locally (search_tools.rs:995) and `impact_analysis` counts per edge kind with a silent 100 cap (cognitive_tools.rs:198). Slice: route both through the shared function (moved to `services::impact_engine::incoming`), keep `compute_blast_radius` as the causal-transitive view that CITES the same one-hop number, and add a four-tool parity test on one fixture node |
| Row 6 ask_codebase golden suite | **BASELINE on the repaired index 2026-08-29 10:20: GATE FAIL** — status-match 26/35 = 74 % (gate ≥ 80), abstain 3/4 = 75 % (gate 100): `ox_missing_3` ("Which Redis cluster caches the redovisningskategori list?") was ANSWERED from the real term while the premise (Redis cluster) has no evidence — the anti-anchoring defect; 7 real questions came back `ambiguous` (exact_5, usage_1/4, impact_2/4, rationale_2, compound_1) and `exact_6` `unsupported` with full citation coverage. Citation coverage 0.79, latency 967 ms. Slice (cycle 3): premise-term coverage → unsupported when the question's salient terms lack evidence; `ambiguous` only when the PRIMARY entity is ambiguous. Authored 09:47: 35 OciusX questions (4 must-abstain, exact/usage/multi-hop/impact/rationale/compound/history/bug/requirements/ambiguous) in `eval/data/ask_golden_ociusx.jsonl` (git-ignored; customer strings stay out of source); run after the corpus repair with `python eval/ask_engine_golden.py <pid> eval/data/ask_golden_ociusx.jsonl` — gate: abstain 100 %, status-match ≥ 80 %, must-cite hit rate reported **RED→patched 2026-08-29 10:27**: `tests/ask_status_premise_tests.rs` (3) failed for the right reason (no `uncovered_named_terms`), then `status::uncovered_named_terms` (a named premise term absent from ALL evidence ⇒ `has_adequate_support=false` ⇒ Unsupported; the report lists "no evidence mentions `Redis`") + Ambiguous only for ≥ 2 DISTINCT canonical names (a table and its .sql file are one thing); GREEN 3/3 + ask_engine_tests 33/33; sweep 26 running; **LIVE r20 11:13: GATE PASS — status-match 28/35 = 80 %, abstain 4/4 = 100 %, cite 0.79, 557 ms** (r19: FAIL 26/35, 3/4). Movements: ox_compound_1 / ox_impact_2 ambiguous→answered, ox_missing_3 → unsupported ✓; but ox_impact_1, ox_impact_3, ox_usage_4 answered→unsupported and ox_usage_1 ambiguous→unsupported — the named-term gate read only evidence TEXT while `Check_pr_id` resolved to `_us.accessctrl.Check_pr_id` with 50 impact relations saying "X calls the target". **Slice 2 (cycle 2, msg58)**: resolved terms count as covered; a resolved entity present in the evidence anchors support; `tests/ask_status_known_terms_tests.rs` (3). Expected after cycle 2: ≥ 32/35. |
| Row 8 enforcement | RED tests + patch prepared (`repo_rule_enforcement_tests.rs`: forbid clause → Critical finding with the line; require/when → Warning; prose-only stays advisory; `RepoRuleGate` after ImmuneGate; dry-run OK) — runs in cycle 2; evidence: `RepoRule {rule_id, file_pattern, rule_text, priority}`; ImmuneGate attaches matching rules to touched files as advice, AddedConventionsGate reads two rule DEMANDS by text (docs-on-API-change, null-guard). Slice: a checkable clause in rule text — `[check: forbid=<regex>]` / `[check: require=<regex> when=<regex>]` evaluated on the diff's ADDED lines of files matching `file_pattern` — turns a high-priority rule into a real finding (severity from priority; verdict can go red); the promoted quality-gate mandates get clauses at promotion time where the text is mechanical (SQL concatenation, missing check_pr_id on a client pr_id read, empty catch); negative-path test per clause kind |
| Dream ablation | **MEASURED 2026-08-29 10:16 (live)**: the OciusX graph carries **0 `co_occurrence` edges** (edge histogram of 1,349,696 edges: temporal_coupling 1,220,190, contains 58,477, calls 52,285, …, dependency 1,604 — the dreamer's only retrieval-visible output is absent), and no retrieval arm of get_change_set reads that kind — so dream on/off is identical by construction on the 5-PR gate. The effectiveness claim is unsupported by any data; DECISION PENDING (owner question): default Dream off, or keep it running unmeasured. Earlier design: the dreamer writes `CoOccurrence` chunk↔chunk edges + file↔chunk `Dependency` edges into the graph; consumers are impact/blast (non-causal "often searched with" lines) and cognitive tools — NOT get_change_set. Ablation = (a) count dream-origin edges on OciusX, (b) render the 5 gate dossiers with those edges present vs removed (expected identical), (c) measure the impact/blast noise they add. Runs after the corpus repair  **OWNER DECISION 2026-08-29 10:33 (AskUserQuestion): fix first, then ablate** — 0 edges is treated as a defect (the dreamer runs but writes nothing): RED on a fixture → fix → redeploy → on/off ablation on real edges. **CORRECTION 2026-08-29 10:40 (live)**: the "0 co_occurrence edges" reading was a MEASUREMENT ERROR — `get_codebase_overview` prints only the top-15 edge kinds and hides the rest behind `... and 15 more kinds`. Two 10-hit `search_memory` calls moved the graph from 1,349,696 to 1,349,916 edges = +220 = 2 × (20 dependency + 90 co_occurrence), `chunk` nodes 0→20, and the store holds 176 dreamer insights: the write path works, there is no dreamer defect to fix; the owner's "fix first" decision rested on my wrong number. Real defect surfaced instead: the overview's silent top-15 cut (row 2 observability) — RED test + fix queued in cycle 2. NEXT: the on/off ablation proper (retrieval with vs without the dreamer's insights) on the 5-PR gate + ask golden set. |
| 143-tool surface | **DECIDED by the owner 2026-08-29 09:32: tiered surface (~20 core tools advertised, the rest behind an opt-in flag)** — open |
| Row 5 get_ui_conformance | **DECIDED by the owner 2026-08-29 09:32: build M1 (WebForms extractor + catalog) + M2 (`get_ui_conformance(region)`), then re-A/B with n ≥ 5 stories** — M1 slice 1 staged for cycle 2 (`services::ui_catalog::build_families` + `ui_catalog_tests.rs`: families from container nodes, class SETS, consistency typed by evidence, exemplar, generation); design: the index already stores `ui_container` / `control_layout` / control nodes with `container_type`, `layout_style`, `logical_grouping`, `css_class` metadata (what `get_ui_blueprint` reads). M1 = cluster those nodes into families (container type + css-class SET + control-type sequence, normalized per the spec's Layer 0), derive per-axis contracts with evidence counts, persist the catalog as docs in the `insights` namespace at index time; M2 = `get_ui_conformance(region)` matches the region's nodes to families → contract + every deviation, honest caps/coverage; then the n ≥ 5 arm-B replay decides the `/story` wiring **M1 slice 1 staged** (`services::ui_catalog::build_families` — families by container_type/layout, class SETS, Consistent/Chaotic axes with evidence counts, `tests/ui_catalog_tests.rs`), **M2 prepared** (`families_for_region` file/dir/glob pull, `check_classes` every-axis check with the deviation named, tool `get_ui_conformance` — advanced tier, the core tier is at its 32 cap; `tests/ui_conformance_tests.rs`); **re-A/B candidates chosen (n=9 UI stories)**: PRs 1908, 1937, 1918, 1941, 1828, 1899, 1877, 1886, 1894 (`ab_ui_candidates.json`); prep runs with the daemon stopped after M2 deploys. |
| Row 1 EN↔SV domain entities (lexicon) | **RED observed live 2026-08-29 10:57**: English-only story "…the reporting of quantities to show the change requests per fiber installation plan…" → concepts `project, manager, reporting`, 0 Swedish terms in the change set, while `git grep -il` finds mängdredovisning in 9 files and fiberinstallationsplan in 8 (19 s under sweep load). Deterministic source found: OciusX `Site/App_GlobalResources/*.resx` — ~3,000 sv↔en pairs (text 1,374, label 1,148, control 245 keys; 235/244 control keys bilingual, e.g. `Mängdredovisning` ↔ `Reporting of Quantities`). **OWNER DECISION 10:58 (AskUserQuestion): build the resx-lexicon slice after cycle 2 lands** (cycle 3 with M2): EN n-grams of the story → SV resource values → stems as `lexicon`-signal concepts (golden tier, like `gloss`). |
