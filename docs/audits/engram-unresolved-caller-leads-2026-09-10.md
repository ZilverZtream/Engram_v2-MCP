# Unresolved caller inspection leads

A historical replay exposed a gap in pre-edit discovery: a shared overloaded method had no bound incoming callers in the fallback graph, although source inspection found consumers. Conservatively refusing an overload binding is correct; omitting the unresolved references leaves an agent without useful leads.

`get_method_edit_context` now returns a separate `unresolved_caller_leads` section. It searches retained unresolved names, reads fingerprint-verified source windows and explains that overload/receiver binding is unproven. It does not modify graph bindings, confirmed caller counts, blast scores or safety verdicts. The no-caller advisory now includes unresolved overloads and extraction gaps among possible causes.

Lookup covers up to eight exact-spelling name suffixes, the first 100 incoming call references per suffix in source-id order, and one lookahead for truncation. Output follows `max_callers`, separately from confirmed callers, with a hard limit of 20 leads and the existing shared source-read budget. Missing fingerprints, changed source, stale graph generations, duplicate confirmed callers, caps and lookup errors receive explicit handling. This is partial indexed discovery, not complete static analysis or runtime binding.

An independent review caught that the first implementation used a weighted graph API which scanned and sorted the entire matching adjacency before truncating. That release was withheld. The final implementation uses a separate bounded prefix iterator; existing weighted lookup behavior is unchanged. A follow-up review found the correction addressed the issue.

Validation passed 29 focused checks: one graph prefix test, five caller-lead tests, seven edit-safety tests, one caller-count parity test and 15 method-context integration tests. Compiled MCP probes verified two unresolved source leads with zero guessed bindings for overloaded methods, and no duplicate leads for uniquely bound callers. All 147 schemas matched the preceding release except the `max_callers` description.

A post-seal diagnostic used separate byte-identical copies of the historical graph/registry, with search/history/LLM providers excluded. It exposed the two manually discovered consumers among three verified leads; the old binary exposed none. Confirmed bindings and blast scores were unchanged. Single-call timings were approximately 0.110 and 0.113 seconds; these are not a performance benchmark. Original trial stores were preserved, and the new result was not supplied to trial participants or used as their behavioral evidence.

Release SHA256 `894d96b7451988bbf264a0e8937aa0c9855d79f9c9ef49cd80d979b65ab0b728` was deployed at 2026-09-10 07:54:12 UTC. Live health was OK, 147 tools were advertised, configuration was unchanged and a rollback binary was retained. Existing unresolved graph references benefit without re-extraction; absent references are not recreated by this read-only feature.

Method edit context remains a provisional 9/10. Passing these checks does not establish agent speedup, general caller recall or a behavioral advantage in the historical replay.

Reproducible validation, review, compiled probes and deployment: `C:\ai-projects\audits\Engram\caller-coverage-20260910\`. Application-specific diagnostic evidence remains outside the Engram repository in the isolated replay campaign.
