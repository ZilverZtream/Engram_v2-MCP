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
| A2 | **open** — repository-literal pass inside the tool (the G1 script `row4_g1.sh` is the external stand-in today) |
| A3 | **fixed (slice 1)** — every matching table/state node is an anchor up to `ANCHOR_CAP` = 50 (reported when hit), consumer expansion fetches `CONSUMER_CAP_PER_ANCHOR + 1` = 201 per anchor so truncation is a fact; the 6-kind consumer whitelist is unchanged and now visible as a cap line |
| A4 | **open** — alias layer (table ↔ dbml entity ↔ designer member ↔ class ↔ nav-property) |
| A5 | **open (next slice)** — LINQ nav-property reads as `QueriesTable`/`ReadsColumn` edges in the VB extractor; regression fixture `redovisningsartiklar.vb:51-52` |
| A6 | **open** — consumer classification |
| A7 | **fixed (slice 1)** — `FootprintCoverage` → `## Coverage` block: node scan (complete/truncated/failed), anchors matched/expanded (cap), consumers (status, edge count, per-anchor cap), lexical (status, files/hits/page), failures; the node-scan / consumer / lexical swallows are gone |
| A8 | **open** — `find_symbol_references` cap+1 on the symbol fetch, outgoing truncation flag, label-cap note |
| A9 | **open** — shared incoming-count function (edit tools 76 vs blast 98 for `Check_pr_id`) |
| A10 | **partly** — `footprint_coverage_tests` x3 (paging status, anchor cap, coverage block); the fixture-repo golden footprint comes with A4/A5 |
| G1 | baseline 60/240 = 25 % (5 concepts: redovisningskategori 5/25, installationsobjekt 21/130, arbetslag 19/46, personalliggare 15/38, tidrapport 0/1); after-deploy number in §7 |
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
