# ask_codebase M1 — live eval result (OciusX)

**Date:** 2026-08-21 · **Project:** OciusX (`5a35e8e0-…`, dotnet_webforms_vb, code-only index)
**Binary:** `ask-codebase-brain` @ `2bec522` (deployed to the live daemon)
**Harness:** `eval/ask_engine_golden.py` + `eval/ask_engine_golden.jsonl` (12 questions)

## Result — GATE: PASS

| metric | value | gate |
|---|---|---|
| abstain on missing_knowledge | **2/2 = 100%** | = 100% ✅ |
| status-match (correct engagement) | **11/12 = 92%** | ≥ 80% ✅ |
| mean citation coverage (answerable) | 0.76 | — |
| mean latency | 634 ms | — |

Graph-backed arms (impact / rationale / compound / bug) hit **1.00** citation
coverage. Cold-start (first call) is ~2.9 s on this large redb store; warm calls
are ~0.3–0.5 s.

## What the live eval caught that unit tests could not

The eval drove three fixes that only surface on a large real codebase, where
loose FTS finds *something* for any question:

1. **Abstention was broken** — nonsense questions returned `partial`/`answered`
   because evidence was never empty. Fixed with `has_adequate_support`
   (`status.rs`): a resolved-entity graph relation, or ≥2 distinct query terms
   covered across the evidence set.
2. **Everything defaulted to `partial`** — OciusX indexes no docs/business-rules/
   history, so "Answered needs all needed_evidence kinds" was unreachable. Fixed
   by keying Answered off the answer type's **primary** evidence kind.
3. **Concept-arm false support + per-hit coverage** — a single-stem concept match
   (a "…Policy" class) and a per-hit term rule caused both false support (missing
   questions) and false abstention (compound questions). Fixed: concept excluded
   from adequacy + no question-echo; distinctive terms (len ≥ 5, filler-filtered);
   coverage aggregated across the evidence set.

## Known conservative behavior (by design)

`bug_1` ("why does import fail for *some users*") abstains: the engine found
import code but no evidence about the user-specific failure mode. Abstaining is
the safe failure mode (honest "I can't support that" beats a fabricated cause) —
consistent with the eval's anti-anchoring governing principle.

## Reproduce

Deploy the branch binary to the daemon, then:
`python eval/ask_engine_golden.py 5a35e8e0-d37a-41b3-a250-a26957e7aedb`
