# Round-8 live evidence (sanitized) — generation 965

Raw eval output and validate reports reference OciusX paths/symbols (customer
data — [[no-customer-strings-in-source]]) and stay git-excluded. This is the
sanitized, auditable summary: counts and verdicts, no customer content.

**Snapshot:** OciusX, wipe_and_reindex → **generation 965**, 2277 paths complete
(tantivy/vectors/graph all 2277). Binary = round-8 branch head (all 8 commits).

## P0-1 — the three live false-PASS reproductions, RE-RUN live

| Repro (the auditor's exact scenarios) | Round-7 (before) | Round-8 (now) |
|---|---|---|
| Arbitrary text at an existing same-language target — only a generic lint runs | PASS | **INSUFFICIENT** (0 verified, 0 assertion, 2 generic-lint) |
| A token present ONLY in a comment satisfies an expected_session_key | PASS | **INSUFFICIENT** (0 verified, 1 assertion, 1 generic-lint) |
| Code in one language aimed at a different-language target file | PASS | **FAIL** (language/target-extension mismatch) |

The verdict now reports the coverage breakdown (verified / caller-assertion /
generic-lint) so the reason is auditable, not just the badge.

## P0-3 — route provenance, live

A CALLEE question ("which server API functions does `<a client .ts>` call?")
renders the wrapper-mediated hop as:
`… calls api.getimg (function) VIA getImage_wrapper — NOT a direct call; defined in <api impl>.vb`
— no longer presented as a direct call. The Answered status blurb no longer
claims "direct".

## Floors — NO REGRESSION (audit acceptance bar)

| Suite | Floor | Round-8 (gen 965) |
|---|---|---|
| causal (20) | ≥16 | **16** ✓ |
| golden (35) | ≥23 | **23** ✓ |
| blind (8)   | ≥6  | **6** ✓ |

The P0/P1 changes (evidence-based validate coverage, ambiguity-preserving
resolver, edge-kind-gated api routing, boundary-aware file scope, service-identity
method routes) did not move any floor. P0-2 / P1-3 / P1-4 are additionally
covered by RED→GREEN unit tests; the extractor change (P1-1) is validated by this
reindex holding the floors.
