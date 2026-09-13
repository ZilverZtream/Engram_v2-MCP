# Replay adapter failures and repair

A historical replay exposed two interface failures before useful tool execution:
the old Python caller could not print the full tool catalogue under Windows
cp1252, and it discarded the body of an HTTP error from the broker.

The preserved planning request had PowerShell extended-string objects in `story`
and `work_item_text`, contrary to their advertised string types. After both
submissions were sealed, the unchanged request reproduced MCP error -32602:
`invalid type: map, expected a string`. The broker wrapped that rejection in
HTTP 500. This establishes a malformed request and lost diagnostic, not a
failure inside Engram's planner.

`tools/replay_mcp_call.py` is the repaired generic evaluation client. It emits
ASCII-safe JSON, reads UTF-8/BOM and UTF-16 argument files, checks required and
obvious top-level argument types against advertised schemas, and preserves HTTP
error bodies. It reports PowerShell metadata objects with a `ReadAllText` remedy;
it never silently coerces them. The preflight is intentionally not a complete
JSON Schema validator. MCP still validates the request.

Five subprocess/HTTP tests passed, including Unicode round trips under cp1252,
pre-network rejection of malformed arguments, HTTP diagnostic preservation and
non-success exit for MCP errors. The actual 147-tool replay catalogue reproduced
the old Unicode failure and passed with the repaired client. The original
malformed request is rejected locally with an actionable explanation.

Evidence is retained at `C:/ai-projects/audits/Engram/replay-adapter-20260910/`.
Use the new client for future replay adapters. Do not replace the copied client,
participant artifacts or traces inside an already frozen trial.

The affected participant completed one search call. It did not call the new
method historical-context or caller-excerpt features, so this trial cannot
establish their benefit to implementation. Its original interface failures remain
part of the evidence; passing client tests do not change the behavioral grade or
prove improved Engram reasoning.
