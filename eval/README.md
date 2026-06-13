# Engram ↔ OciusX evaluation harness

Measures whether **Model + Engram, given only a User Story, lands on the same
implementation a developer eventually merged** — and finds the *Engram tool
sequence* that gets closest. OciusX is a live repo: **everything here is
read-only** (DevOps GET + throwaway git worktrees; the live working tree is
never touched).

## Hard rules (validity)

1. **US-only input.** The model + Engram see ONLY the user story (work-item
   title/description/acceptance — what a developer gets, sometimes just a
   headline). They NEVER see the PR (title/description/comments/files): the PR
   describes the implementation and would leak the answer. In the dataset,
   `story` = input, `ground_truth` = scoring-only, physically separated.
2. **No temporal leakage.** For each story, index OciusX at `base_commit`
   (master *before* that PR merged) so the index can't already contain the
   answer (current code, the PR's diffs, its co-change edges).
3. **Read-only.** DevOps GET only; OciusX accessed via `git worktree` at the
   historical commit in a temp dir. Never write to OciusX.

## Phases

- **Phase 0 — corpus (`ado_fetch.py`, done).** Completed→master PRs with linked
  US, changed-file ground truth, author, and `base_commit`. PAT from `ADO_PAT`
  env or `eval/.secrets/ado_pat.txt` (gitignored). `--author Torvang` isolates
  the CTO's gold-standard set.
- **Phase 1 — strategy tournament (deterministic, no model).** Per story: index
  at `base_commit`, run each candidate Engram sequence, take surfaced files as
  the prediction, score recall/precision/companion-coverage vs the PR's files.
  Winner = empirically optimal `engram-workflow.md` sequence.
  Candidate strategies: S0 search-only (≈ no Engram), S1 `ask_codebase`,
  S2 `plan_user_story`, S3 full mandated flow, S4 retrieval-heavy.
- **Phase 2 — full code-gen + A/B.** Drive the model through the winning
  sequence; LLM-judge the diff + mandate-compliance (ABSOLUTE-TRUTH doc) vs the
  merged PR. A/B: Model-alone vs Model+Engram → the delta is Engram's value.

## Metrics
- **Recall** — of the PR's changed files, how many did we identify? (the
  "find all the places" value — the headline number)
- **Precision** — of files we named, how many were really in the PR?
- **Companion-coverage** — did `detect_incomplete_changes` catch the
  settings/permission/admin-page files? (Engram's core promise)

## Corpus snapshot (2026-06-13)
80 completed PRs fetched; 56 with a linked US. File-count spread: 5×1-file,
15×2-5, 23×6-15, 13×16+. Marcus Torvang (CTO) gold set: 7 US-linked PRs.
Validated case — PR 1906 → WI 768 "As an admin I want changes to markers to be
logged": 1 file (`logs.aspx.vb`), base commit captured.
