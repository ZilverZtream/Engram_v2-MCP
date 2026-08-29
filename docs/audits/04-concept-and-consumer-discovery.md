# Row 4 — Exact entity/consumer discovery: `get_concept_footprint`, `find_symbol_references`

Audit date 2026-08-28. Code at `ask-codebase-brain` (fef66ca). Live evidence
from OciusX project `5a35e8e0-d37a-41b3-a250-a26957e7aedb`, gen 824.
Research pass by an isolated Sonnet agent; every citation re-verified by
grep before it was written down.

## 1. Verdict

`get_concept_footprint` promises "EVERY touchpoint" of a concept and is the
tool the generated workflow mandates for "change ALL touchpoints or justify
each one you skip". It matches the concept against **node names only**,
expands consumers from at most **five** table/state anchors through a
six-kind edge whitelist capped at 200 edges each, and falls back to a
**top-50** lexical search of the index — none of which is reported as a
cap. Live, for `redovisningskategori` it named **5 of the 25** `.vb`/`.aspx`
files that literally contain the term and **2 of the 14** UI
markup/code-behind files. The auditor's exact numbers did not reproduce
(the index has been regenerated since), but the substance — the tool
cannot honestly claim completeness — is verified.

One of the misses is an **ingestion gap**, not a query gap: a LINQ
navigation-property read (`ra.rk_redovisningskategorier.pr_id`) produces no
`QueriesTable`/`ReadsColumn` edge, so the consumer is invisible to every
graph-based tool, not just this one.

`find_symbol_references` is the honest sibling (fan-out threshold and
incoming cap are reported), but its initial 50-node fetch has no
truncation flag, so "matches 50 distinct symbols" is a cap stated as a
fact. Three tools now give three different incoming counts for the same
node (`Check_pr_id`: 78 / 98 / 50) because there is no shared count layer.

## 2. Verified defects (`handlers/planning_tools.rs` = `plan`, `handlers/search_tools.rs` = `srch`)

| # | Sev | Defect | Evidence |
|---|---|---|---|
| D1 | P0 | Concept matched against node NAMES only — never bodies, markup, LINQ | `plan:406` `if !matches_concept(&n.name, &stems_b) { continue; }`; live 5/25 code files, 2/14 UI files named |
| D2 | P0 | Consumer expansion from ≤5 anchors, 200 edges each, 6 edge kinds — none reported | `plan:419` `matches!(n.node_type.as_str(), "db_table" \| "global_state") && anchors.len() < 5`; `plan:439` `find_incoming_edges_with_kind(&pid, None, anchor_id, 200)`; whitelist `plan:443-449` (6 of 40 `EdgeKind`s) |
| D3 | P1 | Lexical fallback is a top-50 index search, offloading exact-literal recall to the agent | `plan:470` `top_k: 50,`; the tool's own no-match text `plan:481-487` "grep_project / read the working tree before concluding it's absent" |
| D4 | P1 | Provider failures render as "nothing found" | `plan:400` `.unwrap_or_default()` (node scan), `plan:439-440` `if let Ok(incoming)` (anchor consumers dropped), `plan:490, :493` `.unwrap_or_default()` (lexical) |
| D5 | P1 | Morphology is English-only; no alias layer | suffix rules `plan:39-60` (`'s'`, `ies→y`, `es`), `CONNECTIVES` `plan:73`; the Swedish plural matched only because the singular is a literal prefix (`plan:148` `starts_with(&s_compact)`); `grep -n alias plan` → 0 |
| D6 | P1 | No consumer classification | consumers labelled by raw `EdgeKind::as_str()` `plan:444-449, :566` (`[queries_table]`), no read/write/filter/export/delete/test role |
| D7 | P1 | No shared identity/count layer | `concept_stems`/`matches_concept` used only at `plan:406`; live `Check_pr_id` incoming = **78** (`find_symbol_references`, all kinds) vs **98** causal (`compute_blast_radius`) vs **50** (edit tools' cap) |
| D8 | P1 | Ingestion gap: LINQ nav-property access produces no table edge | `Site/App_Code/redovisning/code/redovisningsartiklar.vb:51-52` `Where ra.rk_redovisningskategorier.pr_id = projectId` — `find_symbol_references` on `_rv.redovisningsartiklar` shows only `contains` edges; the file appears in NO section of the footprint |
| D9 | P1 | `find_symbol_references` 50-node fetch has no truncation flag | `srch:765-766` `query_nodes_by_symbol_name(.., 50).unwrap_or_default()`; store `engram_graph/store.rs:1587` `if out.len() >= limit { break }` returns no "more exist"; fan-out text `srch:903` states "matches {} distinct symbols" — live `GetByID` = exactly 50 |
| D10 | P2 | `find_symbol_references` unreported caps + swallowed joins | outgoing per kind (`max_outgoing_per_kind`, default 50) has no truncation message unlike incoming; label resolution cap 400 `srch:866` silent; `srch:886` `.unwrap_or((Vec::new(), HashMap::new(), false))` — a join failure = "not found" |

Already right (keep): `NODE_SCAN_LIMIT` (200k, `handlers/mod.rs:26`) IS
reported when hit (`plan:533-536`); per-group display caps say "… and N
more"; `find_symbol_references` reports the fan-out threshold
(`srch:900-908`) and the incoming cap (`max_incoming` → "CAPPED at …");
both request structs are `deny_unknown_fields` with every field read.

## 3. Live OciusX evidence (2026-08-28, gen 824)

`get_concept_footprint("redovisningskategori")` — 0.47 s:

```
stems matched: redovisningskategori | graph touchpoints: 48
## Data — 1: rk_redovisningskategorier (db-ociusx.sql/dbo/Tables/rk_redovisningskategorier.sql)
## UI — 2: _rk_redovisningskategorier / _rk_redovisningskategoriers (Site/App_Code/iFalt.designer.vb)
## Logic — 43: _rv.redovisningskategorier + 42 methods (redovisningskategorier.vb)
## Files — 2 | ## Consumers of core anchors — 4 (Site/Reports/redovisning/redovisning.{en,nb,,sl}.rdl)
## Mentioned only in text — 3: grunddata/code/projekt.vb, redovisning/api-json/api-redovisning.vb,
                               admin/production/productioncodelistcategory.aspx.vb
```

Repo literal scan (case-insensitive, excl. node_modules/bin/obj/.git): **45
files** — vb 20, sql 6, aspx 5, rdl 4, md 3, json 2, other 5. Of the 25
`.vb`/`.aspx`/`.designer.vb` files, the tool named 5. Missed (20):

```
admin/production/copycodelistcategory.aspx.vb, import.aspx(+.vb), import_price_list.aspx(+.vb),
productioncodelist.aspx(+.vb), productionprojectcodelist_edit.aspx(+.vb),
projectplanner/admin/linkcodeandforecastqty.aspx(+.vb), system/project/harddelete/harddeleteproject.aspx.vb,
redovisning/code/redovisningshuvudkategori.vb, redovisningsartiklar.vb, estimatedVsReported.vb,
redovisningslistaredovisningsartiklar.vb, ata/code/atalista.vb, grunddata/code/arbetslag.vb,
api-v2/Services/reportingOfQuantities/RoqPriceListService.vb, …/RoqPriceListItemCategory-Out.vb
```

`find_symbol_references("Check_pr_id")` — 0.22 s: 1 symbol, "Incoming
references (78)", 20 shown per kind + "… and 56 more", no CAPPED note
(78 < 200) — complete for this symbol.
`find_symbol_references("GetByID")` — 0.41 s: `⚠ "GetByID" matches 50
distinct symbols` — the 50 is the fetch cap (D9).

## 4. Redesign

### A. Defects to fix now (each gets a failing test first)

| Fix | Mechanism | Closes |
|---|---|---|
| A1 | Term-level matching over indexed TEXT, not names: run the concept stems through the FTS index with cap+1 paging until exhausted or a work budget is hit; report `lexical: N files (complete \| budget hit at N)`. The 50 top-k goes away | D1, D3 |
| A2 | Repository-literal pass inside the tool (the `grep_project` machinery, same 6 KB output budget) as the ground truth for "mentioned in text", diffed against the index result so index staleness is itself reported | D3 |
| A3 | Anchors: every matching table/state/class/entity node (no 5-cap); consumer expansion through the blast substrate's exact-count adjacency with `≥` markers when a per-anchor cap hits; the 6-kind whitelist replaced by the typed edge-policy matrix (row 10 slice step 1) or, until then, reported as "kinds considered: …" | D2 |
| A4 | Alias layer derived from data already in the graph: table ↔ dbml entity ↔ `designer.vb` member (`_rk_redovisningskategorier`) ↔ class name ↔ nav-property name; one `ConceptIdentity` used by footprint, change-set (row 1) and symbol references | D5, D7 |
| A5 | Ingestion: extract LINQ navigation-property reads (`x.<table>.<column>`) in the VB extractor as `QueriesTable`/`ReadsColumn` edges; regression fixture from `redovisningsartiklar.vb:51-52` | D8 |
| A6 | Consumer classification from edge kind + syntax: write (`WritesState`, `InsertOnSubmit`/`SubmitChanges`, UPDATE/INSERT SQL), read/filter (`QueriesTable`/`ReadsColumn`, `Where`), export (`.rdl`, export handlers), delete (`DeleteOnSubmit`, DELETE), test (`is_test_path`) — rendered per consumer | D6 |
| A7 | Every provider failure is a reported line (`consumers: FAILED (…)`), never an empty section; every cap reported with the blast vocabulary | D4 |
| A8 | `find_symbol_references`: cap+1 on the symbol fetch ⇒ "≥50 symbols (capped)"; outgoing truncation flag + message; label-cap note; join failure ⇒ error, not "not found" | D9, D10 |
| A9 | One shared "incoming references of X" function (kind-set parameter) used by symbol references, edit tools and blast so counts agree or the differing kind sets are labelled | D7 |
| A10 | Tests: fixture repo with markup/LINQ/nav-property consumers (golden footprint); Swedish plural/compound morphology; cap reporting per layer; cross-tool count agreement | all |

### B. Redesign that needs evidence first

- **Localized vocabulary tables** (Swedish ↔ English domain terms): the
  `.resx` 7-locale strings are a real source; whether they lift recall
  needs the row-1 story eval, not opinion.
- **Consumer-role precision** (A6 heuristics): label a 50-consumer sample
  by hand before trusting the classifier in a "justify each skip" loop.

## 5. Acceptance gate

| Gate | Measure | Target |
|---|---|---|
| G1 | Recall vs repo literal scan on 5 OciusX concepts (`redovisningskategori`, `installationsobjekt`, `arbetslag`, `personalliggare`, `ata`) | ≥ 90 % of the `.vb`/`.aspx` files the literal scan finds are named, and every miss is explained by a reported cap/budget. Today: 5/25 = 20 % |
| G2 | Caps | 0 unreported numeric caps in the footprint path (grep-verifiable list in D2/D3) |
| G3 | Cross-tool count agreement | for 20 nodes, `find_symbol_references` / edit context / blast incoming counts agree or the kind-set label explains the difference; today 78/50/98 |
| G4 | Nav-property ingestion | `redovisningsartiklar.vb` appears as a graph consumer of `rk_redovisningskategorier` after reindex |
| G5 | Latency | ≤ 2 s per concept on OciusX (today 0.47 s; A1/A2 add work — budgeted) |
| G6 | Sweep green; new tests mutation-checked | |

## 6. Disposition table (implementation in slices — slice 1 landed 2026-08-28)

| Item | Disposition |
|---|---|
| A1 | **partly fixed (slice 1)** — the lexical layer now PAGES the FTS index (`top_k = LEXICAL_PAGE + 1` = 2001, was 50) and reports complete/truncated from the extra hit; still a term search over the index rather than body-level entity matching — the graph match is name-only until A4/A5 |
| A2 | **fixed (slice 3)** — literal (substring, case-insensitive) pass over the indexed chunk text via `engram_index::grep::grep` (the grep_project backend), `LITERAL_CAP` = 5000 with status from the cap; distinct files merged into "Mentioned only in text" (`footprint_text_only_files`, vendor-filtered, deduplicated); coverage line `- literal: …`; failure is a named line. Tests `footprint_literal_tests` x3. **Slice 4 (root cause of the 15 residual misses, §7b):** the trigram index is case-preserving, so the term-index tier could only reach one spelling — fixed at query time with a case-variant trigram conjunction (`fts_mode: literal_ci`, `case_variants`) for the term-index and term-narrowed tiers; `grep_case_variants_test.rs` x2 RED→GREEN + 3 units. Affects `grep_project` too |
| A3 | **fixed (slice 1)** — every matching table/state node is an anchor up to `ANCHOR_CAP` = 50 (reported when hit), consumer expansion fetches `CONSUMER_CAP_PER_ANCHOR + 1` = 201 per anchor so truncation is a fact; the 6-kind consumer whitelist is unchanged and now visible as a cap line |
| A4 | **slice 1 in flight (slice 9): D5 Swedish morphology** — `concept_stems` strips erna/arna/orna/na, er/ar/or, en/et and vowel+n (base ≥ 4) so a Swedish plural/definite concept reaches the singular identifier; `tests/footprint_swedish_morphology_tests.rs` (3) RED first. **open**: the alias layer itself (table ↔ dbml entity ↔ designer member ↔ class ↔ nav-property) |
| A5 | **fixed (slice 2)** — VB extractor: `rangeVar.<table-shaped member>.column` chains on LINQ query-clause lines become `queries_table` READ edges (`orm=nav`), one per (function, table), skipped when a context access already covers the pair; PascalCase EF nav-properties deliberately not matched (need the DDL table set). Tests `linq_navigation_property_tests` x3 (fixture = the audit's `redovisningsartiklar.vb:51-52` shape). Live effect needs a full OciusX reindex — §7 |
| A6 | **in flight (slice 8)** — `consumer_role(kind, src)` from edge kind + source member name/path (test > export > delete > write > read; `sql?` when a SqlCalls name states no verb); every consumer line `[role:kind]`, header tallies the roles and states the limit (bodies not inspected). `tests/footprint_consumer_role_tests.rs` (2) written RED first |
| A7 | **fixed (slice 1)** — `FootprintCoverage` → `## Coverage` block: node scan (complete/truncated/failed), anchors matched/expanded (cap), consumers (status, edge count, per-anchor cap), lexical (status, files/hits/page), failures; the node-scan / consumer / lexical swallows are gone |
| A8 | **fixed (slice 6, 2b85769, deployed 02:29)** — symbol fetch cap+1 ("matches 50+ distinct symbols (fetch cap 50 — more exist; narrow with file_scope)"), symbol-lookup / incoming-fetch failures as FAILURE lines, the blocking-join failure is an error not "no references", `## Coverage` with the 400-label cap ("labels resolved for X of Y endpoint(s)"). `tests/symbol_references_caps_tests.rs` x2, RED first. Outgoing per-kind truncation note still open |
| A9 | **in flight (slice 7)** — not one number but one RULE per number: `find_symbol_references` prints "N edges, all kinds; D distinct caller(s) via calls+dependency — the number check_edit_safety / blast_radius use"; the edit tools print "N distinct callers (calls+dependency, dedup by caller[; capped])"; blast_radius already says "causal 1-hop, dangling quarantined". `tests/incoming_count_parity_tests.rs` x2, RED first |
| A10 | **partly** — `footprint_coverage_tests` x3 (paging status, anchor cap, coverage block); the fixture-repo golden footprint comes with A4/A5 |
| A11 | **fixed (slice 5, sweep14)** — `max_per_group` ceiling 100 → `FOOTPRINT_GROUP_CEILING` 500 so a caller can list a whole "Mentioned only in text" section (the last 14 G1 misses sat behind the ceiling; the cut was reported, the ceiling made it permanent). `tests/footprint_ceiling_tests.rs` x2 on a real 120-file project, RED first |
| G1 | baseline 60/240 = 25 % → slice 1 84/240 = 35 % → **slices 2+3 (+ full reindex) 225/240 = 94 %** (redovisningskategori 25/25, installationsobjekt 116/130, arbetslag 46/46, personalliggare 37/38, tidrapport 1/1) — §7; the 15 residual misses are one root cause (case-preserving trigram index), slice 4 |
| G2-G6 | per slice in §7 |

## 7. Live evidence — slice 1 (2026-08-28, binary 20:35, OciusX healed graph)

G1 harness: `row4_g1.sh` — for each concept, the `.vb`/`.aspx`/`.ascx` files a
case-insensitive literal scan of the working tree finds vs the files
`get_concept_footprint` names in any section.

| concept | literal-scan files | named BEFORE (top-50 lexical, 5 anchors) | named AFTER slice 1 | coverage line (after) |
|---|---|---|---|---|
| redovisningskategori | 25 | 5 (20 %) | 5 (20 %) | anchors 1/1 · consumers complete (4 edges) · lexical complete (10 files from 65 hits) |
| installationsobjekt | 130 | 21 (16 %) | **42 (32 %)** | anchors 7/7 · consumers complete (3) · lexical complete (35 files from 413 hits) |
| arbetslag | 46 | 19 (41 %) | 19 (41 %) | anchors 0 · lexical complete (21 files from 53 hits) |
| personalliggare | 38 | 15 (39 %) | 18 (47 %) | anchors 2/2 · lexical complete (17 files from 75 hits) |
| tidrapport | 1 | 0 | 0 | lexical complete (1 file from 1 hit) |
| **all** | 240 | 60 (25 %) | **84 (35 %)** | |

What the coverage line proves: every lexical page is `complete` — the
index simply does not contain the other files for these terms. The
literal scan matches the stem INSIDE identifiers (`rk_redovisningskategorier`,
`Redovisningskategori #id`); the FTS tokenizer indexes the identifier
whole, so `redovisningskategori` reaches 10 files where the literal scan
reaches 25. That is defect D3's real shape and the reason A2 (an in-tool
literal pass) is next, not a bigger page. Anchors: `arbetslag` and
`tidrapport` match no table/state node at all, so the consumer arm has
nothing to expand — also now visible rather than silent.

## 7b. Live evidence — slices 2 + 3 (2026-08-28, commits 09aac51 + 1499b3f, OciusX wipe_and_reindex gen 828, 2,274 files / 18,144 functions)

Same G1 harness (`row4_g1.sh`, `max_per_group: 100`):

| concept | literal-scan files | after slice 1 | **after slices 2+3** | coverage line (after) |
|---|---|---|---|---|
| redovisningskategori | 25 | 5 (20 %) | **25 (100 %)** | anchors 1/1 · consumers complete (6 edges — was 4: the two LINQ nav-property reads) · lexical complete (35 files/117 hits) · literal complete (35 files/246 matches; cap 5000) |
| installationsobjekt | 130 | 42 (32 %) | **116 (89 %)** | lexical complete (166 files/755 hits) · literal complete (166 files/2137 matches); text-only section 137 files, 100 shown + "… and 37 more" |
| arbetslag | 46 | 19 (41 %) | **46 (100 %)** | anchors 0 · literal complete |
| personalliggare | 38 | 18 (47 %) | **37 (97 %)** | anchors 2/2 · lexical complete (47 files/151 hits) · literal complete (46 files/281 matches) |
| tidrapport | 1 | 0 | **1 (100 %)** | literal complete |
| **all** | 240 | 84 (35 %) | **225 (94 %)** | |

Slice 2 visible on the live graph: `sym:function:Site/App_Code/redovisning/code/redovisningsartiklar.vb:_rv.redovisningsartiklar.GetByProjectId:46 -> rk_redovisningskategorier` is now a `[queries_table]` consumer edge (the audit's D5 example, `orm=nav`).

**Residual (15 files) diagnosed — one root cause, not the cap.** Every
miss is a file whose only occurrence of the concept is a differently
cased spelling: `' PERSONALLIGGARE` (`api-broker.vb:252`),
`_io.InstallationsObjektProjektPropertiesLog` (`marker_property_logs.aspx.vb:58`),
`InstallationsObjekt…` in `assetaccessrequest_edit.aspx:322`. Live
repro with `grep_project` (same backend as the footprint's lexical page and
literal pass): `personalliggare`, case-insensitive, no prefix → 151 chunks /
46 files, reports `complete`, **api-broker.vb absent**; `PERSONALLIGGARE`
with `case_sensitive:true` → found (tier `term_index`). Cause: the
`content` field is trigram-tokenised with `NgramTokenizer::new(3, 3, false)`
and NO lowercasing (`tantivy_index.rs:95`), so a lower-case pattern's
exact trigrams cannot occur in a chunk whose only spelling is upper or
mixed case — the term-index tier silently narrows to one spelling while
its coverage line says `complete`. This affects `grep_project` itself (a
quiet-failure of the "reports complete while missing" class), not only
the footprint. Slice 4 (`grep_case_variants_test.rs`, RED first): a
case-insensitive literal builds a case-variant trigram conjunction at
QUERY time (no reindex, no old-index regression), for both the
term-index and term-narrowed tiers.

The `max_per_group` ceiling (clamp 1..=100) is reported ("… and 37 more")
but makes a 137-file text section unreachable by any caller; left as is
for now — the harness measures what the agent is shown, and the honest
line is there.

## 7c. Live evidence — slice 4 (2026-08-28, commit 59f2005 deployed 22:56, OciusX gen 828)

`grep_project personalliggare` (case-insensitive, no prefix, cap 5000):
before 281 matches / 46 files, `api-broker.vb` absent, `complete` →
**after 290 matches / 49 files, `api-broker.vb` present**; against the
38-file literal scan: **38/38 found** (was 36/38). Tier stays
`term_index` (137 ms).

G1 after slice 4: **226/240** — `personalliggare` 38/38 (100 %, was 37),
`redovisningskategori` 25/25, `arbetslag` 46/46, `tidrapport` 1/1,
`installationsobjekt` 116/130 (89 %, unchanged). The 14 remaining
`installationsobjekt` misses (`assetaccessrequest.aspx`, `…aspx.vb`, …)
sit in the "Mentioned only in text" section beyond position 100: the
section prints "… and 37 more" and `max_per_group` is clamped to ≤ 100,
so no caller can see them. That is the reported cap, not a silent one;
raising the ceiling is a one-line follow-up (A11, not a discovery
defect).

## 7d. Live evidence — slice 5 (2026-08-29 01:21 deploy, commit 8ff187f)

`get_concept_footprint("installationsobjekt", max_per_group: 500)`: the
"Mentioned only in text" section lists all **138** files (no cut line);
against the 130-file literal scan: **130/130**. With the ceiling raised,
G1 = **240/240 (100 %)** across the five concepts (25/25, 130/130,
46/46, 38/38, 1/1). The default cap (8) and any cap below the section
size still print "... and N more".

## 7e. Live evidence — slice 6 (2026-08-29 02:29 deploy, commit 2b85769)

`find_symbol_references("GetByID")` — 0.24 s:

```
⚠ "GetByID" matches 50+ (fetch cap 50 — more exist; narrow with file_scope) distinct symbols — too many to expand each …
## Coverage
- symbols: 50+ — more exist, narrow with file_scope (fetch cap 50)
- labels resolved for 275 of 349 endpoint(s) (cap 400)     ← 74 endpoints are dangling ids (no node), not the cap
```

`find_symbol_references("Check_pr_id")`: "Incoming references (78)", `symbols: 1
(fetch cap 50)`, `labels resolved for 79 of 79 endpoint(s) (cap 400)`.
The "exactly 50" of the audit is now a stated cap; the label line shows
the unresolved remainder, which is dangling endpoints rather than the 400
cap on these two symbols.
