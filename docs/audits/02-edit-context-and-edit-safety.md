# Row 2 — Follow the code before editing: `get_method_edit_context`, `check_edit_safety`, `get_page_context`

Audit date 2026-08-28. Code at `ask-codebase-brain` (post 3637c4d). Live
evidence from OciusX project `5a35e8e0-d37a-41b3-a250-a26957e7aedb`,
generation 824.

## 1. Verdict

The three tools promise a pre-edit oracle ("Call this BEFORE modifying any
method", `tools.rs:1401`) and a standalone safety verdict (`tools.rs:1471`).
What they deliver is a verdict computed from providers that can each fail or
truncate WITHOUT the verdict knowing:

- a missing blast report becomes risk **0.0** and feeds the GREEN branch;
- `check_edit_safety` never reads the body, so complexity is **always 0**
  and its two complexity thresholds can never fire;
- caller counts are a **cap (50) presented as a count** — the live hot
  helper `Check_pr_id` reports "50 callers" while the blast substrate counts
  98 exact causal callers;
- the method's DB/session facts come from **global first-5000-edges scans**
  (`list_edges_by_kind`), which on OciusX already covers only 9.4 % of
  `calls` edges (53,370) in `get_page_context`;
- three schema fields (`include_business_logic`, `include_master_page`,
  `include_codebehind`) are accepted and ignored;
- there are **zero tests** for `compute_edit_safety` or any of the three
  handlers.

None of this needs a new engine. It needs the same discipline blast-radius
got in round 5: every provider returns a typed completeness, and missing or
truncated evidence can never render green.

## 2. Verified defects (grep-verified 2026-08-28; `access_layer_tools.rs` unless stated)

| # | Sev | Defect | Evidence |
|---|---|---|---|
| D1 | P0 | Missing blast evidence ⇒ risk 0.0, not unknown | `:906-908` `br_score = blast_radius.map(..).unwrap_or(0.0)`; `:911-913` `has_triggers … unwrap_or(false)`; result field `:2353-2356` `blast_radius_score … unwrap_or(0.0)` while `:2357-2360` prints band `"Unknown"` — score and band contradict |
| D2 | P0 | Blast failure swallowed | `:2318-2325` and `:4319-4326` `compute_blast_radius(..).ok()` — error text lost, verdict proceeds |
| D3 | P0 | `check_edit_safety` complexity is always 0 | handler `:4250-4328` never reads the body; `build_method_info_from_node` `:769-773` reads `metadata.complexity_score` which no extractor writes (comment `:2210-2213`); only `get_method_edit_context` estimates it `:2214-2218`, and only when `include_full_body` and the read succeeded (`:2203-2205` `.ok()`) |
| D4 | P0 | Caller cap rendered as a count | `:699` `incoming_caller_edges(.., 50)`; `handlers/mod.rs:45-65` per-kind limit then `truncate(limit)`, no truncation flag; verdict reason `"{} callers — high blast radius"` `:945-947` prints the capped length; renderer `:1289-1293` `"## Callers ({} shown)"` with no total |
| D5 | P1 | DB/session facts from global capped scans | `:721, :731, :742, :749` `list_edges_by_kind(.., 5000)` for QueriesTable/SqlCalls/ReadsState/WritesState then filtered by source; store `engram_graph/store.rs:813-835` returns the first N edges of that kind in KEY order project-wide. `has_session_writes` (`:910`) is a verdict input |
| D6 | P1 | `get_page_context` same pattern, six times | `:2463, :2466, :2541, :2546, :2637-2646` `list_edges_by_kind(.., 5000).unwrap_or_default()`; methods `:2460` `query_nodes(.., 500)`; no section says whether it is complete |
| D7 | P1 | Schema fields accepted and ignored | `:2138` `_include_biz_logic`, `:2396-2397` `_include_master`, `_include_cb`; schema promises them (`requests.rs:2921-2923, :2944-2949`, "Default: true") |
| D8 | P1 | Silent provider failures in the edit context | file-read failure ⇒ `vb_traps = vec![]` `:2289`, `sync_hazards = vec![]` `:2313`, `full_source = None` `:2203-2205`; a dangling caller source ⇒ silently skipped `:2235` (`if let Ok(Some(..))`) |
| D9 | P1 | Overload identity | same-class multiples take `candidates[0]` `:2196`, `:4316` (comment `:2168-2171` calls it "historical first-candidate behavior"); cross-class ambiguity IS refused `:2172-2194` |
| D10 | P1 | The two tools compute the verdict from different inputs | edit-context: body read, complexity estimated, `compute_blast_radius(.., true)`; edit-safety: no body, complexity 0, `compute_blast_radius(.., false)`. Same `compute_edit_safety` `:902`, different facts ⇒ verdicts can disagree for the same method |
| D11 | P2 | `is_orphan ⇒ RED` conflates "no callers" with "callers not indexed" | `:920-922`, reason `:957-960`; with D4/D8 a method whose callers were all dangling gets RED "may be invoked via reflection" |
| D12 | P2 | No tests | `grep -ln "get_method_edit_context\|check_edit_safety\|get_page_context\|compute_edit_safety" crates/engram_server/tests/*.rs` → none; the file's 10 `#[test]`s (`:4392-4563`) cover SQL table parsing and method resolution only |

What is already right (keep): cross-class ambiguity refusal (D9 second
half); causal-coverage floor from round 5 (`:1009-1041`: causal-truncated
⇒ never green, confidence ≤ 0.5); callers listed as identities by default
with bodies opt-in; `output_json` on all three.

## 3. Live OciusX evidence (actual output, 2026-08-28, gen 824)

**Hot permission helper** `Site/App_Code/users-security/code/accessctrl.vb`
`_us.accessctrl.Check_pr_id` (lines 18-27):

```
get_method_edit_context (markdown):
  # Edit Context: `_us.accessctrl.Check_pr_id`  🔴 RED
  - **Complexity**: 3
  ## Callers (3 shown)
  - **Blast radius**: 40 (Medium)
  - 50 callers — high blast radius
check_edit_safety (json): verdict red 0.5
  - 50 callers — high blast radius
  - Seam candidates present — downstream triggers may fire
compute_blast_radius (same node):
  **Causal dependents (1-hop, may break if this changes)**: 98
  **Unresolved endpoints (dangling sources, quarantined …)**: 259
  **Raw 1-hop degree**: incoming 265, outgoing 443
```

"50 callers" is the D4 cap; the substrate knows the exact number (98) and
the dangling count (259). The edit tools show neither.

**Ordinary API method** `Site/App_Code/installationsobjekt/api-json/api-installationsobjektprojekt.vb`
`api.ioGetIdsFilteredByMarkerCheckListItemStatus` (lines 2205-2261, the PR
2032 sibling):

```
get_method_edit_context: complexity 15 (estimated from body) | called_by 1 | tables [iom_installationsobjektmoments]
                          blast 10.0 Low | verdict yellow 0.7 — "Seam candidates present"
check_edit_safety:        verdict yellow 0.7 — same reason; complexity 0 (D3), invisible only because 15 < 16
```

**Edge population vs the 5000 caps** (`get_codebase_overview`):
`calls 53,370` (cap covers 9.4 %), `queries_table 2,015`, `writes_state 1,065`,
`dependency 2,353`, `temporal_coupling 1,381,614`. Today the method-level
scans (D5) still fit; `get_page_context`'s `calls`/`dependency` scans (D6) do
not, and nothing in the output says so.

**Page** `Site/kmlquery.aspx` via `get_page_context`: 13 methods, 2 tables,
"UI coverage confidence: 95%", no session/AJAX/validation/auth sections — and
no way to tell "the page has none" from "the scan never reached them".

## 4. Redesign

### A. Defects to fix now (each gets a failing test first)

| Fix | Mechanism | Closes |
|---|---|---|
| A1 | `EditContextCompleteness` typed struct on both results: per provider `Complete` / `Truncated{shown, cap, known_total?}` / `Failed(reason)` / `NotRun`. Providers: blast, callers, body, complexity, db_tables, stored_procs, session_reads, session_writes, vb_traps, sync_hazards. Rendered as a `## Coverage` block in markdown and a `completeness` object in JSON | D1, D2, D8 |
| A2 | Verdict rule: any REQUIRED provider (blast, callers, complexity) that is `Failed`/`NotRun` ⇒ verdict cannot be `green`; reason names the provider; confidence ≤ 0.5. Extends the round-5 causal floor (`:1009-1041`) to every provider | D1, D2, D3 |
| A3 | `check_edit_safety` reads the body exactly like the edit context (`read_lines_from_file`) and estimates complexity; both tools call `compute_blast_radius` with the same arguments; a parity test asserts identical `EditSafetyResult` for the same node | D3, D10 |
| A4 | Callers from the blast substrate's exact count: `component_adjacency` / `find_incoming_edges_with_kind` with cap+1 semantics ⇒ `callers: 98 (exact)` or `≥50 (capped)`; renderer `Callers (3 shown of 98)`; the RED/YELLOW reason prints the exact or `≥` form, never a bare capped length | D4 |
| A5 | Replace the four global scans in `build_method_info_from_node` with per-node outgoing lookups (`store.rs:837` structural-edges-touching-node is O(degree)); result is exact and gets `Complete` in A1. Same for the six scans in `get_page_context` | D5, D6 |
| A6 | Schema honesty: wire `include_business_logic` (business-rule lookup for the method via `query_business_logic`'s service) or delete the field; `include_master_page` / `include_codebehind` either gate their sections or are deleted. A test enumerates the three request structs and asserts every field is read (extends the quiet-failure sweep rule) | D7 |
| A7 | Same-class multiples: return the AMBIGUOUS message with signatures (`:2172-2194` shape) unless `class_name` + a new optional `signature` selects one; header prints the signature | D9 |
| A8 | `is_orphan` requires callers `Complete` and dangling = 0; otherwise the reason is "callers unknown (N dangling / provider failed)" and the verdict is yellow-unknown, not RED-reflection | D11 |
| A9 | Tests: `compute_edit_safety` unit suite (each provider Failed ⇒ not green; capped callers ⇒ `≥` text; causal floor kept); handler tests against a temp graph (missing blast node, dangling caller, unreadable file, same-class overloads); parity test (A3); renderer marker tests; schema-field-read test (A6) | D12 |

### B. Redesign that needs evidence first

- **Callee-side context** ("follow the code" through `_rv.<domain>.<method>`
  helpers into LINQ/SQL) is the causal-trace engine (row 7). This doc only
  makes the CURRENT facts honest; it does not add depth.
- **Tiered tool surface** (auditor P0 #6): the pre-edit oracle is one of the
  ~12 core tools. Decide the tiers after rows 1-3 settle the vital list.
- **Verdict calibration**: the thresholds (`>15 callers`, `>40 complexity`,
  `>60 blast`) are uncalibrated. Calibrate against the OciusX merged-PR
  corpus (did edits to RED methods actually produce review findings /
  reverts?) before touching them. Not in scope here.

## 5. Acceptance gate (measured before and after)

| Gate | Measure | Target |
|---|---|---|
| G1 | Every provider failure/cap surfaces in output | test-enforced: 0 `.ok()` / `unwrap_or(0.0)` / `unwrap_or_default()` on provider results in the three handlers without a completeness record (grep + unit tests) |
| G2 | Verdict parity | `check_edit_safety` == `get_method_edit_context` verdict + reasons on 20 OciusX methods (10 hot fan-in, 10 ordinary) via `engram_drive.py`; today they differ whenever complexity > 15 |
| G3 | Caller honesty | for the 20 highest fan-in OciusX methods, the reported caller count equals the blast causal count or carries `≥cap`; today `Check_pr_id` reports 50 vs 98 |
| G4 | Complexity honesty | `check_edit_safety` complexity == `get_method_edit_context` complexity on the same 20; today 0 vs estimated |
| G5 | Schema honesty | every field of the three request structs is read by its handler (test) |
| G6 | Latency | p50 of `get_method_edit_context` on the 20-method set not worse than today (measure first; A5 should make it faster by removing four 5000-edge scans) |
| G7 | Suite | full `--tests --lib` sweep green; the new tests fail when their fix is reverted (mutation-checked) |

## 6. Disposition table (fill at implementation)

| Item | Disposition |
|---|---|
| A1 | |
| A2 | |
| A3 | |
| A4 | |
| A5 | |
| A6 | |
| A7 | |
| A8 | |
| A9 | |
| G1-G7 | |
