# Engram Improvement Day — 2026-07-04

42 loop iterations, 36 commits, every change live-verified against OciusX.

## The headline number

**Engram's planning brief lifts agent file-set recall 31% → 84% aggregate** —
measured leak-free (plain-file snapshot trees, git forbidden), same-day, at
constant model (Sonnet), across six story shapes. The lift lands exactly
where stories underspecify team knowledge:

| PR (story shape) | Control | + Engram brief | Lift |
|---|---|---|---|
| 1893 — feature on an unfamiliar surface | 0% | 100% | co-change finds what exploration misses |
| 1890 — new API domain | 8% | 54% | 7× |
| 1933 — UI change w/ design divergence | 17% | 83% | 5× |
| 1938 — cross-cutting JS/bundles | 27% | 80% | 3× |
| 1905 — debugging | 29% | 93% | 3.2× |
| 1904 — fully-specified story | 100% | 100% | tie at ceiling |
| **Aggregate** | **31%** (24/77) | **84%** (65/77) | — |

The better-specified the ticket, the smaller the lift; the more the change
depends on team-convention knowledge, the bigger (up to 7×). Engram's brief
supplies exactly what the story leaves out.

Sonnet + brief also **beat the frontier model running bare** on PR1905
(93%/100% vs 86%/80%): the brief makes the cheaper model the best performer.

## What the brief contains (all built + measured today)

- **Scaffold with concrete paths**: story entity grounded in the ranked file
  set (`MarkerInspection`, not `Inspection`), full conventional cohort spelled
  out (Controller/Service/interface/QueryParams/DTOs).
- **Wiring checklist**: permission catalogs, source pages, DTO projection
  variants — the misses a live A/B exposed.
- **Configurability prior**: "this team ships behaviour changes as settings" —
  took PR1933 from 1 file to the real 10-file setting-gated design (10×).
- **Signal legend + REQUIRED-decision framing**: skipping evidence-backed
  candidates now demands stated justification; every checklist item is an
  explicit decision.
- **Approved exemplars**: top merged-PR cards inline, `merged_before` for
  leak-free historical replay.

## Retrieval (dossier recall, no agent in the loop)

57.6% → **86.4%** across the 9-PR sweep. Remaining misses are drive-by edits
and design divergence — agent-judgment territory served by the priors above.

## Live corpora (all with refresh pipelines)

| Corpus | Size | Serves |
|---|---|---|
| Code graph | 2,246 files / 54K nodes / 113K edges | all funnels; reindex 20s |
| Settings brain | 789 cataloged, 40 LLM wikis | `list/get/describe_setting`, `derive_test_matrix` |
| Merged work | 1,678 PRs, gen-0 (reindex-proof) | exemplars, `find_merged_work`, `merged_before` |
| Quality gates | 449 findings + 258 generic rules | `pre_push_audit` |
| Team knowledge | 263 wiki/docs sections | `search_memory`, `ask_codebase` |
| Gold-Standard KB | `generated/settings/` 40 wikis | regenerable views over Engram |

## Perf / correctness hardening (spot list)

- Every full structural-edge scan eliminated (`edges_touching`, O(degree)):
  `check_edit_safety` 7.5s → 0.04s, impact/blast/dossier all sub-0.1s warm.
- Call-site `@L` anchors in `find_symbol_references`; ambiguity headers.
- Settings-store extraction (VB + C#): `ConfigSettings.X.Y` reads are edges.
- Node-scan cap 50K → 200K (was silently truncating post-growth).
- PR docs at generation 0 — reindexes no longer silently kill filters.
- Sub-second QA test matrices (settings × roles × shared state per change).

## Measurement rules (standing)

1. Agents run ONLY on `engram_p2_wt` snapshot trees (no `.git`), git
   commands forbidden in prompts.
2. A/B pairs are same-model, same-day. Cross-session model changes confound.
3. Leak signatures (perfect recall, quoted commit hashes) fail the run.

Full ledger: memory files `engram-utilization-wall.md`,
`settings-intelligence-design.md`, `merged-work-corpus.md`,
`ociusx-quality-gate-sources.md`, `ociusx-gold-standard-kb.md`.
