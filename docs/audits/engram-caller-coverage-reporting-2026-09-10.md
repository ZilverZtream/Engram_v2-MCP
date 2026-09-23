# Caller coverage reporting review — 2026-09-10

## Status and scope

Read-only code review found no actionable correctness regressions in the reviewed caller expansion metadata and error handling, coverage interpretation, or eight-line caller excerpt windows. This is a bounded review, not acceptance of the surrounding work in progress. Combined validation and deployment passed; see final acceptance below. No builds or tests were run as part of this review.

Reviewed sources:

- [Access layer handlers](../../crates/engram_server/src/handlers/access_layer_tools.rs)
- [Caller excerpts](../../crates/engram_server/src/handlers/caller_excerpts.rs)
- [Caller expansion coverage tests](../../crates/engram_server/tests/caller_expansion_coverage_tests.rs)
- [Shared incoming caller lookup](../../crates/engram_server/src/handlers/mod.rs)

External validation evidence location: [caller-coverage-20260910](C:/ai-projects/audits/Engram/caller-coverage-20260910). This location is recorded for subsequent acceptance evidence; its contents were not validated in this review.

## Caller expansion reporting

`get_full_method_body` reports `caller_expansion` alongside the target body and returned caller bodies in JSON, with equivalent metadata in Markdown. Fields record whether expansion was requested and attempted, its status, returned body count, cap, truncation, omission reasons, and suggested next actions.

| Status | Meaning |
| --- | --- |
| `not_requested` | Caller expansion was not requested. |
| `unsupported_direct_range` | Expansion was requested for a direct file/line range without a unique indexed target method. |
| `omitted` | An indexed target was resolved, but `max_callers=0` disabled expansion. |
| `failed` | The incoming caller query failed. |
| `partial` | At least one selected indexed caller body was unavailable or withheld. The independent `truncated` field can also be true. |
| `truncated` | Selected caller bodies were returned successfully, but additional indexed callers exceeded the cap. |
| `complete` | The indexed lookup completed without detected omissions or cap truncation; this includes zero indexed callers. |

Caller query and individual caller source failures preserve a successfully retrieved target body. Missing source nodes, unreadable source, known fingerprint mismatches, and invalid caller spans are reported as omissions. The cap applies to selected indexed callers; unavailable bodies can reduce the returned count below the cap. Direct-range expansion guidance points to resolving an indexed method or using edit context with a more precise selector.

## Coverage interpretation

`EditContextCompleteness` supplies the same coverage qualification through its default constructor, `all_complete`, and the deserialization default for the added interpretation field. The qualification is serialized and rendered in coverage output. Rendered edit-context blast radius scores are explicitly described as estimates from indexed evidence.

Provider completion describes query execution and available indexed evidence. It does not establish exhaustive extraction, correct binding of every call, runtime coverage, or absence of consumers and side effects when indexed callers are empty. These reporting changes do not independently demonstrate improved consumer discovery or changed runtime behavior.

## Caller excerpt windows and limits

Caller excerpts require an indexed file fingerprint matching the bytes read and a valid indexed caller span. Reads respect the shared remaining byte budget and an eight-MiB per-file ceiling. Missing fingerprints, mismatches, exhausted budgets, invalid spans, and non-UTF-8 source withhold excerpts with an explanation.

Selection uses case-insensitive lexical matches of the final method-name component followed by an opening parenthesis. It considers at most three matching lines, each contributing a window of up to eight lines: one preceding line, the matching line, and up to six following lines. Overlapping output is deduplicated, source line numbers are preserved, and each source line is limited to 240 characters plus a truncation marker.

These windows are bounded source leads. Comments, strings, and calls on other receivers can match. Windows can stop within an argument list or branch, and additional matches can remain undisplayed. Full caller inspection remains necessary for argument binding, default values, and complete output handling.

## Validation and deployment acceptance

The reviewed test source covers direct-range reporting in JSON and Markdown, exact caps and truncation, stale and missing caller files while preserving target output, empty versus disabled expansion, consistent completeness qualification, and rendered blast-radius qualification. Caller excerpt unit tests cover nearby result comparisons, numbering and overlap, output bounds, and read budgets.

The combined source passed 38 focused tests and the optimized release build. Six new integration checks verify expansion states and coverage qualifications. The first fixture attempt failed because synthetic functions lacked FQN metadata; that fixture was corrected without changing assertions or production resolution. Earlier partial builds and failed receipts were retained and not deployed.

Real compiled MCP fixtures passed overload/unique caller handling, adjacent result-comparison visibility, direct-range JSON/Markdown omissions, exact FQN caps, and serialized coverage qualification. All 147 input schemas remained unchanged. An independent read-only review found no actionable issues. A separate copied historical graph diagnostic confirmed wider excerpts include nearby comparison handling, without changing caller bindings or the original trial store. This is tool behavior evidence, not proof of better agent outcomes.

Deployment passed live project health and schema checks; configuration was unchanged. Installed SHA-256: `4e6dfcea97ae539db5a4dc7d10c1100e8c9c2f775e1f81bac66878912cd63abc`. Final external receipt: `C:/ai-projects/audits/Engram/caller-coverage-20260910/combined-validation-summary.json`. Method-context readiness remains a provisional 9/10.
