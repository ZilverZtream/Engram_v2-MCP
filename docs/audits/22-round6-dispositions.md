# Round-6 audit — dispositions (2026-09-05)

Round 6 rejected the round-5 "closure" as premature. The ruling was correct on
the merits: the headline round-5 fix was unreachable, and two of my round-5
dispositions were wrong. This is the honest close-out, verified end-to-end at
the handler level, with a full sweep before any deploy. No new audit is
requested off this doc — it records what landed against named properties and
the limits that remain.

## Findings and dispositions

| Round-6 finding | Disposition | What landed / evidence |
|---|---|---|
| validate_generated_code round-5 fix was UNREACHABLE — the always-on sync-hazard check meant `checks` was never empty, so `class X {}` still returned PASS; the regression test only exercised the extracted helper with a state the handler cannot produce | **ACCEPTED — fixed** | Redesigned around explicit coverage: `ValidationCoverage { contract_checks_ran, target: TargetStatus }`. Verdict is INSUFFICIENT unless ≥1 real contract check ran (sync_hazards/target_file excluded from the count); a nonexistent `modify` target FAILs; a `create` target is not failed for absence; exact-match file resolution, not substring. **8 tests, incl. handler-level** `handler_bare_code_with_no_contract_is_insufficient_not_pass` driving the REAL handler (`validate_generated_code_tests.rs`). |
| prepare_implementation_context "refuses ambiguous methods — Partial; introduces substring false ambiguities"; the correct exact-name preference already exists in select_method_node but was not reused | **ACCEPTED — fixed, and a deeper bug found** | Replaced the hand-rolled `candidates[0]` scan with `select_method_node(...)`. Driving it with a real fixture exposed that **select_method_node itself was broken against the live node model**: function nodes store a class-QUALIFIED `name` (`orders.GetAll`) under a SEARCH namespace (`memory`), so its exact-name filter compared a qualified name to a bare one (never fired) and its cross-class/`class_name` logic keyed off `namespace` (always identical). Fixed to derive class/method from the node (`class_of_node` + new `bare_method_name`). This also repairs the two OTHER callers, `get_method_edit_context` and `check_edit_safety`. **3 handler tests** (`prepare_implementation_context_tests.rs`); edit_context suite still 10/10. |
| prepare advertises "everything an LLM needs to generate correct code in one call" (indefensible) | **ACCEPTED — reworded** | Description now: best-effort providers, failures surfaced in `warnings`, "a starting context, NOT a guarantee — verify against the real code," and the exact-existing-method precondition. |
| pre_commit_review advertises "eleven gates"; nineteen are registered; "full battery" is vague | **ACCEPTED — fixed + tripwire** | Description now lists the real 19-gate set and points callers to the per-gate outcomes the response already returns (so they can see what actually ran). **Tripwire test** `gate_description_parity_tests.rs` binds the advertised count to `all_gates().len()` — the next gate added/removed fails a test instead of silently re-drifting. |
| Agent feature workflow overstates get_change_set ("every file this change should touch", "the backbone") and says pre_push_audit "checks the change" | **ACCEPTED — reworded** | get_change_set is now "a RANKED CANDIDATE dossier … NOT exhaustive (~73–77% page-family recall) … verify/extend before you edit." pre_push_audit is now "RETRIEVES … candidate rules; it does NOT verify your code against them." |
| Doc 20: get_page_context "+0.156 conformance" marked DISPUTED | **MY ROUND-5 DISPOSITION WAS WRONG — corrected** | +0.156 is real and reproducible from `s2_final_scores.json`: dossier arm 0.4227 → house-style arm 0.5788, 13 stories × 2 implementers; `make_closeout.py:65` attributes it to house_style via get_page_context. I had read a phase-1 recall number in the wrong file. |
| Doc 20: 0.38% mechanism recall marked UNVERIFIED | **MY ROUND-5 LABEL WAS TOO WEAK — corrected to VERIFIED** | `replay_mech_cache.json`: 1 genuine `same_issue` match in 202 judgments (0.50%); 3 share a gate label (1.49%). Floor-level, as claimed. |
| Doc 21 feasibility: "~4 null-safety cases", "only 6 of 16 PRs recoverable", gate "not measurable" | **MY ROUND-5 NUMBERS WERE WRONG — corrected** | Tight classifier over `qg_coderabbit.json` = 7 findings / 5 PRs (low teens loose). All implicated PRs have `Merged PR N:` merge commits in OciusX history (1651 total); diffs are recoverable. The probe is runnable; only the small-N caveat is real. |

## Deliberately NOT done (owner-gated)

- **ChangeVerifier analyzer build.** The corrected feasibility says the probe is
  runnable, not that it should be built now. Building it is a program-scope
  decision the owner gated (doc 16). Queued, not folded in.
- **Extractor-side population** of return_type/access_level (the round-5
  `unknown` fix stands until a re-index-bearing extractor change is scheduled).
- **Hard removal** of prepare/pre_push_audit — made honest in place; deletion
  is a larger API decision.

## Known limits (stated, not hidden)

- `method_info.method_name` carries the class-qualified form (`orders.GetAll`);
  cosmetic, pre-existing, not changed here.
- select_method_node's cross-class detection now works, so callers that
  previously got a silent (possibly wrong) pick on an ambiguous name now get an
  AMBIGUOUS error asking for `class_name`/`line`. That is the intended
  correctness change, but it IS a behavior change for ambiguous inputs.
