# The typed-brain program (from external audit round 4, doc 14)

Owner decision 2026-09-02: adopt the auditor's prescribed order. The r74
retrieval batch is HELD un-landed: the dangling-`file:` evidence fallback is
DISCARDED (it converted corruption into evidence — the real work is
ingestion/canonicalization plus integrity repair, folded into step 2); the
scan-based symbol-substring arm is SHELVED until it has an indexed name
lookup, measured latency, error propagation and noise tests; the
cardinality-One and modality-domain fixes are folded into steps 3–4 where
they gain the paraphrase/e2e tests the auditor requires.

Discipline unchanged: RED reproducing the auditor's exact finding → GREEN →
full sweep → commit+push → release → deploy → live re-run → disposition →
memory. Batched landings, chain template v2, no-repair verify where
ask-engine-only.

## Step 1 — typed answer members + exact CoverageProof (P0-2 core)

ask_codebase returns structured `answer_members` for relation questions:
`{target_node_id, display_name, relation, source_node_id, path, coverage}` —
the members ARE the answer; the markdown renders them. A typed
`CoverageProof` per provider: discovered/processed sources, edges
available/emitted, per-kind cap state (cap+1 or exact counts), dangling
count, errors, policy. The exhaustive walk counts what it skips instead of
silently dropping it (graph errors, dangling targets, dispatch >2, the 500
caps). The false `an_exhaustive_walk_never_truncates` assertion is replaced
by cap/dangling/error tests.

## Step 2 — fail-closed completeness (P0-2 status side)

Any unknown, error, dangling endpoint, or cap hit in the CoverageProof
prevents complete/Answered. Coverage renders in the MARKDOWN report
(truncation in the provider line; coverage_gaps lists cap hits and dangling
counts). The dangling-`file:` class becomes an ingestion/canonicalization
fix + an integrity-repair path (single node spelling per file; repair
detects and rebinds dual-spelled nodes) — never an evidence fallback.

## Step 3 — semantic/paraphrase-stable contract compilation (P0-1)

Questions compile into a typed relation query — subject, relation,
direction, target type, cardinality, quantifier — normalizing call/invoke/
request/use/depend-on, passive voice, "list", "all", "every" before
execution. The exhaustive lane keys on the compiled query, not phrase
templates. Paraphrase/metamorphic tests assert identical answer members and
completeness status across wordings (the auditor's four live paraphrases are
the RED set). Folds in: the One+Definition contract shape (held F2) with
paraphrase coverage.

## Step 4 — path-aware ambiguity + multi-target execution (P1)

File identity is path/node-id, not basename. `qtyManager.ts → 2` either
executes and labels every branch or returns a real Ambiguous demanding path
qualification — never `named.first()`. The ambiguity grouping keys on
node_id. Folds in: the modality domain-term enrichment (held F4) with
collision tests.

## Step 5 — structured identity-set evaluation (P0-3)

The judge compares NORMALIZED SETS of returned `answer_members` identities —
not substring hits in prose. Distinct-member proof, subject/target
distinction, unexpected-member (precision) accounting. The auditor's
fabricated single-item construction is the RED. Evidence durability: run
manifests, prompts, outputs, snapshot hashes and verdicts committed
(un-ignored evidence dir); Phase-G-style A/Bs get randomized opaque arm
labels and repeated/independent judges.

## Step 6 — only then: the survivor grind + a preserved, arm-blinded A/B

Resume the 16 survivors on top of the typed foundation; re-run the agent A/B
with blinded arms and durable artifacts.

## Also queued from round 4 (slotted between landings)

- allowed_evidence consumers + entity_type constraining members (contract no
  longer ornamental); facet validation with real semantics (Caller ≠ any
  relation; Rationale not unconditional).
- The two forgotten adjuncts: derived-resolution collapse narrowed;
  .d.ts/typings/.coderabbit exclusions liftable by allowed_evidence.
- Blind-suite mechanics: scoring auto-retires on inspection; consider
  encrypted-at-rest storage.
