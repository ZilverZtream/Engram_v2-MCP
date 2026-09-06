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
