# Definition of done — DRAFT for owner ratification (2026-09-02)

The acceptance bar is the OWNER's, not the auditor's. Audits measure against
this bar; findings become triaged backlog (accept / accept-but-queued /
dispute-with-evidence) on the owner's cadence — never an automatic program
reset. Green in-flight work lands unless a finding shows it is actively
harmful.

## The bar (each item measurable, each with its instrument)

- **D1 — Truthful completeness.** ask_codebase never reports a complete
  Answered while any provider's CoverageProof is incomplete (caps, dangling
  endpoints, errors, unknowns). Coverage state is visible in the MARKDOWN
  answer, not JSON-only.
  *Instrument:* property tests (cap/dangling/error injection) + the live
  canonical probe.
- **D2 — Paraphrase invariance.** Any phrasing of a relation question
  (call/invoke/use/list-every/passive) yields the same answer-member set and
  the same completeness status.
  *Instrument:* metamorphic suite seeded with round-4's four live
  paraphrases; identical-members assertion, not similarity.
- **D3 — Suite floors (never regress).** causal ≥ 16/20, golden ≥ 23/35,
  reference ranks 6/6, Health OK with exact cross-store equality. Floors
  ratchet up when a landing raises them; they never come back down.
  *Instrument:* the per-landing no-repair verify.
- **D4 — Blind yardstick.** Sealed-suite aggregate ≥ 6/8 (current baseline);
  target 7/8. Scored aggregate-line-only; inspection retires the suite and a
  fresh one is sealed.
  *Instrument:* eval/_seal_suite.py verify + aggregate grep.
- **D5 — Ambiguity honesty.** A basename resolving to multiple files yields
  Ambiguous-with-candidates or all-branches-labeled — never a silent
  first-match answer.
  *Instrument:* collision tests + the qtyManager.ts live case.
- **D6 — Judge integrity.** Evaluation compares normalized identity SETS of
  returned answer members; a single fabricated evidence item containing all
  expected names in prose must fail.
  *Instrument:* the round-4 fabrication class as a standing negative test.
- **D7 — Durable evidence.** Every scored run commits its manifest, prompts,
  outputs, snapshot hashes and verdicts; agent A/Bs use opaque randomized
  arm labels and repeated or independent judges.
  *Instrument:* repo presence checks in the landing chain.

## Process rules

1. Audits happen at release milestones the owner chooses, against this bar.
2. Every audit finding gets a written disposition: accept (scheduled),
   queue (with reason), or dispute (with evidence). Disputes are argued,
   not silently folded into new programs.
3. The bar changes only by owner edit to this document.

## Current status vs the bar (2026-09-02)

D1 in flight (doc-15 step 1, c57). D2 not met (round-4 P0-1) — doc-15 step 3.
D3 met at 16/20, 23/35, 6/6, equality. D4 met at 6/8. D5 not met — doc-15
step 4. D6 not met (judge v4 gameable) — doc-15 step 5. D7 not met
(gitignored verdicts, labeled arms) — doc-15 step 5.
