# ChangeVerifier v1 — pre-registered probe gate (2026-09-05)

Owner adopted ChangeVerifier v1; the discipline (which let Gate 1 fail
cleanly) is to fix a falsifiable gate BEFORE writing the analyzer, so it
cannot be moved to fit the result.

## The probe (smallest falsifiable slice)

One defect family first: **null-safety** — a changed hunk that dereferences a
value which could be `Nothing`/`null` without a guard. NOT the full
AST/dataflow engine; the minimal check that can be measured against real
labeled defects.

## Ground truth (measurable — confirmed present)

`eval/data/qg_coderabbit.json`: 334 real CodeRabbit findings with
`{file, line, message, severity, source_pr}`. The "32" first written here was a
LOOSE null/nothing keyword estimate; the TIGHT, reproducible count is **7
findings across 5 PRs** — see round-7 P1-6 below. Time-separated split by
`source_pr` (older PRs = calibration, newest ~40% = holdout). The OciusX repo is
local, so each PR's changed hunks are recoverable by `git diff`.

**Round-7 P1-6 / round-8 P1-5 — sealed ground truth (CORRECTED, honest).** The
earlier wording here was wrong on two counts the round-8 audit caught:

1. **The DATA is NOT committed** — and cannot be, by policy. Only the CLASSIFIER
   `eval/nullsafety_classifier.py` is tracked. Both `eval/data/nullsafety_subset.json`
   (the 7-finding labelled subset) and its source `eval/data/qg_coderabbit.json`
   are **git-excluded** (`.gitignore`), because they contain real OciusX
   findings/paths — the [[no-customer-strings-in-source]] mandate. A fresh clone
   therefore CANNOT reproduce the subset without the local corpus present; the
   seal is a reproducibility check for whoever HAS the corpus, not a
   clone-portable artifact. The prior "the subset is committed" claim was false.
2. **The digest is a CANONICAL-PAYLOAD sha256, not the file's sha256.** The seal
   `60792b87355585a6a8d7272f907eb65cff9bb3ea11836d1b6bef6d851be71db4` is the
   classifier's hash of the SERIALIZED findings payload (before the trailing
   newline); the on-disk file's sha256 differs (e.g. `96B4AE…`). It is named
   here as a canonical-payload digest so the two are not conflated.

The count is 7 findings across 5 PRs (1900/1911/1930/1932/1940). Re-running the
classifier over an unchanged local corpus reproduces the same subset + payload
digest. This does NOT make the gate large enough — 7 heterogeneous cross-language
findings is a small, noisy holdout — so the analyzer stays owner-gated.

## Pre-registered gate (fixed now; not movable)

Run the null-safety check over the holdout PRs' changed hunks:

- **Mechanism recall ≥ 40%** of the holdout null-safety findings flagged at
  the correct file (± a few lines).
- **Precision ≥ 60%** — of what it flags, that fraction are real
  null-safety issues (judged against the finding set + manual spot-check).
- **≤ 5 findings per diff** — the anti-flood bound that held in Gate 1.
- **No clean verdict** emitted when the diff could not be parsed.

If any gate fails: **STOP** and report, exactly as Gate 1's 6/15 was
reported — no fix-and-rerun to manufacture a pass.

## Honest risk

My own prior verify experiment (verify_implementation_plan) failed Gate 1
6/15 because the modal defect was wrong-mechanism, not a pattern a simple
checker catches. Null-safety is more local and pattern-detectable than
wrong-mechanism, so this probe has a better shot — but the base rate of
auditor-directed programs on this codebase is 0-for-2 on moving the metric,
and N=7 (the tight, reproducible count — NOT the loose 32 estimate this doc
originally carried) is small. This is a probe, not a commitment; the gate decides.

## Sequence

1. Land the round-5 honesty batch (doc 20) — the confirmed-defect closure.
2. Build the null-safety RED (holdout harness) → minimal analyzer → measure.
3. Gate passes → expand to permission family. Gate fails → stop, report,
   and the honesty batch stands as the round-5 deliverable.

## Feasibility check — CORRECTED 2026-09-05 (my round-5 numbers were wrong)

My round-5 feasibility analysis claimed the gate was "not measurable." Re-run
against the raw data, **both of its load-bearing numbers were wrong**, and the
error was mine (incomplete queries), not the data's:

- **Null-safety count.** A tight classifier over `eval/data/qg_coderabbit.json`
  yields **7** genuine null-safety findings across 5 PRs (canvas-context guard,
  `Nothing`-body dereference, DBNull handling, instanceof-before-dataset,
  stale-async-callback guard, feature-gated-object guards) — not the "~4" I
  reported; a looser classifier reaches the low teens. Still cross-language
  (TS + VB) and still small, but not as thin as claimed.
- **Diff recoverability.** I claimed "only 6 of 16 implicated PRs have
  base/merge commits recoverable." That was FALSE — an artifact of querying the
  `merged_before` replay worktree instead of origin history. Every implicated
  PR (1900, 1911, 1930, 1932, 1940) has a `Merged PR N:` merge commit in the
  OciusX history (`git -C <OciusX> log --all --grep="Merged PR <n>"`), and the
  full history carries **1651** such commits. Each finding's diff is recoverable
  via `git diff <merge>^ <merge>`. The auditor was right.

**Corrected conclusion.** The pre-registered gate IS runnable: the labeled
findings exist and their diffs are extractable. The honest residual caveat is
statistical, not data-availability — N≈7–12 cross-language findings makes a
mechanism-recall threshold noisy (a 40%-of-7 bar is 3 flags), so a pass must be
read with that N in mind, not as proof of a general capability.

**What does NOT follow:** I am not unilaterally launching the ChangeVerifier
analyzer build off this correction. Building it is a program-level scope
decision the owner has explicitly gated (doc 16: findings get triaged, not
auto-adopted). The correction moves the probe from "blocked on ground truth" to
"queued and actually runnable," to be run only when the owner greenlights that
scope — with the small-N caveat stated up front and no fix-and-rerun to
manufacture a pass.
