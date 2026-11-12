# Engram ↔ OciusX evaluation harness

## Current task-evaluation verification

`frozen_task_eval.py` verifies a separately recorded private corpus of scoped
policy, companion-edit, history and diagnosis tasks. Candidate agents receive
only their frozen task/source packets, with or without the actual production
context response. Both arms can request the same bounded frozen files. A fresh
independent judge receives opaque candidate IDs without arm labels or metrics.
Task criteria and a judge-only leakage canary are frozen before candidate runs.

The November 3 pilot reran all eight arms using the same authorized read-only
packet delivery after a manual-copy transcription error. The earlier attempts
remain recorded and excluded. Opaque packet files preserve the frozen task and
source values; each candidate may read its delivery packet and request only
allowlisted evidence. Verification checks the delivery protocol, source bytes,
response shapes, requested-versus-delivered reads and blinded judging records.

Both conditions met 20/20 frozen criteria across four tasks, with no false
guidance identified by the independent judge. Automatic context reduced
requested evidence IDs from 10 to 6, while recorded packet plus delivery-message
bytes increased from 202,876 to 249,825 (23.1%). Both conditions had the same
evidence allowlist. This pilot does not measure file discovery, provider token
cost, pure inference latency, or completed implementation/QA effort. These
inspected cases can serve as regressions; future tuning needs new frozen tasks
for another holdout comparison.

```powershell
python -B eval/frozen_task_eval.py verify <recorded-corpus> --require-results
python -B eval/frozen_task_eval.py report <recorded-corpus>
python -B eval/frozen_task_eval.py check <recorded-corpus>
```

The complete check requires eight captured candidate runs and their independent
judgments. Input-only verification explicitly reports that candidate runs have
not been verified. Checking saved runs is an offline regression/integrity check;
fresh agents supply a new model evaluation. Reported agent wall time includes
coordinator handoff and requested-file waits, not pure inference latency. These
review/plan tasks do not establish completed OciusX implementation or matched QA
productivity. Private source packets remain outside Git in the recorded evidence
directory. Manual collection and offline verification open no index, database,
server or model. The explicit `execute` command below launches models.

Use `check` as the final quality step in regular verification after collecting
and judging a run. It first verifies the complete frozen run, then exits 1 if
any assisted task has unsatisfied or withheld criteria, no criteria, or recorded
false guidance. Honest withholding remains visible; it does not establish task
completion. Baseline failures remain in the comparison and do not fail the
assisted quality check. `report` stays informational and exits successfully for
valid recorded judgments even when task criteria failed.

This command checks the recorded run only. To assess another installed build,
capture its current outputs and execute a new run before calling `check`; passing
an old corpus cannot certify a new binary. Private source and model replies stay
outside Git. The existing benchmark CI tests the command's pass/fail behavior
with synthetic recorded runs, including false guidance and unmet criteria. It
does not execute private OciusX tasks or models. Regular CI execution of those
private current-output tasks remains unconfigured.

### Collecting another run

`frozen_task_run.py` collects new responses without rewriting the original
corpus or its results. Use a new private directory outside Git. It copies only
frozen inputs, prepares eight opaque candidate packets, and records requested
reads under the existing limits: three IDs per turn and eight per candidate.
Candidate responses retain their original bytes for cost accounting.

```powershell
python -B eval/frozen_task_run.py prepare <recorded-corpus> <new-private-run>
python -B eval/frozen_task_run.py status <new-private-run>
python -B eval/frozen_task_run.py bind <new-private-run> <candidate-id> --agent-id <fresh-agent-id>
python -B eval/frozen_task_run.py record <new-private-run> <candidate-id> --agent-id <same-agent-id> --response <response.json>
```

The coordinator must use a fresh, isolated agent for each candidate, deliver
only the exact packet instruction printed by `status`, and continue that same
agent when it requests reads. The commands do not launch agents or grant
permission to do so. An agent identity in a ledger is not proof of isolation;
retain the actual execution provenance separately. Never supply old answers,
the oracle, the other arm, or prior results to candidates. Status output omits
the arm mapping. Keep the whole run directory private, including transcripts.

After all eight candidates finish, `status` names a blinded judge packet.
Give only that packet to a separate fresh judge. Record its JSON array with:

```powershell
python -B eval/frozen_task_run.py judge <new-private-run> --agent-id <fresh-judge-id> --response <judgments.json>
python -B eval/frozen_task_eval.py report <new-private-run>
```

Changed recorded packets, reused agent identities, exceeded read limits,
leakage canaries, and a judge task/oracle differing from the frozen originals
are rejected. Interrupted runs retain evidence; an existing lock must be
investigated, not deleted because of its age. `--kind protocol_test` labels
synthetic protocol exercises. Benchmark CI runs these protocol tests only;
it does not launch models or establish fresh task quality. A completed
collection remains an evaluation of historical frozen context, not a new
query against the current Engram build or matched implementation/QA benefit.

### Executing the frozen comparison with Codex

To capture current installed producers before using that same comparison flow:

```powershell
python -B eval/frozen_task_run.py prepare-current <input-spec-directory> <new-private-corpus> --pins <pins.json>
python -B eval/frozen_task_run.py prepare <new-private-corpus> <new-private-run> --delivery-mode manual_packet_v1
```

`prepare-current` connects only to an already running Windows Engram primary.
It does not start services, change configuration, open databases or run models.
`pins.json` requires `binary`, `binary_sha256`, `build_manifest`,
`build_manifest_sha256`, `primary_metadata`, `primary_pid`,
`primary_started_at_ms`, `source_root`, `project_id`, and `active_generation`.
Use absolute paths and SHA256 values observed independently of this capture.

Prepare `spec.json` with `version: 1`, `oracle` (a relative JSON path),
`source_files` (project-relative paths mapped to independently checked SHA256),
optional `frozen_metadata` (relative evidence paths), and exactly four `cases`.
Each case has `id`, the original `category`, `task`, `base_inputs` (JSON paths),
`available_reads` (read IDs mapped to JSON paths), and `tool_requests`
(`{name, arguments}` objects). The original OX workflow can use `get_change_set`,
`search_history`, and `derive_test_matrix` for the pinned project. Existing
CodeMap/impact/Blast recipes remain readable, but those tools are closed for
further evaluation under the current OciusX delivery scope. Discovery requires
the requested tools and `get_index_freshness`, not every optional tool in the
collector's allowlist. Mutating and unknown tool requests remain rejected before
connecting. Supply an oracle
with `judge_only_canary` and `cases[id].criteria` as in existing frozen corpora.
Freeze tasks, criteria and source evidence independently before capture.

The new destination retains the recipe before any producer query, full replies,
tools/list, freshness observations, process/build pins and selected source
hashes. Failed captures remain there for inspection; use a new destination for
another attempt. Observation waits retain the same pending request. Build,
primary process, generation or selected-source drift prevents a usable freeze.
Both arms receive the same base sources and available reads; assisted inputs
add complete tool response text with byte/hash accounting. The unchanged
`prepare`/`execute` protocol consumes the resulting four-category/eight-arm
corpus. Metadata distinguishes `current_producer` from historical inputs;
capture itself establishes neither a fresh holdout nor productivity benefit.

Use Python 3.11 or newer, an authenticated native Codex executable, and a new
private run directory.
The executor uses the existing four-task protocol, eight candidates, bounded
requested reads, and separate opaque judge packet:

```powershell
python -B eval/frozen_task_run.py prepare <recorded-corpus> <new-private-run> --delivery-mode manual_packet_v1
python -B eval/frozen_task_run.py execute <new-private-run> --codex-executable <absolute-native-codex-path>
python -B eval/frozen_task_eval.py report <new-private-run>
```

Here `manual_packet_v1` names the existing direct-packet protocol: exact packet
bytes go through stdin, so no file-opening instruction is counted as model
input. Each candidate gets a fresh persistent session; requested reads resume
that exact session ID. The judge gets a ninth fresh session. Execution inherits
the configured model and records its configuration hash. It does not retry,
repair answers, substitute models, or replace failed sessions.

The executor disables known tools and configured MCP servers, suppresses project
instructions and memory, and uses an otherwise empty working directory. It
rejects runs with unexpected or tool events. This is contamination detection,
not proof that platform context or every possible capability is absent. Raw
events, final responses, process outcomes and session IDs remain private beside
the collection. Persistent Codex sessions also remain in normal runtime storage;
the executor does not delete them. Failed runs retain their evidence and cannot
be silently restarted. The per-invocation deadline defaults to 900 seconds;
ordinary observation waits do not restart a running invocation.

Packet bytes and collector wall times remain the original metrics. Provider
usage and subprocess timings are separate execution evidence. This command can
run a fresh model comparison over the historical packets; it does not capture
current Engram producer output or demonstrate completed implementation/QA benefit.
CI checks the execution protocol with synthetic subprocesses, not paid model
runs or private project evidence. For local protocol tests, set
`ENGRAM_TEST_TMPDIR` to the recorded scratch directory before running
`python -B -m unittest discover -s eval -p 'test_frozen_task_*.py' -v`.

The [November 9 execution receipt](eg24-codex-execution-outcome-20261109.json)
records a completed automatic run: eight candidates, eight exact-session
continuations and a separate judge in about seven minutes. Both arms satisfied
19/20 criteria with no false guidance identified; assisted packet bytes were
96.1% higher and requested evidence IDs fell from eight to seven. Both arms
missed the diagnosis task's bounded-next-test criterion. A startup-warning
failure before any task turn is retained separately. These results do not
replace the earlier pilot scores or demonstrate a quality/productivity gain.

The [November 12 current-output receipt](ociusx-current-task-evaluation-20261112.json)
records four OciusX regression tasks using `get_change_set`, `search_history` and
`derive_test_matrix` from installed generation 3148. Both conditions satisfied
20/20 criteria, with no false guidance identified by the blinded judge. Assisted
runs requested eight additional reads versus ten; input increased from 166,083
to 167,603 bytes (0.9%). Capturing the current context took 167.8 seconds, including
identity, source and freshness checks as well as producer calls. The diagnosis
case uses the existing OciusX KML evidence rather than the earlier memory case,
so the totals are not a week-to-week score comparison. These are inspected
regression cases, not a fresh holdout or matched implementation/QA benefit.

Separate instruction-maintenance examples cover a real reporting `no_change`,
a present KML code/policy `conflict`, and a historical PR2077 marker API `change`.
The last uses only two original files in an owned replay fixture: the old guide
and changed code. Its proposal reproduces the accepted historical guide
byte-for-byte, preserves all other conditions and exceptions, and refuses stale
evidence. Independent review and raw Git/BLAKE3 checks support these examples.
Current OciusX marker guidance already contains that accepted update. These are
bounded proposal checks, not new model comparisons or implementation-compliance
claims. Missing replay dependencies remain explicit. The new replay fixture is
preserved because automatic approval review rejected its cleanup; exact local
receipts record the path and the prohibition on equivalent deletion retries.

## Repeated editing with explicit collection

```powershell
python -B eval/edit_collection_resource_workload.py <binary> <new-recorded-evidence-directory>
```

This wrapper imports the audit's frozen edit driver unchanged. It cycles 200
edits over 50 paths, keeps one sibling unchanged, and requests zero-cutoff
collection with vector compaction every 50 edits. It checks every current source
through lexical and path-filtered pure-vector reads after collection and restart,
including absence of the previous declaration. Source/native counts must survive.
One owned fixture and sequential child processes are cleaned up on completion.
Stops are fixed at 900 seconds, 512 MiB private memory, 512 MiB fixture size and
1 GiB cumulative process logical writes; these are protective stops, not product
memory requirements or hard OS quotas.

`collection-report.json` records individual original RPC windows separately from
collection and verification. The base driver's query windows include injected
work. Counters include background work and IPC, so they do not measure physical
SSD writes or isolate causality. Compaction retains old reader versions and need
not reduce disk usage. Compare the same wrapper/configuration and disclose run
order; a single pair does not establish a memory plateau or general performance.
The frozen base-driver hash is checked before any fixture is created.

The current audit uses the existing checkout, read-only Git objects and bounded
owned fixtures. The historical harness notes below retain older production-store
and temporary-worktree assumptions; they are not the current audit's run recipe.

## Historical implementation/retrieval harness

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
- **Phase 2 — code-gen A/B (in-session Opus agents).** Per story, two Opus 4.8
  agents (spawned in-session via the Workflow tool — Claude Code subscription, no
  API) implement the story from the SAME base-commit code, US-only input: one
  **alone** (plain Read/Grep/Glob exploration), one **+Engram** (also given the
  validated 3-arm ensemble as a dossier). An Opus judge scores each arm's
  proposal — file-recall + implementation closeness (1-5) — against the real
  merged diff. Harness: `phase2_prep.py` (index at base → ensemble → dossier +
  worktrees), `phase2_build_run.py` (bake story data into the run script — the
  workflow `args` global is unreliable with `scriptPath`), `phase2_workflow.js`
  (the A/B + judge). Agents read the worktree/dossier/diff (no writes, no git) so
  there are no permission stalls.

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

## Phase-2 results (2026-06-13, 5-story Opus A/B)

Stories: 1933 (invoice filters), 1908 (upload map-marker icons), 1967 (resources
in multitenant — a BUG), 1937 (camera icon), 1974 (geospatial export).

| | alone (no Engram) | +Engram dossier |
|---|---|---|
| mean impl_score (1-5) | **3.2** | 2.6 |
| mean file_recall | **0.46** | 0.33 |
| per-story wins | 3 | 2 |

On average the dossier was **slightly net-negative**, but the aggregate hides the
real result — **Engram's value is conditional on story type**, and one story
(1967) dominates the mean:

- **Companion-completeness / feature stories → Engram WINS.** PR 1908 (upload
  icons): alone localized only 3 of 7 languages and added a spurious
  `ConfigSettings.vb`; Engram's **git co-change signal surfaced the full
  7-language `.resx` family** ("usually changed together") → recall 0.56 → **1.00**,
  impl 3 → 4. The forgotten-companion catch is exactly Engram's promise. PR 1933
  also a modest Engram win (recall 0.08 → 0.17, impl 3 → 4).
- **Misdiagnosable bug-fixes → Engram can HURT.** PR 1967 (a tenant-access bug):
  the no-Engram agent *traced* the real read-side filter near-verbatim (recall
  **1.00**, impl **5**); the Engram agent **anchored on the ranked dossier and
  diagnosed a plausible-but-wrong save-side file** not in the real PR (recall
  0.14, impl 2). A "relevant files" list steers root-cause work toward
  pattern-matching and away from causal tracing.

**Anchoring is the central risk.** The first 1933 run had the Engram agent
declare "nothing else needs touching" and ignore a file it *had* listed at rank
22. Reframing the dossier — flat, layer-grouped, "non-exhaustive starting map,
implement EVERY layer, don't stop early" instead of a numbered ranking — lifted
its impl quality (2 → 4 on 1933) but did not fully eliminate the effect.

**Product implication / recommended use:** hand Engram's dossier to an agent as
an **advisory complement** that augments (never replaces) exploration; lead with
the **co-change/companion** signal (its strongest, most differentiated value).
For feature/CRUD work, surface the companion set aggressively. For bug-fixes,
present Engram output as orientation only and require the agent to trace
causality — do not let a ranked list short-circuit root-cause analysis.

Caveat: n=5, real model variance; this is a directional pilot, not a powered
study. The mechanisms above are diagnosed per-story (not aggregate noise), which
is what makes them actionable.

## Data-quality fix — Engram flipped from hurting to helping (2026-06-13)

The Phase-2 v1 result (Engram HURT, impl 2.6 vs 3.2) was traced to Engram's
*supplied data*, not the model: the dossier was ~93-100% noise — real files were
single-signal (co-change/concept) and **buried by a signal-COUNT ranking + a
global top-40 cap**, so the agent anchored on noise. Generic fixes (no OciusX
hardcoding): co-change-first tiered ranking (history/co-change = golden, never
cut), co-change *confirmation* (promote files that historically co-shipped with a
high-confidence seed), framework-generic family expansion (.NET code-behind +
full .resx set), and a vendor/min.js filter in `get_concept_footprint` (the one
real Engram-side change).

**A/B trajectory (engram impl_score, 5-story Opus, vs alone ~2.6-3.2):**

| config | dossier recall | engram impl |
|---|---|---|
| v1 (signal-count rank, cap 40) | ~0.10 | 2.6 — *Engram hurt* |
| **co-change-first + family + denoise (45cf172)** | 0.70 | **3.2 — Engram helps (+0.6 to +1.2 swing)** |
| + temporal-coupling expansion | 0.75 | 2.4 — regressed (flood) |
| tighter cap-8/top-6 | 0.55 | — (recall regressed; real files cut) |

**Lesson: dossier *recall* is the wrong optimization target — precision /
signal-to-noise drives the agent's implementation quality.** Both "more recall"
(temporal flood) and "less" (aggressive cap) regressed; the co-change-confirmed,
family-complete, denoised set at cap-18 is the sweet spot. `45cf172` is the
proven recipe.

**+ Completeness guidance (best result):** adding a `.NET` completeness checklist
to the dossier ("edit code-behind+markup, all .resx languages, the SQL, the
bundle") lifted engram recall 0.38→0.52 and gave the cleanest A/B yet — engram
**impl 3.2 vs alone 2.8, 0 losses** (2 wins, 3 ties; previously alone won 2).
Ties are at the ceiling (1974 both 5/5) or the bug (1967 both fail).

**Landed in Engram:** `get_change_set` — a native generic tool that runs the
recipe server-side (concept + git co-change, co-change/history-first ranking,
.NET family expansion via the indexed file list, vendor denoise, completeness
checklist). Plus two real Engram fixes from this eval: the apostrophe NL-query
crash and the `get_concept_footprint` vendor filter.

Remaining frontier (NOT solved by more retrieval): the agent's execution ceiling
(~3.2 even with good files) and bug root-cause causal tracing (PR 1967). Next:
larger-N A/B for statistical confidence; a symptom→cause trace arm for bugs.

## Efficiency note (harness)
Each dossier experiment used to re-index all 5 base-commit projects (~20 min).
Fixed: indexes are KEPT (`index_map.json`); `phase2_prep.py --reuse` skips the
re-index (~20-70s); `p2_rerender.py` re-renders from the dumped `prov` instantly
(no Engram). Same data, ~95% less wall-clock per iteration.
