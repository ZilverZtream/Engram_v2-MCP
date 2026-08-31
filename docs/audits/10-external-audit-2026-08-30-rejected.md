# External audit 2026-08-30 — verdict REJECTED (second round) — the remediation checklist

Source: the external auditor's second report, delivered by the product owner on 2026-08-30 after
release 33 and the close-out report (doc 09). Findings verbatim below; paths as cited (verified
against the working tree at c76d241 — see "Citation check"). Mandate unchanged: loop until every
item is gold standard or shown impossible with evidence; every scope decision is the owner's;
never commit into the OciusX repo.

## Verdict (auditor): REJECT "everything is fixed"

"A substantial amount genuinely is fixed, and the current OciusX index is healthy. But four vital
brain-quality problems remain: (1) a real generation-GC race can still delete new vector data;
(2) project health can report 'complete' for a catastrophically incomplete corpus;
(3) get_change_set finds the required files but buries them in a 200-file dossier;
(4) the ask_codebase '35/35 golden' gate measures status classification, not answer correctness.
The deployed binary matches the repository release binary exactly."

## P0-1 — The GC race is only half fixed (PARTIAL)

Tantivy removes only generations older than the published one (good). LanceDB still removes every
generation `!= active` (`crates/engram_index/src/vector.rs:190`): N published → GC sees no active
indexing → an update starts N+1 after the check → GC reaches LanceDB → `generation != N` deletes
the in-flight N+1 vectors. The GC checks an atomic counter but never takes the per-project update
mutex (`actors/gc.rs:94`): check-then-act. `purge_never_deletes_the_generation_being_built` passes
because its helper counts Tantivy documents only (`tests/gc_generation_race_tests.rs:100`); it
would pass while the vector store deletes N+1. After publishing, update swallows purge failures
(`handlers/project_tools.rs:2071`: `purge_old_generations(...).await.ok()`).

Required: GC and update mutually exclusive on the per-project lock (or a durable set of building
generations); vector KeepLatestOnly deletes `generation < active`; a LanceDB assertion that both N
and in-flight N+1 survive purge(N); post-publication purge failures surfaced as degraded state and
retried, never swallowed.

## P0-2 — "Generation completeness" is not a completeness measurement (WRONG)

Live: generation 836, 31,225 code chunks, 2,277 tracked files, "1,371.3 % — complete".
`project_tools.rs:2215` divides chunks by graph file nodes (`ratio = code_chunks / files;
complete = files == 0 || ratio >= 0.5`). Chunks and files are different units (≈13.7 chunks per
file): the overwhelming majority of files could vanish and it would still report > 50 %. It checks
Tantivy only, not the active-generation vector paths, never compares against eligible repository
files, uses graph files as the denominator (graph damage shrinks the expectation), and a few large
files mask losses. The tests use one small chunk per file and delete 80 %, so they miss the
masking defect. A percentage above 100 % should itself have failed the invariant review.

Required: compare PATH SETS — eligible repository paths ↔ active-generation Tantivy paths ↔
active-generation LanceDB paths ↔ graph File nodes; report expected distinct paths, present per
store, missing and extra, cross-store generation mismatches, sample missing paths; chunk/document
counts only as diagnostics.

## P0-3 — get_change_set has recall, but not usable precision (PARTIAL)

Reference story live: ≈4–6 s, 200 files returned, 25 omissions, all six expected files present —
at ranks 14 (api-redovisning.vb), 18 (redovisningskategorier.vb), 121 (the page), 122 (its
code-behind), 176 (rk_redovisningskategorier.sql), 198 (iFalt.dbml). Raw precision ≈ 3 %.
Principal cause: a single .resx translation is "golden" (`planning_tools.rs:4887`) and lexicon
matches are exempt from the weak-tail cap (`:5468`); generic translations (category → kategori,
reports → rapporter) promote huge parts of OciusX. The committed acceptance checks presence and
runtime only — not rank, count, precision, token cost or the resulting plan.

Required: a single generic lexicon term is never golden; score translations by rarity/IDF,
phrase specificity and compound coverage; require lexicon evidence to be corroborated by another
independent arm; cap broad translation populations; penalise globally common translated terms;
evaluate must-have RANK. Suggested acceptance: all critical files in the top 20–30; ≤ ~40
implementation candidates; an explicit "possible companions" set below the primary set;
plan file-F1 or expert-scored usefulness.

## P0-4 — ask_codebase 35/35 does not mean 35 correct answers (WRONG acceptance claim)

`eval/ask_engine_golden.py:140`: `gate_ok = abstain_rate >= 1.0 and status_rate >= 0.80`.
Citation coverage (mean 0.87) is printed but not gated; no exact-answer predicate, no required
evidence modality. Concrete failure: "Which resource keys describe the main code category
workflow?" → answered with ten evidence items and no .resx evidence (unrelated image API, auth,
SQL column, logout controller); citation coverage 0.00, gate passed. "35/35" means 35 accepted
status labels, not 35 correct answers.

Required per golden question, one or more of: required symbols/files/node ids; required evidence
modality (.resx, SQL schema, call graph); forbidden distractors; exact factual assertions;
minimum evidence precision; contradiction checks; an expert-labelled acceptable answer set.
An answered result with zero evidence from the requested modality must fail.

## P1-1 — Pre-commit still has fail-open integrity paths (PARTIAL)

Live review: 19 gates, 24 findings, yellow, 0 degraded, caps reported — genuine. But a search hit
whose backing document returns Ok(None) is converted to empty content or skipped
(`gates.rs:1482, :1513, :2881, :3090`); that is an integrity failure, not empty evidence.
Pre-commit also inherits the invalid chunk/file completeness test.

## P1-2 — Co-change is cached, but "no call-time Git walk" is false (PARTIAL)

≈292 ms live. But the snapshot builder still opens the repository twice, walks commit OIDs on
every invocation and diffs commits absent from the snapshot (`planning_tools.rs:567`, `:606`);
index/update warms 500 commits while get_change_set requests 800 (`:6399`), so a later call can
diff commits 501–800. Duplicate `GitWalker::open_repo` statement (build warning).

## P1-3 — The 17/17 acceptance record is not a clean, reproducible run (REJECT evidence claim)

The raw log ends `PASS: 16 | FAIL: 4` (`docs/audits/evidence/acceptance_r33.log:103`); corrections
were appended later and the close-out generator uses "last result wins"
(`make_closeout.py:19`). The script is not self-contained (undefined `$PC_DUMMY`, external
deploy.ps1 / g1_check.sh / row8_live.py — `acceptance_r33.sh:46`). This does not invalidate the
verified fixes; it invalidates "one fresh mechanical 17/17 acceptance run".

## P1-4 — Dream remains active without demonstrated value (MEASURED, NOT IMPROVED)

Dream ON: 0 insight evidence in 35 questions; OFF: 0; same 35/35; ON slightly slower. Its write
path works; no evidence it helps an agent. Default it off until an A/B shows positive answer
accuracy, planning quality or defect-prevention impact.

## Verified fixes (auditor)

Deployed binary matches the release build; literal redovisningskategori corpus recovery;
concept-footprint coverage; edit-context parity 20/20; core surface 32 tools; 113 advanced tools
discoverable; generated Claude workflow contract; caller authority 76 = 76; cross-store integrity
healthy now (generation 836, Tantivy 172,107 documents, vector rows 172,107, 49,494 nodes,
1,349,916 edges, no mismatches); first-GC delay; Tantivy newer-generation preservation; pre-commit
gate-status/cap reporting; UI house-style implementation-time approach previously measured
positively. Focused suites: 21 binaries, 34 tests, 34 passed.

## Auditor's blocking order

1. Generation atomicity across Tantivy and LanceDB.
2. Per-store path-set integrity instead of the chunk/file ratio.
3. get_change_set precision and ranking before more recall.
4. An answer-correctness golden suite instead of the status tournament.
5. The remaining pre-commit missing-document paths.
6. Acceptance rebuilt as one immutable, self-contained run.
7. Dream opt-in until it demonstrates positive agent outcomes.
8. The larger ImpactEngine only after the causal golden suite exists.

## Citation check (2026-08-30, working tree c76d241)

| Citation | Read | Verdict |
|---|---|---|
| `engram_index/src/vector.rs:190` KeepLatestOnly | `"namespace = '{}' AND generation != {}"` — deletes every generation other than the active one, including an in-flight N+1 | **accurate** |
| `actors/gc.rs:94` | `purge_project_old_gens` reads `active_indexing_count` then purges; no per-project update mutex is taken | **accurate** (check-then-act) |
| `handlers/project_tools.rs:2071` | after `set_meta(active_generation)`: `ps.search.purge_old_generations(project_id, new_gen).await.ok()` | **accurate** (swallowed) |
| `tests/gc_generation_race_tests.rs:100` | helper `code_docs` = `count_docs_by_namespace` (Tantivy) only | **accurate** |
| `handlers/project_tools.rs:2215` | `ratio = code_chunks / files; complete = files == 0 \|\| ratio >= 0.5`, Tantivy count vs graph File nodes | **accurate** (1,371 % live) |
| `handlers/planning_tools.rs:4887` | `golden = cochange \|\| history \|\| gloss \|\| lexicon` | **accurate** |
| `handlers/planning_tools.rs:5468` | tail-cap exemption includes `lexicon` (and vtop/family/gloss) | **accurate** |
| `handlers/planning_tools.rs:567/606` | `GitWalker::open_repo` twice; `walk_older_commits(max_commits)` every call; diffs commits not in the snapshot | **accurate** |
| `handlers/planning_tools.rs:6399` vs `project_tools.rs:1670/1689/1712` | get_change_set asks 800 commits; index/update warm 500 | **accurate** |
| `pre_commit_review_service/gates.rs:1482/1513/2881/3090` | `get_doc_by_pk` `Ok(None) => String::new()` / `.flatten()` / `continue` | **accurate** |
| `eval/ask_engine_golden.py:140` | `gate_ok = abstain_rate >= 1.0 and status_rate >= 0.80`; citation coverage printed, not gated | **accurate** |
| `docs/audits/evidence/acceptance_r33.{log,sh}`, `make_closeout.py:19` | raw log `PASS: 16 \| FAIL: 4`; corrected rows appended; last-wins in the generator; `$PC_DUMMY` undefined; external scratch scripts | **accurate** — the 17/17 is a run plus corrections, not one clean run |

Every citation is accurate. Nothing in this table is disputed.

## Owner decisions (2026-08-30 ~06:45, AskUserQuestion)

- Order: the auditor's blocking order 1→8 as given.
- Dream (P1-4): default OFF now (`include_insights` opt-in); an A/B on plan quality decides whether it returns.
- Loop: the 2-minute loop re-armed with the round-2 mandate; same discipline as round 1.

## Disposition (filled as work lands — every row ends fixed@commit+live, or impossible@evidence)

| Item | Disposition | Live evidence (OciusX) |
|---|---|---|
| P0-1 GC race (LanceDB `!= active`, no lock, Tantivy-only test, swallowed purge) | fixed@52e6ef8 + live r34 (2026-08-30 09:28) | vector purge `generation < active` (KeepLatestOnly); GC takes the per-project update lock (`try_acquire_project_update_lock`, outcome `SkippedUpdateInFlight`); post-publish purge failures recorded as `purge_pending` in the registry, surfaced in the update report and retried by the GC. Tests: gc_vector_race_tests (LanceDB rows of in-flight N+1 survive purge(N); GC yields to a held update lock; purge outcome recorded/retried). LIVE: update_project → `purge: ok`; check_integrity before/after tantivy 172,107 = vectors 172,107, mismatches []; health OK, generation 836→837 (verify_r34.log) |
| P0-2 completeness = path-set integrity per store | fixed@54cfa55 + 4b5b105; live r35 → r39 healed (2026-08-30 14:57) | Path sets per store replace the chunk/file ratio (`GenerationCompleteness`: expected / tantivy / vectors / graph, missing + sample, extra, cross-store mismatch; chunk and row counts diagnostic only; health, freshness and the pre-commit note all speak in paths). The first live run (r35) exposed 5 REAL gaps hidden by the old tolerance: four Latin-1 sources and a vendor bundle skipped at ingest as 'Invalid UTF-8' — graph File node, no search document, content unsearchable. Follow-up 4b5b105: non-UTF-8 files are indexed lossily (warned, counted as indexed); files skipped BY RULE (binary / too large / unreadable) live in a per-project ledger subtracted from the expectation and reported (`skipped by rule: N — reason n`); tolerance is ZERO. LIVE r39 pre-heal (honest): `Health: CORRUPT — active generation 837 is INCOMPLETE (5 of 2277 eligible paths missing …)` naming the five; same-id `repair_project` (scope full, wipe_and_reindex) in 80 s → generation 838, files=2277 chunks=31263; post-heal: `Health: OK`, expected 2277 / tantivy 2277 / vectors 2277 / graph 2277, missing 0, extra 0, mismatch 0, skipped by rule 0, health 1 s; freshness `generation_complete: true (2277 of 2277 …)`; the Latin-1 stylesheet's content searchable; check_integrity 203,370 = 203,370. Tests: index_integrity_pathset_tests, index_integrity_skips_tests, two old-contract tests updated |
| P0-3 change-set precision/ranking | fixed@06219bd + a936cf2 + a598164; live r39 6/6 (2026-08-30 15:00) | Precision: a .resx translation is golden only when an independent arm corroborates it; a term matching ≥ 40 files is `broad` (footprint totals as the IDF proxy) and never evidence — live `main` 860 files, `reporting` 358, `category` 286, `kategori` 117 refused and named in coverage; story-word NAME coverage (≥ 3 words) is golden; a PRIMARY set ranked across layers, capped at 40 (tier ≤ 2), name coverage leading its tier and the best tier ≤ 1 row of every layer in the head, then layer-grouped companions; JSON `set`/`rank`, markdown '## Primary candidates' numbered. LIVE r39 reference story (5 s, co-change warm): 166 candidates, PRIMARY 40; ranks rk_redovisningskategorier.sql 3, iFalt.dbml 5, page 13, code-behind 14, api-redovisning.vb 20, redovisningskategorier.vb 22 → 6/6 within the top 30 (r33: 14/18/121/122/176/198 of 200). Tests: change_set_rank_tests (4) + unit story_word_name_coverage_leads_its_tier + the_best_row_of_each_layer_leads_the_primary_set; every existing change-set suite green |
| P0-4 answer-correctness golden suite | fixed@6ad6932 + 001a6ab + 96185a2 + bfcaa38 + 7ec2850; live r42 34/35 (2026-08-30 17:36); multi_4 = TS→API route resolution (item 8) | Golden harness judges EVIDENCE per question (required_modality / required_all / forbidden / min_precision; 31/35 rows carry requirements; corpus git-ignored by the no-customer-strings rule, referenced by sha256). Honest baseline r33 27/35 → r37 32/35 → r39 29/35 (callee flooding) → r40 31/35 → r41 33/35 → r42 34/35; status-match 35/35 throughout. Engine: Modality on the plan (whole-word cues), suffix filter inside the index on both legs, a filtered arm per requested modality, one bounded call-graph hop (intent-gated, name-cued, one callee per file, max 3, directness 0.6), ONE reserve pass (needed kind / up to three items of a requested modality preferring the question's words / named file) with protected eviction, at most two items per file, lowercase hyphenated file names as entities. LIVE r42: the auditor's three questions cite .resx / .rdl + .sql / .sql (rk_redovisningskategorier.sql among them); multi_1 answered via CanUserBulkUpdate one hop from the API entry point; compound_1 cites api-installationsobjektprojekt.vb; impact_3 cites .resx. Remaining: multi_4 'How does the map's marker info window fetch a marker's images?' → api-images — the TS→API endpoint edge does not exist in the graph (route resolution), which is the causal/ImpactEngine work of item 8. CLOSED 2026-08-31 @r52 (1f0a2de): golden 35/35 live — api-images.vb cited via the route-resolved getImage wrapper (see the item-8 row); judge hardening to item-level predicates is doc-11 item 2 |
| P1-1 pre-commit Ok(None) integrity paths | fixed@54a581b + live r38 (2026-08-30 13:48) | `gates::hit_content` is the one way a gate reads a search hit: Ok(None) → the gate is DEGRADED with 'hit <pk> has no backing document in the search index — integrity failure, not empty evidence' and the hit is skipped (the four sites incl. the suppression closure that silently counted a missing document as 'no suppression'). Tests: pre_commit_hit_integrity_tests (ghost pk → degraded + integrity note; real hit reads, nothing degraded). LIVE pre_commit_review diff=head: 19 gates, degraded [], integrity notes 0, verdict yellow |
| P1-2 co-change call-time git walk (500 vs 800) | fixed@8f85a20 + live r38 (2026-08-30 13:48) | `CO_CHANGE_DEPTH` = 800 is the one depth (index/update warms to it, get_change_set requests it); a snapshot covering HEAD at the requested depth is served as-is (no oid walk, no diff) and the tool prints the coverage line; duplicate `open_repo` removed (build warning gone). Tests: co_change_depth_tests (warm depth ≥ request depth; a call at that depth reports the warm path with nothing diffed). LIVE find_similar_changes: 'co-change snapshot: warm (served without a git walk)', 800 commits, 199 ms then 164 ms |
| P1-3 immutable, self-contained acceptance run | fixed@29ec241 (script) + d28a8aa (run 2) — docs/audits/evidence/acceptance_round2.sh is ONE self-contained run: daemon restart, then every P0/P1 probe (24) and the P0-4 golden suite against live OciusX, writing a stamped read-only .log + .results.txt and the golden JSON; no appended corrections — a fresh complete run replaces | run 1 20260830_173903 = 22/24 (probe defect: the produce_claude_md check read the wrong file; kept verbatim); run 2 20260830_174718 = 23/24 — the single FAIL is P0-4 correctness 34/35 (ox_multi_4 = item 8's open TS→API route), P0-1 purge/gen 840/172,145=172,145, P0-2 2277/2277/2277, P0-3 6/6, P1-1/P1-2/P1-4 all PASS |
| P1-4 Dream default | fixed@560ab4d + live r38 (owner: default OFF) | `ask_tools::insights_enabled` = include_insights.unwrap_or(false); request doc says Default OFF; the golden harness sends the switch explicitly (`--insights` turns it on). Tests: ask_insight_switch_tests (absent = OFF, explicit true = ON); memory_readable_composites_tests opts in. LIVE ask_codebase: providers without the switch = business_logic/code/concept/doc/memory (no insight arm); with include_insights:true the insight arm is present; golden 32/35 with insights OFF (no regression) |
| Item 8 ImpactEngine — causal golden suite first, then the one-hop slice (TS→API route resolution) | fixed@5aad54a + 2bcb084 + 0e1703c + 39ced3e + 5162d4c + 2c7ee25 + b12835f + 137cf08 + c8a1b87 + 1f0a2de; live r52 (2026-08-31 12:36): causal 20/20, golden 35/35, ranks 6/6 ×2 | Scope decided by the owner 2026-08-31 (doc 11): item 8 = "TS/VB route enrichment + ask-bounded causal hop" — the shared ImpactEngine (ChangeSpec, edge-policy matrix, SCC, shared JSON contract) is its own queued program; full closure owes the HELD-OUT causal suite (doc 11 item 3). Causal suite eval/data/ask_causal_ociusx.jsonl (20 questions, git-ignored, sha256 in every --out): r42 baseline 5/20 → r43 10/20 → r44 19/20 → r47–r52 20/20 (SIX consecutive releases, full wipe+reindex each). Golden 35: 33/35 (r44) → 34/35 (r45–r51) → 35/35 (r52): ox_multi_4 cites api-images.vb through the route-resolved getImage wrapper — the hop now COLLECTS candidates across the bounded walk and a rank decides the cap (cue-hit wrapper implementations, cue hits, the rest; stable by discovery; one per defining file) after streaming/reservation provably could not make the walk arrive (r50/r51 diagnosis). Ranks 6/6 twice every release. Route machinery: js name-route + web-method edges, VB dispatch_key on sidecar AND fallback paths, ingest same-key metadata union, GraphStore::resolve_route_edges (live: api_calls=432 added=313 ambiguous=1 unbound=1), receiver-aware ambiguity, family-seeded callee hop. Evidence: causal_golden_r47–r52.*, golden_v2_r47–r52.*, verify_r47–r52.log |
