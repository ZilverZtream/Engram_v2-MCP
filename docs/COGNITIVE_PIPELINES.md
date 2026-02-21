# Cognitive Pipelines

This document describes the high-level cognitive processes that run in the background.

## 1. Dreaming (REM Cycles)

- **Triggers**: 
  - `AppEvent::SearchSession`: Records which chunks were returned together in search results.
  - Idle time: Triggered every 20s if no events occur.
- **Inputs**: `CoOccurrence` edges in the graph.
- **Process**:
  1. Cluster chunks based on `CoOccurrence` edge weights.
  2. For each cluster, pull chunk contents.
  3. `DreamingEngine` summarizes the cluster into an `Insight`.
- **Outputs**: 
  - `Insight` node in Graph.
  - `Insight` document in Tantivy (namespace `insights`).
- **Persistence**: Redb (nodes/edges) + Tantivy.

## 2. Temporal Coupling

- **Triggers**: `index_git_history` or `update_project`.
- **Inputs**: Git commit history.
- **Process**:
  1. Walk commits using `git2`.
  2. For each commit, identify files changed together.
  3. Emit pairs of files as `TemporalCoupling` edges.
  4. Increment edge weights based on frequency.
- **Outputs**: `TemporalCoupling` edges in Redb.
- **Tool**: `analyze_temporal_couplings`.

## 3. Style Mimicry

- **Triggers**: `analyze_file_coding_style`.
- **Inputs**: Recent git diffs for a specific file.
- **Process**:
  1. Fetch last N diffs from git.
  2. `StyleMimicryEngine` analyzes indentation, naming, error handling, etc.
  3. Generates a compact Markdown style guide.
- **Outputs**: `StyleGuide` returned to user.

## 4. The Immune System

- **Triggers**: 
  - `index_git_history` (with `index_antipatterns=true`).
  - `immune_check`.
- **Process**:
  1. Detect "revert" commits in git history.
  2. Index the *original* (bad) commit diffs into Tantivy (namespace `antipattern`).
  3. `immune_check` compares new code against these anti-patterns.
- **Outputs**: `Allow`/`Warn`/`Block` decision with confidence score.
- **Persistence**: Tantivy (`antipattern` namespace) + Redb (anti-pattern edges).
- **Safety Calibration** (Phase 27): 7-scenario labeled corpus with `SafetyConfusionMatrix` tracking true/false allow/deny rates. Assertion: false-allow rate on high-risk scenarios ≤ 1%.

## 5. Autonomous Decision Protocol (ADP)

- **Triggers**: `autonomous_decision_gate` tool call.
- **Inputs**: Proposed change description, target files, risk profile, optional pre-computed evidence (extraction confidence, immune verdict, trace metadata, runtime evidence).
- **Process**:
  1. **8-gate pipeline** — each gate evaluates independently:
     - Extraction Confidence: checks WebForms extraction signal scores
     - Trace Certainty: verifies trace paths aren't ambiguous (fallback penalty)
     - Safety Policy: runs `evaluate_safety` against impact/coverage/blast thresholds
     - Retrieval Quality: checks NDCG/Recall/MRR against production gates
     - Blast Radius: computes multi-hop impact, rejects if score > `adp_max_blast_radius`
     - Anti-Pattern: runs `immune_check` against indexed anti-patterns
     - Runtime Evidence: validates presence and quality of runtime confirmation
     - Evidence Sufficiency: meta-gate — ensures enough gates had sufficient data to evaluate
  2. **Verdict computation**: All gates pass → Allow, any hard failure → Deny, insufficient evidence → Abstain
  3. **Rollout policy** (Phase 27): Verdict passes through `apply_rollout_policy()` which enforces the current rollout phase (shadow/advisory/guarded/autonomous). Kill-switch forces all verdicts to Deny.
- **Outputs**: `AdpDecision` with verdict, per-gate results, failed gate IDs, and required follow-up actions.
- **Persistence**: None (stateless evaluation). JSON reports via `build_decision_report()` for auditing.

### ADP Calibration (Phase 27)

- **Deterministic replay**: `replay_from_scenario()` converts serialized inputs into reproducible verdict evaluation
- **Batch corpus testing**: `run_corpus()` processes labeled scenario sets and produces `AdpConfusionMatrix`
- **Confusion matrix**: Tracks true_allow, true_deny, true_abstain, false_allow, false_deny for calibration
- **Rollout phases**: Shadow (log-only) → Advisory (warn) → Guarded (enforce) → Autonomous (auto-apply)

## 6. Runtime Evidence Loop (Phase 27)

- **Triggers**: `ingest_instrumentation_logs` or external runtime event ingestion.
- **Inputs**: `RuntimeEvidenceBatch` — normalized events typed as ControlInteraction, Route, SqlExecution, or StateMutation.
- **Process**:
  1. Validate batch schema via `validate_batch()` (non-empty events, valid timestamps, non-empty event types).
  2. Match runtime events against predicted static trace paths.
  3. Per-path reconciliation: `Confirmed` (runtime matches prediction), `Contradicted` (runtime diverges), `Unmatched` (no prediction for observed path).
- **Outputs**: `ReconciliationResult` with per-path status and summary metrics.
- **Purpose**: Closes the loop between static analysis predictions and actual runtime behavior, feeding back into ADP runtime evidence gate.
