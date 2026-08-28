# Row 7 — Causal UI/data tracing: `trace_data_flow`, `trace_ui_event`, `find_connection_path`

Audit date 2026-08-28. Code at `ask-codebase-brain` (fef66ca). Live evidence
from OciusX project `5a35e8e0-d37a-41b3-a250-a26957e7aedb`, gen 824.
Research pass by an isolated Sonnet agent; every citation re-verified by
grep before it was written down.

## 1. Verdict

The auditor's description of `trace_data_flow` is verified on every count
and the live run adds the worst case: it produced a **false causal step**.
The tool extracts ONE method body, runs per-line regexes, recognises a
"method call" only when it is a bare `Name()` on its own line, never walks
a `Calls` edge, and attributes graph edges to the trace by **file-path
substring**, so a `Session["map_iomarker_export_ids"]` write from a
different method in the same file was reported as a step of the traced
method. Every one of the OciusX method's real calls (`_us.UserAccess.
CheckRead`, `_io.installationsobjektprojekt.GetAllByCheckingTotalProject`,
LINQ `db.iom_installationsobjektmoments.Where(..)`) is invisible, nothing
says a call went unfollowed, and the output is Rust `Debug` text with the
structured result discarded and `output_json` ignored.

`trace_ui_event` and `find_connection_path` are honest by comparison (they
say "no path within N hops" and list next steps) with one real bug: the
UI-event tool searches 8 hops and tells the caller it searched 10.

This is exactly why the auditor says retrieval can HURT bug fixes here:
an agent that trusts a false step anchors on the wrong cause.

## 2. Verified defects (`services/data_flow_service.rs` = `dfs`; `handlers/migration_tools.rs` = `mig`; `handlers/cognitive_tools.rs` = `cogt`; `handlers/graph_tools.rs` = `gt`)

| # | Sev | Defect | Evidence |
|---|---|---|---|
| D1 | P0 | One method body, line regexes, no callee following | `dfs:632` `fn extract_method_body(content, method_name)` (single span, VB `End Sub/Function` scan); `dfs:163` `trace_data_flow` iterates `method_body.lines()`; `RE_METHOD_CALL` `dfs:82-83` `^\s*([A-Z][a-zA-Z0-9_]+)\s*\(\s*\)\s*;?\s*$` — bare name, empty parens only; `grep -c "EdgeKind::Calls" dfs` → **0** |
| D2 | P0 | False causal anchoring: graph steps attributed by FILE substring | `dfs:795-797` `let relevant = \|source_id\| source_id.contains(entry_point) \|\| source_id.contains(file_path)`; live: the `WritesState Session["map_iomarker_export_ids"]` step came from lines 111/113 of the file, outside the traced method (2205-2261) |
| D3 | P0 | Structured result discarded; `Debug` output; `output_json` ignored | `mig:619` `Content::text(format!("Trace: {:?}", result.steps))`; `DataFlowTrace` (`dfs:101-121`) also carries `tables_touched`, `state_reads/writes`, `controls_read/written`, `methods_called`, `modern_flow_hint` — all dropped; `TraceDataFlowRequest.output_json` (`requests.rs:2496-2497`) never read in `mig:589-621`; live `output_json:true` output byte-identical to `false` |
| D4 | P1 | Project-wide 10,000-edge caps per kind, before the relevance filter, unreported | `dfs:800, :848, :882, :916` `list_edges_by_kind(.., 10_000)?` (store returns the first N in key order, `store.rs:813-835`) |
| D5 | P1 | Unresolved callee never signalled | dotted or argument-bearing calls (`_rv.x.Y(a)`, `_io.x.Y(a, db)`, LINQ) match no regex and produce no "unfollowed" step; OciusX uses `_rv.`-style dispatch in 26 `App_Code` files |
| D6 | P1 | `trace_ui_event` reports the wrong hop bound | default `max_hops` = 10 (`requests.rs:319`), clamp `MAX_GRAPH_HOPS = 8` (`requests.rs:372`); BFS uses `req.sanitized_max_hops()` `cogt:1148` while the message `cogt:1158-1167` interpolates raw `req.max_hops` — live: "within 10 hops" after an 8-hop search |
| D7 | P2 | `find_connection_path` searched-kind universe unstated; `max_depth` doc says 6, handler clamps to 12 | `gt:551` `req.max_depth.clamp(1, 12)`; `gt:562` `find_path(.., &[])` ⇒ all structural kinds except `TemporalCoupling`/`CoOccurrence` (`store.rs:1362-1370`) — only the per-hop kind is printed |

Already right (keep): `trace_data_flow` propagates file-read and trace
errors with `?` (`mig:596-598`) — no swallowing; `trace_ui_event` resolves
markup control → graph node with ambiguity reporting and gives five
concrete next steps on "no path" (`cogt:1158-1167`); `find_connection_path`
reports "No path within {max_depth} hops … (searched directed then
undirected; synthesized file-membership edges excluded)" (`gt:597-601`),
marks reversed hops `<--`, and states each hop's kind; all three request
structs read every field except D3's `output_json`.

## 3. Live OciusX evidence (2026-08-28, gen 824)

`trace_data_flow` on `api.ioGetIdsFilteredByMarkerCheckListItemStatus`
(`Site/App_Code/installationsobjekt/api-json/api-installationsobjektprojekt.vb:2205-2261`).
Calls present in the source: `_us.UserAccess.CheckRead(..)`,
`GetDictionaryIntegerValue(qry.params, "pr_id")`,
`GetDictionaryStringValue(..)`, `JsonConvert.DeserializeObject(Of ..)(..)`,
`New iFaltDataContext()` + `db.iom_installationsobjektmoments.Where(..)`,
`_io.installationsobjektprojekt.GetAllByCheckingTotalProject(pr_id, db)`,
`s.SetOK/SetError`, `LogError`.

```
0.18 s — Trace: [DataFlowStep { sequence: 1, step_type: "Conditional", description: "Branch: If Not _us.UserAccess.CheckRead(..)" ..},
 { 2 "Conditional" "Branch: If pr_id <= 0" }, { 3 .. "If item.statusId = _io.MarkerCheckListItemStatus.NotCompleted" },
 { 4 .. "ElseIf .. CompletedOrInProgress" }, { 5 .. "Else" },
 { 6 "GraphEdge" "Graph: writes Session[\"map_iomarker_export_ids\"]" source: "value" .. }]   ← lines 111/113, another method
output_json:true — byte-identical
```

Zero of the eight real calls appear; zero "unfollowed" notes; one false
step.

`trace_ui_event` on `rowInsert` (`…/admin/production/productioncodelistcategory.aspx`, handler `rowInsert_Click:189`) — 0.14 s:
"No paths found from control:…:rowInsert to any SQL nodes **within 10
hops**" + 5 next steps. Actual BFS depth: 8 (D6).

`find_connection_path(_us.accessctrl.Check_pr_id → api.ioGetIdsFilteredByMarkerCheckListItemStatus)` — 3.0 s:
3 hops, undirected, `<-- [calls] _api2.svc.ImageService.ValidateMapFeatureImageAccess:342 --> [calls] _us.UserAccess.CheckRead:387 <-- [calls] api.ioGetIds…:2205`.
Honest and useful; the searched kind set is not stated.

## 4. Redesign

### A. Defects to fix now (each gets a failing test first)

| Fix | Mechanism | Closes |
|---|---|---|
| A1 | Graph steps attributed to the traced METHOD NODE (its own id / line span), never by file substring; a step from another method in the file is impossible by construction | D2 |
| A2 | Callee resolution: every call expression in the body (dotted, with arguments, LINQ table access) becomes a step — `Resolved(node)` when the `Calls` edge or symbol lookup finds it, `Unresolved(text, reason)` otherwise; the count of unresolved calls is in the header | D1, D5 |
| A3 | Bounded recursive follow: depth ≤ 3 through `Calls` edges into helpers / domain classes (`_rv.`, `_io.`, `_us.` style), stopping at SQL / LINQ table access / state / UI response with the terminal kind stated; per-hop budget and "stopped at depth N" reported | D1 |
| A4 | Render the full `DataFlowTrace` (steps, tables, state reads/writes, controls, methods, hint) as markdown; honour `output_json` with the same struct serialized | D3 |
| A5 | Replace the four project-wide 10k scans with per-node edge lookups (O(degree)); if any cap remains it is reported | D4 |
| A6 | `trace_ui_event` message uses the sanitized hop count; `find_connection_path` states the searched kind set and the 12-hop ceiling; request docs match the clamp | D6, D7 |
| A7 | Tests: fixture with two methods in one file (the false-step regression); dotted/argument calls resolved + unresolved counted; recursion depth and stop reasons; JSON round-trip; hop-message parity; kind-set statement | all |

### B. Redesign that needs evidence first

- **Value-level data flow** (which parameter reaches which column) is a
  different engine (taint-style) — not needed for "follow the code"; defer
  until row-2/row-1 evidence shows agents need it.
- **Cross-language hops** (TS `q` framework → API endpoint → VB handler):
  `trace_ui_action` exists for the DOM/AJAX side; joining it to this trace
  needs the route table (`RouteConfig.vb`) modelled as edges — measure how
  often bug stories cross that boundary before building it.

## 5. Acceptance gate

| Gate | Measure | Target |
|---|---|---|
| G1 | False-step regression: tracing the probe method never reports the line-111 session write | test + live |
| G2 | On the probe method, ≥ 6 of the 8 real calls appear as steps (resolved or explicitly unresolved) | today 0 |
| G3 | A depth-2 follow reaches `GetAllByCheckingTotalProject` and its table access from the probe method | today: no follow |
| G4 | `output_json:true` returns valid JSON with the full trace | today: Debug text |
| G5 | Hop counts in messages equal the searched depth | today 10 vs 8 |
| G6 | Latency ≤ 2 s per trace at depth 3 on OciusX | today 0.18 s, shallow |
| G7 | Sweep green; new tests mutation-checked | |

## 6. Disposition table (slice 1 landed 2026-08-28, b0369f6)

| Item | Disposition |
|---|---|
| A1 | **fixed (slice 1)** — entry node ids resolved by `query_nodes(function, name, file)`; graph steps come from `edges_touching_with_coverage` on THOSE ids only; a step from another method in the file is impossible by construction (regression test with two methods in one file) |
| A2 | **fixed (slice 1)** — every call expression (dotted, with arguments) is a `MethodCall` step with `resolved: Some(bool)`; keywords excluded; `unresolved_calls` counted on the trace header |
| A3 | **fixed (slice 2)** — `follow_calls`: from the entry nodes' outgoing `Calls` edges, each callee's own edges are expanded (table/SQL/state = terminal step `↳ depth N: …` with `details.depth`; further `Calls` one level deeper) up to `FOLLOW_DEPTH_CAP` = 3; `FollowCoverage { depth_cap, edge_cap, followed, truncated_nodes, stops }` on the trace + markdown; every stop stated ("A (depth 3) → B: depth cap 3 reached — not followed", "> 500 edges — partial", "no data/state access and no further calls") |
| A4 | **fixed (slice 1)** — `render_data_flow_markdown` (steps / tables / state / controls / methods / hint); `output_json` serialises the same struct |
| A5 | **fixed (slice 2)** — per-node edge lookups; the 500-edge cap is reported for the entry node (`truncated_nodes`, stop line) and for every followed node |
| A6 | **fixed (slices 1+3)** — `trace_ui_event` message uses the sanitized hop count (test); `find_connection_path`'s no-path message states the kind set ("all edge kinds, directed then undirected; synthesized file-membership edges excluded") and the clamp ("max_depth is clamped to 1..=12 (requested 50, searched 12)") — `tests/connection_path_message_tests.rs`, RED first |
| A7 | **partly (slices 1+2)** — `tests/trace_data_flow_tests.rs` x5 (+ qualified-member resolution, a LIVE finding: App_Code members are indexed as `_rv.x.Method`, slice 1 compared bare names ⇒ 8/10 real calls "unresolved"; `callee_name_matches` + 5 unit cases), `tests/trace_data_flow_follow_tests.rs` x2 (depth-2 table reached with the depth on the step; depth-5 table NOT reached, stop names node + call). Open: `find_connection_path` kind-set statement (A6) |
| G1 | **met (test + live)** — §7: 0 occurrences of the other method's Session write on the probe |
| G2 | **met (live)** — 10 calls are steps; after slice 2, 7/10 resolve live (the 3 left are framework calls: JsonConvert, the DataContext constructor, `s.SetOK`) |
| G3 | **met (test); live FAILED after slice 2, cause found and fixed in slice 3** — the live follow expanded 0 nodes because the ENTRY node is indexed as `api.ioGetIds…` and the entry filter compared bare names (`entry_ids` empty on every App_Code method — which also means slice 1's live "no false step" was vacuous). Fixed with the callee rule; live re-run after the slice-3 deploy |
| G4 | **met (test)** |
| G5 | **met (test)** |
| G6 | **met (live, slice 1)** — probe trace 0.2 s class; slice-2 depth-3 latency measured after its deploy |
| G7 | sweep10 green (110 suites, slice 1); slice 2 in sweep12 |

## 7. Live evidence — slice 1 (2026-08-28, commit b0369f6 deployed 22:56, OciusX gen 828)

`trace_data_flow` on `api.ioGetIdsFilteredByMarkerCheckListItemStatus`
(`Site/App_Code/installationsobjekt/api-json/api-installationsobjektprojekt.vb`):

```
# Data flow: `ioGetIdsFilteredByMarkerCheckListItemStatus`
steps 15 · tables 0 · state reads 0 · writes 0 · controls read 0 · written 0 · calls 10 (8 unresolved)
1-5 [Conditional] … (the same branches as before)
6  [MethodCall] Call: s.SetError(…) — resolved to an indexed function
7  [MethodCall] Call: _us.UserAccess.CheckRead(…) — UNRESOLVED (no indexed function named CheckRead)
8  … GetDictionaryIntegerValue — UNRESOLVED
12 [MethodCall] Call: _io.installationsobjektprojekt.GetAllByCheckingTotalProject(…) — UNRESOLVED   ← wrong, see below
13 [MethodCall] Call: markerIds.AddRange(…) — resolved
… 15 steps; output_json: true → the same structure, `unresolved_calls: 8`
```

- **G1 (false step)**: `map_iomarker_export_ids` — 0 occurrences (the audit's step 6 is gone).
- **G2 (calls visible)**: 10 call steps, each with `resolved: bool` (audit: 0).
- **G5**: `trace_ui_event(rowInsert)` → "No paths found … **within 8 hops**" (was 10).
- **New defect found by the live run**: `GetAllByCheckingTotalProject` and
  every other App_Code member reported UNRESOLVED because those nodes are
  indexed with qualified names (`_io.installationsobjektprojekt.GetAllByCheckingTotalProject`)
  and slice 1 compared the bare name. Also `s.SetError` / `markerIds.AddRange`
  "resolved" by bare-name coincidence. Fixed in slice 2
  (`callee_name_matches`, RED test with a qualified node) — the live
  re-run of this probe after the slice-2 deploy is the acceptance for G2/G3.

## 7b. Live evidence — slice 2 (2026-08-29 00:17 deploy, commit e400653)

Same probe. `calls 10 (3 unresolved)` — was 8 unresolved: `_us.UserAccess.CheckRead`,
`GetDictionaryIntegerValue/StringValue`, `LogError` and
`_io.installationsobjektprojekt.GetAllByCheckingTotalProject` now resolve
(qualified member names). 0.64 s incl. the JSON-RPC round trip.

```
## Follow — depth cap 3, edge cap 500: 0 callee node(s) expanded, 0 node(s) truncated, 0 stop(s)
```

**Gate G3 FAILED live**: nothing was followed. Diagnosis: `collect_graph_steps`
resolved the entry node with `name == entry_point`; the live node is named
`api.ioGetIdsFilteredByMarkerCheckListItemStatus`, so `entry_ids` was
empty — no graph steps, no follow, and the slice-1 "no false step" result
was vacuous for App_Code methods (page methods, indexed bare, were fine).
Slice 3 applies the callee rule to the entry (`tests/trace_data_flow_tests.rs`
`a_qualified_entry_node_still_gets_its_graph_steps_and_follow`, RED first);
the probe is re-run after its deploy.
