# Engram hardening pass 10: evidence reliability

Status: deployed and verified, 2026-09-09. Deployment: 13:56 CEST; live acceptance
completed afterwards. No reference-application code was added to Engram.

Binary SHA-256:
`7C84560C0D85F74C7C4755CDD9A3B940B0DAE4B53D13A203B6F778F7DACCC83B`.
Daemon PID at acceptance: 28232. The previous binary is retained for rollback.
No active jobs were reported before restart. No database schema migration was needed.

## Results and provisional assessments

These scores assess the supported evidence contract, not accuracy percentages,
production certification or exhaustive review coverage.

| Work item | Readiness | Evidence |
|---|---:|---|
| Source freshness | 9/10 | Raw file hashes agree for unchanged CRLF; real edits withhold axes; original reference failure now passes |
| Overload-aware test discovery | 9/10 (was 8) | Exact indexed start-line selection, contradictory/ambiguous requests rejected, sibling callers not promoted to direct coverage; deployed reference overloads pass |
| Review-decision provenance | 9/10 | Immutable transitions, idempotent replay, required external verification evidence, dirty/changed-HEAD invalidation, provider import tests and live imports |
| Explicit review coverage | 9/10 | Submitted/text/binary scope, source snapshots, input digest, head identities, missing evidence and not-run checks exposed; 49 selected integration checks pass |
| Cross-repository acceptance | 9/10 for this bounded suite | Two additional committed C#/Rust reference snapshots plus a separate VB/C# fixture pass isolated and deployed MCP runs |

## Implementation

- `derive_test_matrix` compares the indexer's raw BLAKE3 file fingerprint, not
  normalized chunk content hashes. The regression fixture failed before the fix.
- `find_tests_for_method` accepts `start_line` with an exact file. Returns target
  node/file/line, rejects incomplete candidate scans, and keeps direct edges
  separate from heuristic name matches.
- `record_review_decisions` / `get_review_decisions` retain immutable, source-linked
  observations, accepted exceptions, claimed fixes and external verification
  attestations. Stale attestations are explicit. The full tool surface now has
  147 schemas; the core opt-in surface is unchanged.
- `find_merged_work` includes bounded decision evidence and a full-history handoff.
  Point-in-time replay omits events whose imported timestamps are not verified.
- `pre_commit_review` reports input and source coverage. Changed/unavailable
  snapshots prevent green clearance. Green wording is limited to static gate
  coverage instead of saying the change is safe to commit.
- Provider adapters import saved Azure DevOps/GitHub exports without network
  fetches or posting. Provider resolution is never promoted to verified repair.

See [the evidence contract](../review-evidence.md) for inputs, limits and examples
of the supported distinctions. Existing pattern-learning ingest is separate;
its historical fix-rate heuristics are not reinterpreted as verified decisions.

## Validation

- 1,275 server unit tests passed.
- 49 selected integration tests passed: top-ten audit (17), tool surface (4),
  review gate outcomes (5), review gates (19), degraded review (2), gate caps (2).
- Six provider-import unit tests passed; both adapters passed live import and
  idempotent replay against the daemon.
- Release build, Python compilation and changed-file whitespace checks passed.
- Each isolated/deployed MCP acceptance run passed 27 successful calls and three
  expected rejection cases. Schemas, raw responses, binary hashes, repository
  commits and per-call timings are retained externally.
- Three additional live calls verified the original CRLF and overload cases.
- Live grep: 0.007–0.208 s; get_chunk: 0.003–0.005 s; derive_test_matrix:
  0.004–0.053 s. These are sequential probe observations, not latency percentiles.
- The larger reference snapshot indexed 3,276 files / 25,321 chunks: isolated
  FTS-only 14.668 s; normal Ollama-backed daemon 128.800 s. The other reference
  indexed in 17.205 s on the normal daemon. These are different configurations,
  not an apples-to-apples throughput claim.

External evidence root:
`C:\ai-projects\audits\Engram\evidence-reliability-20260909`.
Accepted manifests: `staging/acceptance-2/results.json`, `live/results.json`,
`deployment.json`; original-case responses: `reference-case`.
The first staging attempt exposed an evaluation assertion that ignored grep's
smart-case behavior. It was corrected to check the case-insensitive query and
the exact returned hit text; the clean rerun passed. No production defect was
hidden or waived to make acceptance pass.

Reference commits: C# `28c6db6c41451f8335b223f3f43b32f9665de5da`, Rust
`45a1036f8d2eadfc4d42af0e51423cbce7f50598`. Reference source snapshots and generated
artifacts are external to this repository.

## Limits

External attestations are not authenticated or executed by Engram. No decision
automatically suppresses findings. A resolved conversation with no explicit
disposition needs human/agent reconciliation against its retained discussion.
Source snapshots are bounded, not atomic filesystem transactions. A supplied
diff is not certified to match the working tree. Dynamic dispatch, executed test
coverage, broad held-out accuracy and concurrent-load certification remain outside
this pass. These limits are exposed to callers rather than hidden behind scores.
