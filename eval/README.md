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
  at `base_commit` (worktree + `index_git_history`, so co-change/history is live
  and leakage-free), run each candidate Engram sequence, take surfaced files as
  the prediction, score recall/precision/companion-coverage vs the PR's files.
  Winner = empirically optimal `engram-workflow.md` sequence. Run with
  `python eval/run_phase1.py --pilot`. Eval server uses a `%TEMP%` config that
  reuses the production `data_dir` (515 MB embed cache → cheap historical
  re-indexing) with `multi_client` off and the temp worktree root allowed;
  **requires no production engram_server running** (single-writer redb lock).

  Candidate strategies (11):
  - `S0_search` — `search_memory` only (≈ no-Engram baseline)
  - `S1_ask` — `ask_codebase` (NB: for "As a…" stories this routes internally to
    `plan_user_story`, so S1≈S2 on most user stories — a real product behavior)
  - `S2_plan` — `plan_user_story`
  - `S3_full` — concept_footprint → implementation_pattern → similar_changes →
    detect_incomplete_changes
  - `S4_retrieval` — search + vector + grep
  - `S5_graph_fanout` — seed search → `impact_analysis` + `find_symbol_references`
    + `analyze_temporal_couplings`
  - `S6_node_traversal` — node-id seed → `find_references` + `traverse_graph`
    (contains/dependency/imports/calls) + couplings
  - `S7_history_expansion` — `search_history` seed → `find_similar_changes` +
    `detect_incomplete_changes` + couplings
  - `S8_coupling_2wave` — history seed → two-wave temporal-coupling expansion
  - `S9_hybrid_funnel` — plan + concept + search seed → couplings + similar +
    `graph_search` + `graph_centrality_rerank`
  - `S10_cochange_funnel` — concept + search seed → similar + couplings +
    incomplete + rerank
- **Phase 2 — full code-gen + A/B.** Drive the model through the winning
  sequence; LLM-judge the diff + mandate-compliance (ABSOLUTE-TRUTH doc) vs the
  merged PR. A/B: Model-alone vs Model+Engram → the delta is Engram's value.

## Metrics
- **recall_modified_page** *(headline / tournament winner)* — of the PR's
  *modified* files, what fraction did we find at the **page-family** level? A
  WebForms "page" = `foo.aspx` + `foo.aspx.vb` + `foo.aspx.designer.vb`;
  surfacing any member means the agent found the right place, so these collapse
  to one key.
- **recall_modified** — same, at exact-file granularity.
- **recall_all** — over all changed files (incl. added/renamed; see below).
- **recall_all_basename** — forgiving filename-only match.
- **precision** — of files we named, how many were really in the PR? (expected
  low for graph/history strategies that trade precision for recall.)

### Scoring rules learned the hard way (runtime evidence)
- **Path canonicalization is symmetric and prefix-robust.** Different tools emit
  the same file with different prefixes — `Site/App_Code/x.vb`,
  `App_Code/x.vb`, `a/…`/`b/…` (git-diff), `db-ociusx.sql/dbo/…` vs `dbo/…`.
  `canon()` strips these on **both** predicted and ground-truth before
  comparison; without it nearly everything was a false miss.
- **Added & renamed files are excluded from the fair retrieval metric.** Their
  ground-truth path is the *post-PR* name, which does not exist at `base_commit`,
  so no retrieval strategy can find them (`split_truth` puts `add`/`rename` in
  the `added` bucket, scored separately).
- **Code lives in the `memory` namespace**, not `code` (which is only the grep
  term-index). `search_memory`/`vector_search` must use the default namespace.
- **Some stories are near-unwinnable by retrieval.** E.g. PR 1906 ("changes to
  markers logged") was implemented by *adding* ~300 lines of marker logging *to*
  a generic admin `logs.aspx.vb` that, at `base_commit`, mentions "marker" on 1
  of 505 lines. The file has no textual/structural link to the story until the
  PR creates one — the developer used tribal knowledge. The pilot reports the
  full distribution rather than hiding such cases.

## Corpus snapshot (2026-06-13)
80 completed PRs fetched; 56 with a linked US. File-count spread: 5×1-file,
15×2-5, 23×6-15, 13×16+. Marcus Torvang (CTO) gold set: 7 US-linked PRs.
Validated case — PR 1906 → WI 768 "As an admin I want changes to markers to be
logged": 1 file (`logs.aspx.vb`), base commit captured.

## Phase-1 pilot results (2026-06-13, 13 PRs, leakage-free per-story index)

Pilot = 7 Marcus Torvang (CTO gold set) + 3 Dennis Östling + 3 weak/vague
one-liners, across 1–43 changed files. Headline metric = `recall_modified_page`
(fraction of the PR's modified page-families surfaced from the US alone).

**Tournament (mean over 13 stories):**

| strategy | page-recall | exact | precision | avg_pred | per-story wins |
|---|---|---|---|---|---|
| **S3_full** (concept→pattern→similar→incomplete) | **0.458** | 0.438 | 0.05 | 99 | 8 |
| S9_hybrid_funnel | 0.430 | 0.418 | 0.04 | 134 | 8 |
| S10_cochange_funnel | 0.271 | 0.256 | 0.02 | 106 | 2 |
| S4_retrieval | 0.235 | 0.219 | 0.04 | 63 | 1 |
| S6_node_traversal | 0.212 | 0.197 | 0.03 | 112 | 2 |
| S2_plan | 0.164 | 0.146 | 0.05 | 31 | 1 |
| S7_history_expansion | 0.161 | 0.140 | 0.06 | 37 | 1 |
| S8_coupling_2wave | 0.144 | 0.130 | 0.06 | 39 | 1 |
| S5_graph_fanout | 0.139 | 0.131 | 0.04 | 39 | 1 |
| S1_ask | 0.133 | 0.123 | 0.11 | 22 | 1 |
| **S0_search (no-Engram baseline)** | **0.045** | 0.042 | 0.03 | 31 | 0 |

**Headline:** the best Engram flow recalls **~10× more** of the developer's
actually-changed files from the user story alone than plain keyword search
(0.458 vs 0.045).

**No single strategy dominates** — story shape decides:
- *Settings/CRUD* (PR 1933 invoice filters): `S3_full`/`S9` = **0.92** (concept
  footprint pulls the whole `systemSettings` family — resx in every language,
  the store, the SQL). History strategies ~0.08 here.
- *Feature-add with strong co-change history* (PR 1908 upload map markers):
  `S7`/`S8` (history) = **0.875**, but `S3`/`S9` only 0.12. The edited files
  share no concept keyword with the story; only co-change history finds them.

### Recommended job-flow: a 3-arm ensemble (union the predictions)

Computed from the per-story hits (union recall):

| job-flow | mean page-recall |
|---|---|
| no Engram (keyword search) | 0.045 |
| best single (`S3_full`) | 0.458 |
| `S3` + `S7` (concept + history) | 0.561 |
| **`S3` + `S7` + `S6` (concept + history + graph)** | **0.604** |
| all 11 strategies | 0.618 |

The 3-arm ensemble reaches **0.60 — ~13× the no-Engram baseline** — and all-11
barely beats it, so 3 arms is the cost/recall sweet spot. Concrete sequence
(US-only input; later steps consume earlier outputs):

1. **Concept arm** — `get_concept_footprint(concept)` for each story concept →
   `find_implementation_pattern(title)` → `find_similar_changes(seed)` →
   `detect_incomplete_changes(seed)`.
2. **History arm** — `search_history(story)` seed → `find_similar_changes` +
   `detect_incomplete_changes` + `analyze_temporal_couplings` per seed file.
3. **Graph arm** — `search_memory(concept)` / `vector_search(title)` seed →
   `find_references` + `traverse_graph(contains/dependency/imports/calls)`.

Union the three; the result surfaces ~60% of the page-families a developer
eventually touched, given only the user story.

### Caveats
- Precision is low (~0.02–0.06): these flows optimize recall ("find all the
  places") and over-predict. A model consuming the union still benefits — it
  ranks/filters — but a precision-aware reranker is future work.
- Sprawling cross-cutting stories stay hard (PR 1917 user-rights report, 30
  files: best 0.15). Retrieval finds the hubs; the long tail needs reasoning.
- Seeding uses per-keyword `search_memory` + `vector_search` to route around the
  apostrophe bug (TODO P0-0, now fixed) and the multi-word lexical limitation.
