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

## 6. Disposition table (fill at implementation)

| Item | Disposition |
|---|---|
| A1 | |
| A2 | |
| A3 | |
| A4 | |
| A5 | |
| A6 | |
| A7 | |
| G1-G6 | |
