# Round-8 RE-AUDIT — dispositions (2026-09-07)

The round-8 re-audit rejected closure. Its meta-point was correct and accepted:
the round-8 changes improved honesty in the EXAMPLES that were tested, but did not
establish honesty as an INVARIANT. Two of its findings were my own errors — a
false "bounded by the resolver" claim, and a test suite that did not compile from
HEAD. All findings accepted; this is the fix batch, TDD, per finding.

## What LANDED (committed + pushed on ask-codebase-brain)

| Finding | Commit | What landed |
|---|---|---|
| **P0-1** validate still grants a fabricated PASS — a fake table listed in `expected_tables` was whitelisted out of `unknown_tables`, and `schema_checked` only required *some* reference + a non-empty schema, laundering a caller assertion into `Verified` (confirmed live) | `c75685d` | Split into two independent checks: `expected_tables` (AssertionOnly — the token appears) and `schema_consistency` (**Verified only when EVERY referenced table resolves to the indexed schema**; a referenced table not in the schema is UNKNOWN regardless of what the caller expected; schema-unavailable is explicit and NOT Verified). The fake-table repro now yields INSUFFICIENT/WARN, never PASS. Integration test added. |
| **P0-2** the api.asmx shortcut can SILENTLY BIND the wrong method — the generic exact-name match and the `route_unique` fallback both ran without the api-layer class gate, so a non-broker `api.asmx/Foo` could bind to any unique `Foo` (the "bounded by the resolver" claim was FALSE) | `f8b1850` | Both resolution passes gated: `resolve_symbol_edges` runs the api-layer gate as **Step 0, before** the generic ladder and is terminal (binds the single `api.`-class function or leaves a VISIBLE unbound route); `resolve_route_edges`' name-route `route_unique` requires the unique candidate to be `api.`-class. A non-api unique symbol now dangles. The "bounded" claim is now actually true. |
| **P0-3** provenance can disappear while coverage says complete — the `via` was fetched by a separate `edges_touching(...).unwrap_or_default()` join (graph error ⇒ empty; cap shared across kinds; failure/truncation not in the CoverageProof) | `7a7cc84` | Rewritten EDGE-FIRST via new `outgoing_edges_of_kind` returning full typed edges + a truncation flag: `via` rides on the same edge (no lossy join), a graph error is COUNTED, truncation is RECORDED. Test asserts truncation reaches the proof while provenance survives. |
| **P1-2** server-cue resolution still fail-OPEN — a lookup error restored the client symbol as a unique 0.9 answer; the 50-cap had no cap+1; server/backend/calls/method were not excluded qualifiers | `7a7cc84` | Query cap+1 (truncation detectable); a lookup ERROR keeps the client at 0.4, never unique; a UNIQUE server pick only when discovery SUCCEEDED and was NOT truncated; server-cue/generic words added to the qualifier stopwords. |
| **P1-3** change_kind was internally inconsistent — a typo (`modfiy`) and `create`+exists both earned PASS | `c75685d` | `change_kind` is now a strict `ChangeKind` enum: missing ⇒ modify; an unknown value is REJECTED at deserialization; `create` targeting an existing file FAILs. |
| **P1-4** the test suite did not compile from HEAD | `c75685d`+`f8b1850` | Fixed the two stale fixtures (AnswerMember `via`, reserve_required_with `pin_definition`); also the empty-path node no longer admitted into every file scope. `cargo test --all-targets --no-run` restored. |

## P1-1 — partial; the honest state

The extractor now emits DISTINCT service-route identities `<service>/<method>`
(kind `service_method`), so two services' same-named methods no longer collapse
(the round-8 collision fix stands and is unit-tested). BUT the auditor is right
that these targets are **not yet connected to their implementations** — for a
non-broker `.asmx` the served method's function is reached only by resolving the
`.asmx` `Class=`/`CodeBehind=` exposure, which is not done. So "service identity
preserved" is true at the extractor and in the graph's distinctness, but NOT
end-to-end to an implementation. That connection is part of the unified route
model below, not claimed as done.

## The unified route/endpoint model — BUILT + SHIPPED (ruling item 2)

The auditor's ruling item 2 asked for ONE route/endpoint model resolving the
.asmx's `Class=`/`CodeBehind=` rather than special-casing filenames. BUILT
(feat commit + live-verified, gen 984; see 25-unified-model-evidence):

- The api.asmx **filename special-case is REMOVED** (`path_lower_eq_api_broker`
  deleted). Every `<service>.asmx/<method>` call — api.asmx included — emits a
  normal web_service route; the resolver binds it to the served function of the
  class the .asmx DECLARES, via `exposes` keyed by service NAME (so a client
  route target that is a different node than the .asmx markup node still binds by
  the declared class). Test `asmx_route_binds_via_declared_class_not_filename`
  proves the DECLARED class wins over the filename, with a decoy so only that
  path can resolve it.
- The getImage wrapper now emits the SAME `service_method` route shape and
  resolves through the declared class too (no hardcoded api_function target). The
  getimg CONSTANT is still hardcoded in that one rule — deriving it from the
  wrapper body is the residual P0-3(b), now the ONLY route-model item left.
- `service_method` routes (P1-1) resolve to the impl through the declared class.

Live canary PASSES: getimg resolves via the declared Class= path (10 caller
edges, ajax.ts among them); ox_causal_20 returns all 16 API functions with a
VIA-labelled mediated hop. Golden 23/35 and blind 6/8 held.

**Causal 16→15 (owner-ratified ship).** One borderline row, ox_causal_19
("which frontend code depends on ConvertHeicToBase64String?"), dipped to item
precision 0.30. It is NOT a model defect: the model cites all THREE real frontend
callers (fbinstplan.js, imgHandler.ts, imgManager.js — verified in source) plus
the impl, but the row's `required_all` lists only two of them, so the EXTRA
CORRECT citations count against precision, and the semantic code arm adds
wrong-modality `.vb` backend files to a `.ts`-frontend question. The answer is
MORE correct, not less. Per the re-audit's own "fix invariants, stop chasing eval
rows," the owner ratified shipping the correct model at causal 15 rather than
hiding real callers to hold a narrow curated row.

## Verification obligation

Resolution changes take effect at INGEST ⇒ reindex + causal/golden/blind floors
(no regression) + the P0-1 fake-table repro returning not-PASS live + the
edge-first provenance still labelling a mediated hop. Sanitized evidence to
follow. Not claiming closure until it passes.
