# Round-7 audit — dispositions (2026-09-06)

Round 7 found three release-blocking correctness failures and six P1 gaps.
Nearly all were correct — including two things I had claimed fixed that the
auditor reproduced LIVE. This is the fix batch, TDD, in the auditor's required
closure order. Not surfaced for a new audit until reindex + full regression.

## P0 — release blockers (all ACCEPTED + fixed)

| Finding | What landed |
|---|---|
| **P0-1** validate_generated_code rejects real indexed files — `query_nodes(name=<full path>)` matches `Node.name` (the basename), so a real path never matches and a modify FAILs "not in the indexed project" | Identity via `get_node("file:{norm}")` first, with a basename-query + exact case-insensitive path fallback for spelling/case drift. Test `handler_exact_existing_modify_target_is_not_rejected`. |
| **P0-2** fake caller compatibility earns PASS — `code.contains(orig_name)` (substring) becomes `caller_compatibility: pass` counting as contract coverage, even with no target and no resolved method | Check 6 rewritten: real coverage requires resolving the EXACT method (exact target + exact name); even then it is a WARN advisory ("this tool does not parse/compare signatures — verify manually"), never a clean PASS. Unresolved → a non-coverage `advisory` (excluded from `contract_checks_ran`). Test `handler_bare_name_containment_is_not_caller_compatibility_pass`. |
| **P0-3** ask coverage ignores dispatch truncation/errors — `complete()` never checked `dispatch_truncated`; find_dispatch_targets errors, missing/unreadable impl nodes all silent | `complete()` now requires `dispatch_truncated == 0`; the dispatch block counts find_dispatch_targets Err (graph_errors), missing impl node (dangling_targets + id) and read Err (graph_errors); the report names dispatch truncation in "_incomplete because_". Test `dispatch_truncation_forbids_completeness`. |

## P1 (ACCEPTED + fixed)

- **P1-1** ox_causal_20 semantically incomplete — the `api.ajax().getImage(...)`
  wrapper (POST `/api.asmx/getimg` → `api.getimg`) was omitted. Added a
  js_extractor rule emitting a wrapper-mediated api_call to `getimg`
  (metadata `via=getImage_wrapper` for provenance); added `getimg` to the
  ox_causal_20 ground truth (15 → 16). Test `getimage_wrapper_routes_to_getimg`.
- **P1-2** API resolver policy over-broad — fired on ANY `ajax_target_method` or
  `dispatch_key`. Now requires `ajax_transport == "api_name"` (for the ajax
  case) and restricts the chosen candidate to a FUNCTION node in the `api.`
  class. Negatives added: WebMethod-not-rebound, no-transport-not-resolved,
  non-function-not-selected.
- **P1-3** ValidationCoverage not returned — `ValidationReport` now serializes
  `coverage` (target status + contract_checks_ran) in JSON and names it in
  Markdown; the "INSUFFICIENT (nothing was verified)" badge is corrected to
  "no project contract verified".
- **P1-4** prepare description falsely promised warnings — reworded to state
  optional providers are best-effort and MAY be SILENTLY ABSENT (only the
  target body-read failure is warned); an empty section does not mean nothing.
- **P1-5** select_method_node capped before exact match — switched to
  `query_nodes_by_symbol_name` (exact/suffix match DURING the scan, cap after
  matching) + function-type filter.
- **P1-6** ChangeVerifier ground truth not sealed — committed
  `eval/nullsafety_classifier.py` + immutable `nullsafety_subset.json` (7
  findings / 5 PRs, sha256 sealed); reconciled doc 21's loose "32" vs tight "7".

## Verification obligation before any next audit

Per the auditor's closure order step 8: FULL graph reindex (the extractor +
resolver changes need it) + causal/golden/blind regression + live re-checks of
the P0-1/P0-2/P0-3 reproductions + ox_causal_20 now yielding getimg. Not
claiming closure until those pass.

## Addendum 2026-09-06 — the getimg/ox_causal_16 saga + retrieval fixes (owner: pursue the BEST fix)

P1-1's getImage rule correctly made getimg a member of ox_causal_20, but adding
getimg-caller edges regressed the compound caller questions ox_causal_16 ("which
TS calls getimg") and ox_causal_18 ("who calls DeleteImage on the server") —
causal dropped 16→15. Owner direction: do NOT take the easy revert; fix the real
problem. Journey (each step verified live + full-floor):

1. **WebMethod-fetch additive edge** (6d18323) — REVERTED (e7d25f9). Emitting a
   method edge for EVERY service call flooded retrieval (ox_causal_16 →
   Unsupported). Net-negative.
2. **Surgical collision fix** (f47265c) — the real defect: `dedup_edges` collapsed
   a file's multiple api_call edges to one service (keyed (source,service)),
   silently dropping all but one method — imgHandler.ts's getimg call was
   clobbered by ConvertHeicToBase64String, so imgHandler was not a graph caller
   of getimg. `split_colliding_service_methods` retargets each edge to its method
   ONLY when a service carries 2+ methods for a file; single-method calls keep
   their web_service target (no flood, no broad change). imgHandler restored as a
   getimg caller.
3. **Retrieval fixes** (query-time, no reindex): (a) a SERVER cue
   ("server"/"web method"/"implements") disambiguates a client/server name clash
   toward the server def (DeleteImage → VB api.DeleteImage), incl. the
   client-unique case; (b) "who calls X" requires the Definition facet; (c)
   `reserve_required` PINS the queried symbol's definition past the cap when
   required — the definition was retrieved but a symbol with many callers crowded
   it out.

**Result — ALL THREE FLOORS MET on the reindexed graph (gen 963):** causal
**16/20** (ox_causal_18 recovered; fails = the 3 precision survivors
ox_causal_3/12/17 + ox_causal_16), golden **23/35** (same 12 documented
survivors, NO new fails), blind **6/8**. getimg completeness (P1-1) delivered,
graph made correct, no data reverted to game the eval. P0-1/P0-2 still hold live.

**One remaining item at the met floor:** ox_causal_16 requires the ajax.ts
getImage wrapper cited among getimg's callers; it drops from the caller-citation
cap. Recovering it needs a caller-completeness change that risks the item-
precision survivors (already at the 0.34 edge), so it is left at the met floor
rather than risk the floors for one row.
