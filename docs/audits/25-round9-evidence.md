# Round-9 (round-8 re-audit) live evidence (sanitized) — generation 966

Raw eval output carries OciusX strings (customer data) and stays git-excluded.
This is the sanitized, auditable summary.

**Snapshot:** OciusX wipe_and_reindex → **generation 966**, 2277 paths complete.
Binary = round-9 branch head (f8b1850 · c75685d · 7a7cc84). The reindex is
required because the P0-2 fixes change RESOLUTION, which runs at ingest.

## The re-audit's confirmed-live P0/P1 — re-run live

| Finding (auditor confirmed live on gen 965) | Round-9 (gen 966) |
|---|---|
| P0-1 — a FAKE table in `expected_tables` + code earned **PASS** | **INSUFFICIENT** — `schema_consistency: WARN — Referenced table(s) NOT in the project schema: zz_auditor_fake…`; the caller assertion is no longer laundered into Verified |
| P1-3 — `change_kind: "modfiy"` (typo) earned **PASS** | **REJECTED** at deserialization: `unknown variant 'modfiy', expected 'modify' or 'create'` |
| P0-3 — a mediated hop could render as direct | a callee question renders **1 VIA-labelled** mediated hop ("… VIA getImage_wrapper — NOT a direct call"), edge-first |

## Floors — NO REGRESSION

| Suite | Floor | Round-9 (gen 966) |
|---|---|---|
| causal (20) | ≥16 | **16** ✓ |
| golden (35) | ≥23 | **23** ✓ |
| blind (8)   | ≥6  | **6** ✓ |

The P0-2 resolution changes (api-layer gate as Step 0; `route_unique` gated to
`api.`-class) did not move any floor — as predicted, OciusX's api routes still
resolve to the unique `api.<method>` function; only a NON-api unique same-name
symbol now dangles instead of mis-binding. P0-2 / P1-2 / P1-4 are additionally
covered by RED→GREEN unit tests (incl. `api_route_does_not_bind_to_a_unique_non_api_symbol`,
RED without the fix).
