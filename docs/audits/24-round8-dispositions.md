# Round-8 audit — dispositions (2026-09-06)

Round 8 audited `2073807..f137def`. It found one live P0 in
`validate_generated_code`, two more unsafe resolution designs, six P1s, and a
false "sealed ground truth" claim — and closed with the right meta-point: stop
patching query examples into graph policy; fix the architectural boundaries.
Every finding was accepted. This is the fix batch, TDD, per finding.

## What LANDED (committed + pushed on ask-codebase-brain)

| Finding | Commit | What landed |
|---|---|---|
| **P0-1** validate_generated_code grants unearned PASS — vb_traps (generic lint) counts as coverage, `Unspecified` target allows PASS, session keys are raw substrings (a comment satisfies them), no language/target-extension check, caller assertions treated as contracts | `c5f5567` | Each check now carries an **evidence class** (`CoverageClass`: Verified / AssertionOnly / GenericLint / Meta). PASS requires ≥1 **project-derived Verified** check (real-schema table check, or the EXACT method resolved). Verdict FAILs on a language/target-extension mismatch and on a `modify` whose target is not in the index; INSUFFICIENT when the target lookup failed, a modify has no exact target, or no Verified check ran. Comments are stripped (string-literal aware) before every presence check. Tool description rewritten honestly. 17/17 tests incl. the 3 live repros. |
| **P0-2** server-cue resolution `.find()`s the first backend match — no ambiguity detection, no qualifier, no provenance | `6192028` | Extracted the shared `narrow_by_qualifiers`; the server-cue path now COLLECTS ALL backend candidates, narrows by class/file qualifier, selects only when one survives, else PRESERVES the ambiguity as low-confidence branches. Never picks the first. |
| **P0-3** wrapper-mediated call rendered as "direct"; `via` metadata dropped by the callee walk | `22c8502` | The callee walk fetches the source's outgoing api_call edge metadata and carries `via` on the AnswerMember; a mediated hop renders "… VIA `<via>` (→ `<endpoint>`) — NOT a direct call" with relation `<kind>_via_wrapper`. The Answered status blurb no longer claims "direct". |
| **P1-3** api-route rule checks metadata, not edge kind | `137faf9` | Gated: `dispatch_key` ⇒ Calls edge, `ajax_target_method` ⇒ ApiCall edge. RED-verified. |
| **P1-4** `select_method_node` file scope is a loose, case-sensitive prefix (`orders.vb` admits `orders.vb.generated.cs`) | `137faf9` | Boundary-aware, case-insensitive scope: exact file OR directory prefix, never a longer sibling. |
| **P1-1** split_colliding lossy (throws away service identity) + effectively untested (its test used api.asmx, now broker-routed before the split) | `17237da` | Retargets to a SERVICE-ROUTE identity `<service>/<method>` (kind `service_method`) — the auditor's recommended model — so two services' same-named methods stay distinct; real tests on Services/MapData.asmx + a cross-service duplicate-name case. |
| **P1-5** doc 21 "sealed ground truth committed" is false | `d38652f` | Corrected: only the classifier is tracked; the subset + qg_coderabbit.json are git-EXCLUDED customer data; the sha256 is a canonical-payload digest, not the file hash; N reconciled 32→7. |

## Known limits / scoped for a later round (owner-gated architectural items)

The auditor's ruling is explicit: the next round should address BOUNDARIES, not
tune another golden row. These are named honestly rather than half-built:

- **P0-3 (b) — DERIVE the wrapper edge, don't hardcode it.** `getImage → getimg`
  is still emitted by a hardcoded js_extractor rule, not derived from the wrapper
  definition, so it is not invalidated if the wrapper's endpoint changes, and the
  caller is flattened onto getimg rather than carrying the full typed path
  Calls→ApiCall→Exposes with hop count + confidence. The false-edge risk in other
  projects is BOUNDED by the resolver's `starts_with("api.")` gate (an unbound
  target dangles visibly via the round-7 coverage proof). Replacing the hardcode
  with a source-derived multi-hop path is a larger feature. The round-8 fix
  removed the DISHONESTY (mediated calls are now labelled, not called "direct").
- **P1-2 — verify the ASMX `Class=`/`CodeBehind=` exposure before routing** the
  api.asmx broker, instead of recognizing it by filename. Documented in the
  extractor; bounded by the same resolver gate (a non-`api`-class api.asmx yields
  an unbound, visible route, not a silent false bind).

## Verification obligation

P1-1 is an extractor change ⇒ FULL reindex. Live re-run: causal/golden/blind
floors (no regression), the three P0-1 live repros return INSUFFICIENT/FAIL, the
server-cue resolver preserves ambiguity, and a wrapper-mediated call renders with
provenance. Sanitized floor + repro evidence committed for P1-6 (raw eval output
carries OciusX strings and stays git-excluded). Not claiming closure until this
passes.
