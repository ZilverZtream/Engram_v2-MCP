# Round-5 audit — dispositions (2026-09-05)

Owner adopted ChangeVerifier v1 after seeing the full evidence, then told
Claude to implement what is best for Engram. The judgment call: every audit
rejection across five rounds traces to the same root — tools reporting
success or completeness they have not earned. So the first work is a
systematic **honesty pass** on the six verified defects, before any new
ChangeVerifier machinery. Then a single falsifiable ChangeVerifier probe.

## Verification (before deciding)

Each round-5 claim was confirmed or disputed with live/source evidence:

| Claim | Disposition | Evidence |
|---|---|---|
| get_method_edit_context reports wrong signature for Check_pr_id (Sub/Private) | **CONFIRMED** | Source is `Public Shared Function Check_pr_id(...) As Boolean` @accessctrl.vb:18; tool showed Sub/Private. Root cause: display fabricates defaults when the VB extractor leaves return_type/access_level empty. |
| Agent workflow omits get_change_set | **CONFIRMED** | planning_tools.rs feature steps had plan_user_story/concept_footprint/find_implementation_pattern/pre_push_audit; no get_change_set — contradicts doc 19. |
| Advertises eleven gates, registers nineteen | **CONFIRMED** | tools.rs:896 "eleven" vs 19 `Box::new` in gates.rs. |
| pre_push_audit labels retrieved docs "Checked" | **CONFIRMED** | Live probe (nonexistent .cs) returned "Checked: 4 rule(s)" of unrelated TS/JS rules. It retrieves; it does not check. |
| validate_generated_code PASS on a nonexistent file | **CONFIRMED (safety)** | Every check guarded by `!expected_X.is_empty()`; verdict defaults PASS on empty; never checks file existence. |
| prepare_implementation_context degraded duplicate | **CONFIRMED** | candidates[0] (no ambiguity refusal), `.ok()` silent body-drop, capped HasColumn 5000 scan. |
| 0.38% mechanism recall on pre_commit_review | **UNVERIFIED** but plausible; aligns with the recorded "rule-bank self-match-inflated / file-level catches inflated" finding. |
| get_page_context "+0.156 conformance at n=12" (auditor's POSITIVE claim) | **DISPUTED** | 0.156 is a phase-1 `recall_modified` metric in phase1_pilot.json, not a page_context conformance delta. The positive claim does not check out — same rigor both directions. |

## Fixes applied — the honesty pass

- **validate_generated_code fail-closed** — extracted `compute_validation_verdict`; empty checks → `INSUFFICIENT` (not PASS); added a target-file existence check (nonexistent → FAIL). RED→GREEN.
- **get_method_edit_context signature** — stop fabricating `Sub`/`Private` when the extractor leaves metadata empty; report `unknown`. (Extractor-side population is a queued follow-up needing re-index.)
- **pre_push_audit wording** — "Checked:" → "Retrieved (NOT verified against your code)".
- **gate count** — drop the false "eleven".
- **agent workflow** — get_change_set is now step 1 for feature work.
- **prepare_implementation_context** (separate commit) — refuse ambiguity instead of `candidates[0]`; surface body-read failures instead of `.ok()`.

## Not done (deliberately)

- **ChangeVerifier full program** — one falsifiable null-safety probe with a
  pre-registered gate first, per the discipline that killed the last verify
  direction cleanly. No wholesale build on the auditor's say-so.
- **Retiring pre_push_audit / prepare_implementation_context outright** — made
  honest in place; hard removal is a larger API decision, not folded in
  silently.
