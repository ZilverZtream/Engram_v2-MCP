# ask_codebase → project brain: design

**Date:** 2026-08-21
**Status:** approved (design); Milestone 1 in implementation
**Branch:** `ask-codebase-brain`

## Problem

`ask_codebase` is advertised as the one natural-language front door — "ask
anything … routed automatically, with provenance. START HERE." The
implementation is a heuristic dispatcher, not a brain. Verified against the
live code (2026-08-21):

- **One-of-five intents** (`Impact | Usage | History | Feature | Explain`),
  chosen by English prefix/substring tests, non-English unsupported
  (`handlers/ask_tools.rs:11,27`).
- **Question reduced to one `subject` string** — the proposed change, scope,
  roles, versions, comparisons, symptoms, error messages, multiple entities,
  and desired answer form are all discarded (`ask_tools.rs:27-193`).
- **Single tool per route**, so Engram's strongest signals are missed: Feature
  calls `plan_user_story` but never `get_change_set` (its own next-step
  advice); Impact assumes the subject is a symbol FQN and skips alias/file/
  table/setting/route/concept resolution, co-change, tests, config consumers,
  permissions, runtime evidence (`ask_tools.rs:232-302`).
- **Explain concatenates five raw sub-tool outputs** with no cross-source
  ranking, dedup, causal ordering, or synthesis (`ask_tools.rs:303-352`).
- **Markdown scraped as an internal API** — evidence presence is detected with
  `t.contains("#1") || t.contains("doc_id")` (`ask_tools.rs:334`). A renderer
  change makes evidence vanish.
- **Epistemic failure hidden**: sub-tool errors become a successful response;
  concept-footprint failure becomes an empty string; team-memory errors are
  ignored. No distinction between empty / stale / misunderstood / ambiguous /
  engine-failed / genuinely-absent (`ask_tools.rs:218,347`).
- **Request is `{ project_id, question }`** — no session, task, as-of,
  audience, or depth (`models/requests.rs:699`).

There is **no** answer synthesis, no claim↔evidence linking, no confidence, no
contradiction detection, no abstention.

## Governing principle (from Engram's own leakage-free eval)

`eval/README.md` is the north star, and it points *away* from "retrieve more":

- `S1_ask` scores **0.133** page-recall — but the **highest precision** (0.11)
  of any arm. The 3-arm ensemble reaches **0.604**; all-11 only **0.618**
  (`eval/README.md:112-165`). Indiscriminate tool-calling has sharply
  diminishing returns.
- The Phase-2 Opus A/B is decisive: the Engram dossier was **net-negative on
  average** (impl 3.2→2.6, recall 0.46→0.33), and on the 1967 bug it **hurt** —
  the agent anchored on the ranked list and diagnosed the wrong file, where the
  no-Engram agent traced the real cause (`eval/README.md:176-215`).

**Therefore: precision, signal-to-noise, and anchoring-avoidance matter more
than raw recall.** A bigger evidence pile is what made agents worse. The
rebuild must resolve, retrieve typed evidence, rank by authority/directness
(not similarity), show conflicts, calibrate uncertainty, and abstain — and
prefer a *small, high-signal* set over a large one.

## Target architecture

```
Question + session/task envelope
  → Query understanding & decomposition   (multi-intent, multi-entity)
  → Entity resolution & ambiguity handling (candidate branches)
  → Evidence-plan construction             (intent-specific DAGs)
  → Parallel typed retrieval               (deadline, budget, shared embedding)
  → Normalize · dedup · authority-rank · detect conflicts
  → Honest status + freshness snapshot
  → [Constrained synthesis → independent verification]   (LLM; Milestone 2)
  → Grounded answer/report + confidence + unknowns + citations
```

## Approved decisions (2026-08-21)

1. **Typed evidence substrate = a new internal evidence-provider layer.** New
   providers produce typed `EvidenceItem`s directly from the substrate (search
   engine, graph store, registry). `ask_codebase`'s arms call *those*, never
   the Markdown-returning handlers. Existing tools are untouched for their own
   callers. Zero Markdown parsing; least blast radius.
2. **Deterministic evidence report by default; LLM opt-in via `depth`.** The
   default is a fully deterministic, typed, authority-ranked, conflict-flagged,
   honestly-statused report — no internal LLM, fast, reproducible. The calling
   agent (already Opus) synthesizes, or opts into `depth=deep` for the
   Milestone-2 internal LLM plan+synthesis+verifier pass. Mirrors the anti-
   anchoring lesson and the support-KB hybrid choice.
3. **Deterministic foundation first, then stop for review.** Milestone 1 is the
   whole deterministic engine, committed and pushed, with a hard review gate
   before the LLM synthesizer/verifier (M2) and conversation memory + hard ACL
   + full golden corpus (M3).

## Milestone 1 — the deterministic evidence engine

New module: `crates/engram_server/src/services/ask_engine/` (mod, evidence
types, providers, planner, resolver, retrieval, ranking, status, render).
`handle_ask_codebase` becomes a thin orchestrator over it.

### 1. Typed evidence model

```rust
struct EvidenceItem {
    evidence_id: String,          // "ev_<n>"
    kind: EvidenceKind,           // SourceCode, DocSection, MemoryNote, Insight,
                                  // BusinessRule, HistoryCommit, GraphRelation,
                                  // ConceptGroup, TestRef, Setting, ...
    authority: Authority,
    path: Option<String>,
    lines: Option<(u32, u32)>,
    symbol_id: Option<String>,
    title: Option<String>,
    content: String,              // bounded snippet
    generation: Option<u64>,
    commit: Option<String>,
    timestamp: Option<u64>,
    confidence: f32,              // extraction/retrieval confidence
    relevance: f32,               // query relevance from the arm
    extraction_method: String,    // ast | fts | vector | graph | git | memory
    warnings: Vec<String>,
    provider: String,             // arm that produced it
}
```

`Authority` is an **ordered** enum encoding the precedence:
`RuntimeEvidence > CurrentCode > ApprovedRequirement > CurrentDocs >
MergedHistory > DerivedBusinessLogic > AgentMemory > DreamerInsight >
SemanticSimilarity`. Precedence never *silently* resolves a conflict — it
orders evidence and decides what to trust *when nothing contradicts it*.

### 2. Evidence providers (typed, call the substrate directly)

`code`, `doc`, `memory` (kind-aware: `decision`/`requirement` →
`ApprovedRequirement`, `note`/`gotcha` → `AgentMemory`), `insight`,
`business_logic`, `history` (commits/diffs/blame), `graph_relations`
(typed edges via the graph store), `symbol_refs`, `concept_footprint`,
`impact` (incoming graph + co-change/temporal), `change_companions`
(get_change_set-style). Each returns `Vec<EvidenceItem>` with **real** line
ranges and symbol ids (hybrid hits already carry them) — no scraping. Where
useful, the low-level logic is shared with existing handlers by calling the
same services, not by refactoring those handlers' signatures.

### 3. Query understanding (deterministic, multi-intent, multi-entity)

Replaces `classify()` with a deterministic planner producing:

```rust
struct QueryPlan {
    intents: Vec<(Intent, f32)>,   // weighted SET, not one-of
    entities: Vec<EntityMention>,  // identifiers, dotted paths, file tokens,
                                  // quoted strings, noun phrases
    qualifiers: Qualifiers,        // roles/tenants, change verbs (X→Y),
                                  // scope words, symptoms/errors, versions
    needed_evidence: Vec<EvidenceKind>,
    answer_type: AnswerType,
}
```

Intent set widened beyond five: `Explain, Impact, Usage, History,
Rationale/Why (≠ History), Feature, BugDiagnosis, Requirements, Compare, Test,
Unknowns`. A question maps to a weighted set (Explain+Impact is normal). The
deterministic planner is exactly the fallback the M2 LLM planner supersedes.

### 4. Entity resolution before retrieval

Each `EntityMention` is resolved across symbol/FQN, file/module, route/endpoint,
table/column, setting/state key, UI page/control, product concept, requirement/
decision, historical alias — via `resolve_id` + graph lookups. Ambiguity
produces bounded candidate branches (investigated cheaply, not jammed into
`symbol_fqn`); the caller is asked only when ambiguity materially changes the
result.

### 5. Intent-specific retrieval DAGs (parallel)

Per intent, a deterministic recipe of provider calls, run concurrently with a
shared query embedding, `deadline_ms`, cancellation token, and per-arm evidence
budget. Examples:

- **Explain**: doc + code + graph-relations + business_logic + memory decisions.
- **Impact**: resolve → incoming graph refs + co-change/temporal + tests +
  config/setting consumers + business rules + change companions.
- **Why/Rationale**: memory decisions + merged PRs + blame/diffs + current impl.
- **Feature**: requirements/memory + concept footprint + implementation
  patterns + change companions + guards + tests.
- **BugDiagnosis**: symptom/error strings + error-path trace + reads/writes +
  conditions + runtime evidence — **causal; similarity only orients, never
  anchors** (the 1967 lesson).
- **Usage**: symbol refs + concept (domain terms) + config/db/route usage.

### 6. Normalize · rank · detect conflicts (the anti-anchoring core)

- **Dedup** across arms by path+lines / symbol_id / doc pk.
- **Score** = weighted(directness, authority, entity-confidence, causal-
  relationship, freshness, corroboration, source-diversity, extraction-
  confidence, relevance). **Semantic similarity is a weak term.** MMR runs only
  *after* authority/directness constraints. Ten weak semantic hits never outvote
  one direct source-line relation.
- **Select a small, high-signal set** — anti-anchoring is a hard requirement,
  not a nicety.
- **Conflicts are shown, not resolved**: high-authority disagreement
  (requirement vs code, runtime vs code) and cross-generation/snapshot mismatch
  are flagged explicitly.

### 7. Honest status + freshness snapshot

Result carries `status ∈ { answered, partial, ambiguous, stale, unsupported,
failed }` plus **per-provider** status distinguishing empty / stale /
misunderstood / ambiguous / engine-failed / genuinely-absent. Freshness is
aggregated into one snapshot (project generation, git commit/branch, memory +
business-logic generations, history watermark); incompatible-snapshot synthesis
is flagged. No more "success containing an apology."

### 8. Request envelope (additive, serde-default, backward compatible)

```
+ session_id: Option<String>
+ task_context: Option<String>
+ as_of: Option<{ branch, commit }>
+ audience: Option<{ role, permissions }>
+ depth: "quick" | "standard" | "deep"   (default "standard")
+ freshness_policy: "best_effort" | "require_current"
+ output_format: "markdown" | "json" | "both"   (default "markdown")
+ deadline_ms: Option<u64>
```

M1 wires `depth` (arm breadth/budget; `deep` reserved for M2),
`as_of`/`freshness_policy` (snapshot pin + stale gate), `output_format`,
`deadline_ms`. `audience`/`session_id` are accepted and carried; full ACL and
conversation memory are M3. Existing `{ project_id, question }` callers keep
working unchanged.

### 9. Output

Structured JSON (plan + typed evidence + status + snapshot + conflicts +
unknowns/coverage-gaps + next-best-investigation) **and** a Markdown rendering
that leads with the question understanding, honest status, ranked **key**
evidence with citations (`path:lines`, authority, freshness), conflicts, and
unknowns — explicitly labelled `retrieval_only`, **not** concatenation.

### 10. Seed golden eval (the promotion gate)

New harness under `eval/` with a small OciusX Q&A set across categories: exact
fact, multi-hop explain, impact, why/rationale, ambiguous name, compound,
missing-knowledge→abstention. Metrics: citation coverage, correct abstention,
latency, plus current-tool baseline vs new engine. **Promotion requires beating
the baseline on signal-to-noise, not merely recall.**

## Security posture (M1)

All repository content (source, docs, memory) is treated as **untrusted data,
never instructions** — no repo text is ever interpreted as a directive to the
engine. `audience` is carried for M3 enforcement. Hard ACL, secret/PII
redaction, remote-provider policy, and prompt-injection isolation land with the
LLM path in M2/M3, before any remote model sees evidence.

## Explicitly deferred

- **M2**: internal LLM planner + constrained synthesis + independent verifier
  (claim-level citations, inference labels, coverage checks, calibrated
  abstention; deterministic + verifier-model double-check for important
  questions). `retrieval_only` report is returned when no LLM is configured.
- **M3**: conversation/task working memory + previous-answer references, hard
  ACL + redaction + prompt-injection isolation + audit records, full golden
  corpus and outcome-based (agent-success) gating.

## Testing strategy

- Unit tests per component: multi-intent planning, entity resolution +
  ambiguity branches, per-provider typed output, authority ranking (weak
  semantic never outvotes direct relation), conflict detection, status
  calibration (empty vs stale vs failed vs absent), envelope back-compat.
- Integration: the deterministic engine on OciusX; the seed golden eval.
- Red-first (TDD) throughout; build with `CARGO_BUILD_JOBS=2`; commit+push per
  tier; OciusX (`5a35e8e0-…`) is the live probe.

## Risks

- **Provider duplication** with existing handlers — mitigated by sharing the
  low-level service calls, not the handler signatures.
- **Latency** from many arms — mitigated by concurrency + `deadline_ms` +
  per-arm budgets + `depth`.
- **Anchoring regression** — mitigated by the hard small-high-signal selection
  rule and the eval gate that scores signal-to-noise, not recall.
