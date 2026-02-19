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
