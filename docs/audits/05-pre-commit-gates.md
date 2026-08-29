# Row 3 — Pre-commit defect prevention: `pre_commit_review`, `pre_push_audit`

Audit date 2026-08-28. Code at `ask-codebase-brain`. Live evidence from
OciusX project `5a35e8e0-d37a-41b3-a250-a26957e7aedb`, branch DMO/930 HEAD
diff. Research pass by an isolated Sonnet agent; every citation below was
re-verified by grep before it was written down.

## 1. Verdict

`pre_commit_review` is the tool most likely to be trusted blindly ("fix or
explicitly justify every finding"), and it has the same honesty flaw the
blast-radius substrate had before round 5: the verdict is computed from the
findings that SURVIVED, with no record of which gates actually ran. The
auditor's claim is correct and understated:

- the orchestrator drops gate errors AND panics into `tracing::warn!` in
  both execution buckets, and `Verdict::from_findings` cannot see them;
- more importantly, **no gate ever returns `Err` today** — each of the 17
  gates swallows its own provider failures into `Ok(vec![])` or a silently
  degraded scan, so the orchestrator's error arms are dead code and
  "search runtime unavailable" renders as "clean";
- `gates_run` counts DISPATCH, not outcome; the JSON payload has no
  per-gate status at all;
- every cap inside a gate (top-k, neighbour limits, per-file finding caps)
  is unreported, except in the blast-radius gate (fixed in round 5).

`pre_push_audit` is honest when it has nothing (it says so) but on the
reference project it HAS nothing: the `quality_gate` namespace is empty, so
the mandated pre-push step is a no-op on OciusX today.

## 2. Verified defects (`services/pre_commit_review_service.rs` = `svc`, `…/gates.rs` = `gates`)

| # | Sev | Defect | Evidence |
|---|---|---|---|
| D1 | P0 | Gate error/panic ⇒ warn only, verdict proceeds | sync bucket `svc:2063-2071`: `Ok(Err(e)) => { tracing::warn!(.. "gate failed") }`, `Err(e) => { tracing::warn!(.. "gate panicked") }`; async bucket `svc:2081-2087` same; `Verdict::from_findings` `svc:213-225` takes only `&[ReviewFinding]`; clean-bill string `svc:1810` `"_No findings — diff passed all gates cleanly…"` keyed on `findings.len() == 0` |
| D2 | P0 | Every gate swallows its own provider failures into "clean" (so D1's arms never fire) | `antipattern`: `ensure_project_runtime` `Err(_) => return Ok(self.destructive_only(ctx))` `gates:1328` (semantic scan silently replaced by regex-only), `hits.unwrap_or_default()` `gates:1399-1400`; `product_intent`: runtime missing ⇒ `Ok(Vec::new())` `gates:2656-2659`, search `.unwrap_or_default()` `gates:2689`; `co_added_family`: `gates:2863-2866`, `gates:2899`; `sync_contract`: `query_nodes(..300).unwrap_or_default()` `gates:2426` (graph failure = "no contracts exist"); `temporal`/`test_coverage`/`state`: `neighbors`/`find_incoming_edges … .unwrap_or_default()`; `blast_radius`: per-file `Err(_) => continue` `gates:398`; `style`: unreadable file ⇒ `continue` `gates:554`; `guard_parity`/`complexity_budget`/`added_conventions`: `std::fs::read(..).unwrap_or_default()` `gates:110`, `gates:3239`, `gates:3500` (unreadable file = empty file = nothing to flag) |
| D3 | P1 | `gates_run` counts dispatch, not success; JSON has no gate status | `svc:2046` `gates_run += 1` before the handle is awaited; `ReviewSummary` `svc:1880-1897` fields are counts + `gates_run` + `elapsed_ms` only; grep `gate_error|gate_fail|skipped|degraded|GateResult` in both files → 0 hits |
| D4 | P1 | In-gate caps unreported | `temporal` top-20 neighbours; `state` 50 readers / 50 writers; `test_coverage` top-30; `sync_contract` 300 contracts + 10 tail nodes; `unwired` `MAX_DEFS=80` `gates:2199`, `.take(25)` `gates:2306`; `antipattern` `top_k 5`; `product_intent` `top_k 5`/`take(5)`/`truncate(3)`; `co_added_family` `top_k 20`/`take(4)`/`findings ≥ 3` early break; per-file `take(3..5)` in `complexity_budget`/`added_conventions`. Only `blast_radius` reports (`FILE_CAP=20` + coverage, round 5) |
| D5 | P1 | `unwired` fails the OTHER way | `gates:2314` `query_nodes(..).unwrap_or_default()` — a graph error means "no known caller" ⇒ a **false positive** finding instead of a silent pass |
| D6 | P2 | No test forces a gate failure | `tests/pre_commit_review_gate_tests.rs` (24 tests): none makes a gate return `Err` or panic and asserts verdict/render; `"passed all gates cleanly"` appears only in production (`svc:1810`) |
| D7 | P1 | `pre_push_audit` is inactive on the reference project and never tallies | single merged namespace `QG_NAMESPACE = "quality_gate"` (`handlers/quality_gate_tools.rs:21`), no per-source accounting; non-empty path `:224-263` never prints how many rules were checked; empty path `:220` prints the honest "no quality-gate rules matched… (If you haven't run ingest_quality_gates…)". Live OciusX: namespace EMPTY (consistent with `repo_rules=0` since the 2026-08-02 reindex) |

What is already right (keep): `pre_push_audit` surfaces provider errors as
hard errors (`quality_gate_tools.rs:218` `map_err(.. internal_error)`) —
live, with Ollama down, it errored loudly instead of rendering clean;
finding IDs stable across runs (`svc` "CI can track this finding");
blast-radius gate coverage reporting; the 17-gate roster itself is broad.

## 3. Live OciusX evidence (2026-08-28, deployed binary = round-5 build)

`pre_commit_review {diff:"head", output_json:true}` on DMO/930 HEAD:

```
verdict: yellow
summary: total_findings 24 (0 critical · 11 warning · 13 info · 0 style)
         files_analysed 21 | gates_run 17 | elapsed_ms 22954   (warm re-run: 5.7 s wall)
findings by gate: temporal 10, test_coverage 7, blast_radius 2, complexity_budget 2,
                  antipattern 1, audit 1, corroboration 1 (meta)
JSON: no key anywhere describes a gate failure, skip, or degradation
```

Markdown header on the deployed binary still reads `**Gates run**: 17/10`
(fixed in source today, row 0).

`pre_push_audit` with a VB snippet, `file_path="Site/App_Code/test.vb"`:

```
attempt 1 (Ollama down): TOOL ERROR -32603 'Ollama request error: … /api/embed'   ← honest
attempt 2 (0.38 s): "Pre-push audit: no quality-gate rules matched this change.
                     (If you haven't run ingest_quality_gates for this project, there are no rules to check yet.)"
```

The generated workflow mandates this call before every push; on OciusX it
checks zero rules.

## 4. Redesign

### A. Defects to fix now (each gets a failing test first)

| Fix | Mechanism | Closes |
|---|---|---|
| A1 | `GateOutcome { name, status: Passed \| Findings(n) \| Failed(reason) \| Panicked(reason) \| Skipped(reason) \| Degraded(reason), caps_hit: Vec<CapHit>, elapsed_ms }` collected by the orchestrator for BOTH buckets; `ReviewSummary` gains `gates_executed`, `gates_failed`, `gates_degraded`, `gates_skipped` and a `gate_status: Vec<GateOutcome>`; markdown gets a `## Gate status` block and the header shows `17/17 ran · 2 degraded` | D1, D3 |
| A2 | `Verdict::from(findings, outcomes)`: any REQUIRED gate (immune, blast_radius, guard_parity, secret_leakage, antipattern, state) that is Failed/Panicked/Degraded ⇒ verdict cannot be Green (Yellow, labelled INCOMPLETE); the clean-bill line becomes `"N gates ran clean; K did not run: <names + reasons>"` whenever K > 0 | D1, D2 |
| A3 | Gates return `GateRun { findings, degraded: Option<String>, caps_hit }` instead of a bare `Vec`. Each swallow point in D2 maps to an outcome: `antipattern` runtime-missing ⇒ Degraded("semantic search unavailable; regex-only"), `product_intent`/`co_added_family` ⇒ Skipped("no search runtime"), graph errors in `sync_contract`/`temporal`/`test_coverage`/`state` ⇒ Failed, unreadable file in `style`/`guard_parity`/`complexity_budget`/`added_conventions` ⇒ Degraded(file), `blast_radius` per-file Err ⇒ Degraded(file) | D2 |
| A4 | Every numeric cap in D4 records a `CapHit { what, cap, observed }` when hit; rendered in `## Gate status` with the blast vocabulary (`≥`, "SAMPLED") | D4 |
| A5 | `unwired`: graph error ⇒ Failed outcome, never a finding | D5 |
| A6 | `pre_push_audit`: always print `N rules checked (sources: copilot 12, coderabbit 30, …)`; when N = 0 print `PRE-PUSH AUDIT INACTIVE for this project — 0 rules ingested` and have `generate_agent_integration` surface that state. Data task in the DoD: ingest OciusX rule sources (`.github/copilot-instructions.md` via `ingest_quality_gates`, CodeRabbit history via `distill_quality_gates`) so the reference project has rules | D7 |
| A7 | Tests: orchestrator with a fake gate that returns `Err` and one that panics ⇒ verdict ≠ Green, status rendered, JSON carries it; one degraded-path test per gate in D2; `unwired` graph-error test; `pre_push_audit` tally test | D6 |

### B. Redesign that needs evidence first

- **Gate calibration**: the 2026-07-10 friction-day data said the gates were
  orthogonal to what reviewers catch (mechanism pre-catch 0.4 %). Deciding
  which gates are REQUIRED for A2 (beyond the safety set) and which are
  advisory should use the CodeRabbit-history replay, not opinion.
- **Structured findings for agents**: today an agent has to parse markdown
  to act; a per-finding `action` (fix / justify / ignore-with-reason) with
  IDs already stable is the row-9 "you forgot the other side" contract.
  Design with row 1.

## 5. Acceptance gate

| Gate | Measure | Target |
|---|---|---|
| G1 | A forced gate `Err` or panic can never render Green or the clean-bill line | mutation test |
| G2 | Every swallow point in D2 has an outcome | grep: `unwrap_or_default()` / `.ok()` / `Err(_) => continue` inside `run`/`run_async` in `gates.rs` without a `GateRun.degraded`/`caps_hit` record = 0 |
| G3 | JSON carries per-gate status | schema test on `render_json` |
| G4 | `pre_push_audit` prints rule count + sources; OciusX has ≥ 1 ingested source and the workflow step returns rules | live re-run |
| G5 | Latency not worse | cold 23.0 s / warm 5.7 s today on the 21-file diff; re-measure after |
| G6 | Full `--tests --lib` sweep green; the 24 existing gate tests untouched or re-expected with reasons | sweep |

## 6. Disposition table (implementation in slices — slice 1 landed 2026-08-28)

| Item | Disposition |
|---|---|
| A1 | **fixed (slice 1)** — `GateOutcome { name, status: Passed \| Findings(n) \| Failed \| Panicked \| Skipped, elapsed_ms }` for every gate in both buckets (async gates under `catch_unwind`; `skip_gates` recorded as Skipped); `ReviewSummary` gains `gates_failed` / `gates_panicked` / `gates_skipped`; JSON `gate_status[]`; markdown header "(K did not run)" + a "⚠ Gates that did not run — evidence is INCOMPLETE" section |
| A2 | **fixed (slice 1)** — `Verdict::with_outcomes`: any Failed/Panicked gate keeps a Green diff at Yellow; the clean-bill line prints only when every gate ran, otherwise "No findings from the N gate(s) that ran; K did not run — NOT a clean bill". Applied to ALL gates rather than a required subset (a gate that did not run is missing evidence whatever its tier) |
| A3 | **fixed (slice 2)** — `GateContext.degrade(note)` sink + `read_project_file` (unreadable ⇒ degraded, never "empty"); runner drains notes into `GateStatus::Degraded { findings, notes }`; verdict floors at Yellow, clean bill suppressed, markdown "⚠ Gates that ran DEGRADED — evidence is PARTIAL" + JSON `gates_degraded` / `kind: degraded`. Instrumented: 3 file reads (guard_parity, complexity_budget, added_conventions), blast_radius lookup, temporal + test_coverage neighbours, antipattern corpus-missing fallback + 2 searches, unwired + sync_contract query_nodes x3, product_intent + co_added_family searches. Left: per-hit `get_doc_by_pk` failures (3 sites) need an aggregated counter |
| A4 | **fixed (slice 3, 6f149c8, sweep17)** — `GateContext::note_cap` → `GateOutcome::caps` (JSON `gate_status[].caps`, markdown "## Caps hit"); instrumented: blast_radius FILE_CAP 20, temporal/test_coverage neighbour caps 20/30, unwired candidates 25, antipattern first 5 hits, sync_contract 300 contracts, product_intent first 5 hits, co_added_family first 4 families, complexity_budget 3 most complex. `tests/pre_commit_gate_caps_tests.rs` x2, RED first |
| A5 | **fixed (slice 4, af737da, deployed 03:15)** — `UnwiredVerdict { Unknown, Wired, Unwired }` + pure `unwired_verdict`; a failed node or caller lookup degrades the gate AND skips the candidate (a finding needs positive evidence). `tests/unwired_verdict_tests.rs`, RED first |
| A7 | **fixed (slice 6, 850d07f; sweep23 129/0; release 18)** — `source_type=rules|copilot` with JSON content parses the objects structurally (`rule` + examples), not as markdown lines; `clear_existing=true` purges the project's `quality_gate` namespace before ingesting (was a no-op). RED tests first |
| A6 | **fixed (d1252ca, deployed 04:31)** — `count_docs_by_namespace` before the search: 0 rules ⇒ "INACTIVE — 0 quality-gate rules are ingested … NOTHING was checked … run ingest_quality_gates"; no match ⇒ "N rule(s) exist and were searched (top_k K); 0 checked"; matches ⇒ "Checked: M of N (top_k K …)"; count failure a FAILURE line. `tests/pre_push_audit_tally_tests.rs` x2, RED first. OciusX rule ingestion itself (an ADO PAT + `ingest_quality_gates`) stays a user action |
| A7 | **partly (slices 1-3)** — `tests/pre_commit_gate_outcomes_tests.rs` (bail + panic), `tests/pre_commit_degraded_tests.rs` (injected provider outage; the REAL complexity_budget gate on a file missing from disk ⇒ "could not read"), `tests/pre_commit_gate_caps_tests.rs` (injected cap on the outcome/JSON/markdown; the REAL blast_radius gate on a 25-file diff ⇒ "20 of 25"), the first two mutation-checked. Still open: A5 unwired false positive (now DEGRADED rather than a finding-on-error, not yet skipped), A6 pre_push_audit |
| G1 | **met** — a forced Err/panic can no longer render Green or the clean-bill line (test) |
| G3 | **met** — JSON carries per-gate status |
| G2 | **met by construction** — a degraded provider can no longer render `passed` (test); live proof needs a provider outage, none observed on the healthy daemon |
| G4 | **met (live, §7b)** — every in-gate cap is a line: the live head diff shows `## Caps hit` for blast_radius (20 of 21 files), temporal ×4 and test_coverage neighbour caps |
| G5-G6 | with the next slices |

## 7. Live evidence — slice 1 (2026-08-28, commit 777c951, OciusX gen 828)

`pre_commit_review {"diff":"head","output_json":true}` on the live OciusX
checkout (21 files analysed, 3.3 s):

```
verdict yellow | gates_run 17 failed 0 panicked 0 skipped 0 | findings 24
gate_status: immune passed · blast_radius findings · style passed · temporal findings ·
             state passed · audit findings · new_file passed · test_coverage findings ·
             secret_leakage passed · guard_parity passed · unwired passed · sync_contract passed ·
             complexity_budget findings · added_conventions passed · antipattern findings ·
             product_intent passed · co_added_family passed
markdown header: **Gates run**: 17/17
```

Every gate ran on this diff, so the live output exercises the
all-ran path (per-gate status visible, header `17/17`); the
did-not-run rendering ("(K did not run)" header, "⚠ Gates that did not
run — evidence is INCOMPLETE" section, verdict floor at Yellow, clean-bill
suppressed) is proven by `tests/pre_commit_gate_outcomes_tests.rs`
(injected bail + panic gates), not by a live run — stated as such.

Not yet proven live: per-gate DEGRADED status (A3) — a gate whose provider
failed inside still renders `passed`; that is the next slice.

## 7b. Live evidence — slice 3 (2026-08-29 02:29 deploy, commit 6f149c8)

`pre_commit_review {"diff":"head","output_json":true}` on the live OciusX
checkout (21 files, 17 gates, verdict yellow, 24 findings):

```
gates with caps: 3
  blast_radius  → looked at 20 of 21 changed files (FILE_CAP 20) — shallower paths first
  temporal      → co-change neighbours of …/CompletionPrerequisiteEvaluator.vb: first 20 only (×4 files)
  test_coverage → co-change neighbours of …/CompletionPrerequisiteEvaluator.vb: first 30 only (…)
## Caps hit — these gates stopped looking at a limit          ← markdown section
```

Before this slice the same review rendered exactly like one that had
looked at every file and every neighbour.

## 7c. Live evidence — slice 4 (2026-08-29 03:15 deploy, commit af737da)

`pre_commit_review {"diff":"head"}`: `unwired` → `passed`, 0 degraded
gates — on a healthy daemon a failed lookup does not occur, so the live
run proves only the absence of false positives; the skip-on-failure rule
is proven by `tests/unwired_verdict_tests.rs`.

## 7d. Live evidence — slice 5 (2026-08-29 04:31 deploy, commit d1252ca)

`pre_push_audit` on OciusX (no rules ingested): **"Pre-push audit: INACTIVE
— 0 quality-gate rules are ingested for this project, so NOTHING was
checked. Run ingest_quality_gates …"** — the previous release said "no
matching rules", which read as a pass. `pre_commit_review {"diff":"head"}`:
17 gates run, 0 degraded, unwired passed.

## 7e. Live — rules ingested into the live OciusX project (2026-08-29 07:20, user-directed)

Ingested from the local corpora (no DevOps call needed): copilot-instructions.md
101 rules, `generic_rules.json` 258 (the June-distilled generic set), the
"PR Feedback Learning" board 14 — **373 in the `quality_gate` namespace**.
`pre_push_audit` on the same probe now says **"Checked: 12 rule(s) retrieved of
373 in the namespace (top_k 12 — the cap was filled; raise top_k for more)"**
and leads with real team rules ([Data Access] return `IQueryable`, [Database]
naming, SSDT schema model …).

**Finding (→ A7, slice 6):** the `generic_rules.json` hits render as raw JSON
fragments (`"bad_example": "Dim cat = pr.parent.category …"`): `source_type=rules`
routes to the MARKDOWN parser, which turns a JSON array of `{rule, bad_example,
good_example, …}` objects into line-shaped junk. And `clear_existing` is a
documented no-op ("reserved"), so the junk cannot be replaced by re-ingesting —
the quiet-failure class "schema param no handler reads".

## 7f. Live evidence — slice 6 / A7 (2026-08-29 08:00 deploy, commit 850d07f)

Re-ingest of the OciusX corpus through the new binary:

```
ingest copilot (clear_existing=true):  "clear_existing=true: purged 373 existing quality-gate rule(s) from the `quality_gate` namespace before ingesting." → 101 rules
ingest generic_rules.json (rules):     258 rules [115 high, 31 low, 112 medium], category from the JSON (null-handling, …) — was "258 medium" line-shaped junk
ingest board:                          14 rules
pre_push_audit (same probe):           "Checked: 12 rule(s) retrieved of 373 in the namespace (top_k 12 …)"; JSON fragments in the hits: 0
  - [generic_rules.json] Guard every reference-type value for Nothing before dereferencing it, including: lookup/repository results …
  - [generic_rules.json] Guard aggregate/statistical calls (.Min/.Max/etc.) against empty sequences …
```

The purge is stated, the count is unchanged (373 = 101 + 258 + 14), and the
generic rules now retrieve as rule sentences with their severity instead of
`"bad_example": …` fragments. Temp copies lived in the OciusX working tree's
git-excluded `_engram_tmp/` for the duration of the call and were removed;
nothing was committed there.
