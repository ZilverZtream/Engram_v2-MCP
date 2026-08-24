# UI Conformance — design spec

**Date:** 2026-08-17
**Status:** Draft for review (brainstormed; not yet planned)
**Feature owner tool:** `get_ui_conformance(region)` (new Engram MCP tool) + an offline **UI Family Catalog**
**Production home / proof rig:** OciusX `/story` orchestrator (`.claude/skills/story/SKILL.md`) and its dry-run replay protocol

---

## 1. Problem

Agents building UI in an existing codebase fail in a specific, repeatable way. It is **not** primarily a
knowledge gap or a pixel-perception gap — it is a **completeness-of-conformance** gap:

> Asked to add (e.g.) a new map info-window, the agent copies the *one* dimension it was told to copy
> (structure) and **regenerates every unmentioned dimension from its own priors** — color scheme, data
> presentation, line-height/padding, naming. Each human correction adds one constraint; satisfying the
> newest constraint does not make the agent re-verify the earlier ones, and it sometimes silently
> regresses them. The human ends up serving as the app's conformance spec, delivered one line per turn.

**Mechanism:** the agent minimizes distance to the *latest instruction*, not to the *exemplar as a whole*.
There is no persistent, explicit set of "things that must all be true at once."

**Why this is worth solving now (eval evidence):** the OciusX eval already names the remaining frontier as
the **execution ceiling** (~3.2/5 impl-score; per-PR baselines ~86-89) — *"NOT solved by more retrieval."*
Retrieval is done (~60% page-recall, ~13× baseline). UI conformance targets execution quality directly.

## 2. Goals / non-goals

**Goals**
- Given a **region of code**, derive and deliver a complete, multi-axis **conformance contract** for the
  UI family that region belongs to — the exemplar to copy plus the invariants that define "matching."
- Deliver it **proactively** (before the agent writes) and **verifiably** (an all-at-once conformance
  check after), so ten correction turns collapse to zero or one.
- On axes where the codebase has a real convention: conform to it. On chaotic axes: **prescribe a blessed
  canonical and drive consolidation** (user decision, 2026-08-17).
- Prove the value cheaply *before* the expensive build (kill-switch milestone, §9).

**Non-goals (YAGNI, deferred)**
- **Vision-model judging** of screenshots ("looks cheap"). Geometry catches the named defects; defer.
- **Non-WebForms extractors** (React/Vue/Tailwind). Schema is stack-neutral; only the WebForms extractor
  is built now, because ~all real agent work is in OciusX. Others are a pluggable seam, unbuilt.
- Auto-*applying* consolidation refactors across the existing tree. The tool proposes canonicals and makes
  *new* code conform; bulk-rewriting existing files is out of scope.

## 3. Core concepts

- **UI Family** — a cluster of sibling UI instances that are "the same kind of thing here" (the N
  info-windows, the form rows, the icon+label buttons). Legacy copy-paste heritage makes families
  **locally consistent even when the app is globally inconsistent** — which is exactly why family-scoped
  contracts beat a global token census (census evidence: structure consistent, tokens chaotic).
- **Conformance Contract** — the per-axis spec of what makes a new instance a conformant member. Each axis
  is **typed by the family's actual consistency**:
  - `consistent` → **conform** (hard rule). e.g. OciusX form row =
    `form-horizontal → form-group → control-label → form-control` (641/309/1169 uses).
  - `chaotic` → **prescribe** a blessed canonical + mark as a consolidation target. e.g. icon↔label gap
    (hand-typed `&nbsp;` 545×, three icon libs, ad-hoc margins) → adopt one `.icon-label` utility.
- **UI Family Catalog** — the offline, pre-computed set of families + contracts + exemplars + **matching
  descriptors**, stored as queryable Engram memory. Built at index/refresh time; matched at query time.

## 4. Architecture (four layers)

### Layer 0 — stack-neutral contract schema
One normalized object every extractor emits. Sketch:

```
ConformanceContract {
  family_id, family_name, purpose,
  exemplar: { path, node_id, source },        // the canonical instance to copy
  axes: [
    { axis,                                    // structure | style.color | style.spacing | style.type
      //                                        | data_presentation | naming | wiring | localization
      consistency: consistent | chaotic,
      policy: conform | prescribe,
      canonical,                               // the value/template/pattern to match (or the blessed one)
      alternatives,                            // observed variants (for chaotic axes)
      tolerance,                               // numeric slack for geometry/spacing axes
      evidence: [ {path, count, weight} ],     // provenance: frequency + hub/IDF weight
      blessed_by }                             // set only when policy=prescribe (human sign-off)
  ],
  base_commit                                  // the commit the contract was derived at (leak-safety)
}
```
Source-of-truth principle (MiniLang lesson: *the corpus, not the style guide*): axis values are what the
family **actually does**, frequency- and IDF-weighted. One-off values are orphans/findings, not tokens.

### Layer 1 — pluggable stack extractors (WebForms first)
Parse the region's markup + CSS and populate the schema. WebForms extractor mines:
- structure/markup skeletons and CSS class co-occurrence (de-facto components),
- style-value histograms (spacing/color/type) with **normalization**: class-lists compared as **sets**
  (`btn btn-primary btn-md` == `btn btn-md btn-primary`); colors canonicalized (`#fff`==`#ffffff`==`white`;
  `rgba(x,1)`→hex); spacing snapped to the derived scale,
- naming patterns over identifiers (handler/class/id templates),
- data-presentation patterns (`<dl>` vs `<table>` field layout),
- wiring (reuses `get_page_context` / `get_ui_blueprint` / `trace_ui_action`).

Reuses existing Engram machinery: `get_concept_footprint`, `find_implementation_pattern`,
`find_similar_changes`, `analyze_file_coding_style`, the code graph, and the census/histogram logic.

### Layer 2 — `get_ui_conformance(region)` (the oracle + the check)
One primitive, both directions:
- **Region** = a file/dir/glob (an exemplar to learn from) **or** a target location + intent (proactive).
- **Pull (proactive):** match region → family/families in the Catalog → return the assembled contract
  (exemplar + typed axes). Consumed *before writing*.
- **Check (verify):** diff new code against the same contract (normalized) → return **every** deviation at
  once, `✓/✗` per axis. Rides the existing `pre_push_audit` / `pre_commit_review` gate pattern.

### Layer 3 — render + measure backstop (geometry, not vision)
Source-level conformance can't see post-cascade values (computed line-height, actual pixel gaps). A DOM
geometry probe (`getBoundingClientRect` + computed styles) checks deterministic invariants against the
family's **baseline geometry** (golden measurements of existing members): icon↔label gap ≥ baseline-min,
row siblings share baseline within tolerance, margins ∈ scale, stacked left-edges align, no overlap.
**Constraint (from `/story`):** the tester role is click-driven and **barred from `browser_evaluate` by
design** — so the geometry probe is a *separate* step (orchestrator-run or a dedicated `ui-geometry` probe
against the running site on `localhost:52065`), never shoehorned into the tester. Geometry is arithmetic on
boxes; the vision layer stays deferred.

## 5. Chaos policy — prescribe & consolidate

On a chaotic axis "match the existing design" is *literally ambiguous* (four contradictory icon-gap
patterns; ~230 colors). Policy (user decision):
- Engram **proposes** the canonical: the statistical mode **plus** a consolidation (e.g. one `.icon-label`
  utility; the recovered 7-color brand set `#083a69/#e6883f/#623588/#70a834/#fead05/#cc3f0c/#337ab7`; the
  top-8 spacing values as the snap scale).
- A human **blesses** it **once** per axis; blessed canonicals are stored like a repo rule
  (`add_repo_rule`) and NEW code conforms to the blessed answer, not the mess.
- **Precedence guard:** blessed canonicals sit at the review-memory tier — **below** Marcus's authoritative
  instruction files and the live tree. A proposed canonical that conflicts with an authoritative file is a
  *proposal to Marcus*, never a silent rule.

## 6. Proactive detection (the hard part, scoped)

"First attempt conforms" only works if the system knows the family **before a line is written**; a wrong
guess actively misleads. Mitigations:
- The Catalog is **pre-computed** with rich **matching descriptors** (purpose text, trigger terms, file/dir
  globs, structural fingerprint), so matching is *retrieval over ~dozens of labeled families*, not
  open-ended inference. Debuggable, not magic.
- Two match routes, combined: **task→family** (fold a "UI conformance" section into `get_change_set`'s
  dossier for UI-touching stories) and **location→family** (the region's globs/fingerprint).
- The Layer-2 **check is the verifier behind the promise** — it catches misses *and* its violations are the
  signal that sharpens the detector over time. Proactive without the verifier is fragile; with it, real.

## 7. Temporal-leakage safety (mandatory for replay)

Contracts are derived at **`base_commit`**, excluding any family instance the target PR introduced (mirror
of the eval's `merged_before` cutoff and `/story`'s replay rule that strips target-PR review-memory
entries). Otherwise the contract would "conform" the agent to the very answer being scored. `get_ui_conformance`
takes an optional `as_of_commit` and honors the existing base-commit indexing path.

## 8. `/story` integration (three existing seams)

1. **Proactive injection (Phase 2/4):** emit the contract as a new **injected artifact**, pasted into
   UI-touching `<domain>-planner`/`-developer` spawns alongside the review memory, scoped to the domain's UI
   territory (e.g. `installation-mapmarkers`: `pages/public/map/**`, `ts/map/**`, markers). Add
   `get_ui_conformance` to the planner/developer/tester Engram tool block.
2. **Verify (Phase 5):** a conformance **check** over the branch diff — either a new `ui-conformance-gate`
   in the standing-gate set or folded into the developer self-gate + tester verification, via
   `pre_push_audit`. Reports all axis deviations at once.
3. **Geometry (Phase 6):** a dedicated geometry step against `localhost:52065` (NOT the click-driven
   tester), comparing measured boxes to the family baseline.
4. **Proof (dry-run replay):** read the impl-score / mechanism-match delta off the existing scorecards.

## 9. Validation plan / milestones (kill-switch first)

- **M0 — cheap A/B, before building the Catalog.** Hand-author (or subagent-generate, exactly like the
  census) a conformance contract for the **icon-label family** and inject it into a `/story` dry-run replay
  on the icon stories (**1908** "upload map-marker icons", **1937** "camera icon" — both
  `installation-mapmarkers`, the worst-conformance axis per census). If impl-score does not move, **stop —
  we ignore the idea** (per the user's condition). If it moves, proceed.
- **M1 — WebForms extractor + schema + Catalog build** (offline; families + contracts + descriptors).
- **M2 — `get_ui_conformance(region)`** pull + check, with normalization; fold a UI-conformance section
  into `get_change_set`.
- **M3 — `/story` wiring** (injection + gate).
- **M4 — Layer 3 geometry probe + baseline store.**
- **M5 — blessed-canonical flow** (`add_repo_rule` integration; the icon-label utility as first canonical).

## 10. Honest limits / risks

- **Detection precision** is the crux (§6); it will not be 100% and needs tuning. The verifier backstops it.
- **Prescriptive canonicals need a one-time human blessing** — not automatic, but not per-use either.
- **Computed/rendered values need the render backstop** (§4 L3) and the `browser_evaluate` constraint.
- **Anchoring risk** (eval lesson: a ranked list short-circuited bug root-cause). UI conformance is
  *additive/stylistic, not causal*, so it is a **safer** dossier input than the file-list — but it must be
  scoped to **UI-touching** stories and framed advisory, never injected into bug/logic-only work.
- **Global inconsistency** is handled by census-with-dominance (surface the modal pattern; expose the
  spread as a finding), but the tool's honest job includes telling the user their own UI is inconsistent.

## 11. Open questions

- Family granularity: auto-cluster only, or allow a curated family registry for the important ones?
- Should the Layer-2 check be a standing `/story` gate or a developer/tester self-gate step (or both)?
- Baseline geometry store: per-family golden measurements captured when — at Catalog build, or lazily on
  first render?
- Does `installation-mapmarkers` (which has Marcus's authoritative `map-marker.instructions.md`) create
  canonical-vs-authoritative conflicts we should resolve up front for the M0 icon stories?
