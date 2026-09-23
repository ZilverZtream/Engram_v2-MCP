# Business-rule shape validation

The parser previously counted an object containing only `source_line` and
`refs` as a business rule: formatting its metadata produced a nonempty string.
It also accepted an incomplete WHEN/THEN pair without a shape warning.

Metadata-only objects are now excluded from the rule count and explicitly
reported in source-check warnings. Partial pairs and legacy free-text `rule`
objects remain available, with a warning to verify the condition and observable
outcome before deriving tests. Complete pairs retain their existing behavior.
This is structural validation; it does not verify the meaning of a rule.

The compiled MCP fixture also exposed missed C# declarations when a class and
its method shared a line. C# business-analysis discovery now uses syntax nodes
and exact declaration ranges, including generic and expression-bodied methods.
Ownership follows syntax ancestry. Comments and abstract declarations without
executable bodies do not create analysis targets. Two parser regressions cover
these cases. The initial failed MCP fixture is retained as evidence.

Both new integration regressions failed before the fix. All eight source-anchor
tests pass after it, together with the existing retry and overload tests.
Build and compiled MCP acceptance evidence is recorded separately under
`C:\ai-projects\audits\Engram\business-rule-shape-20260909`.
The MCP fixture uses a local deterministic HTTP response, not a model comparison.

Final validation passed: 1,277 server unit tests, 21 focused integration tests,
and the original compact-declaration regression through the compiled MCP server.
The build deployed at 20:16 CEST after the history repair completed. Fresh MCP
acceptance confirmed 147 tools, healthy project state and unchanged configuration.
The rollback path and binary digest are in `deployment.json` in the evidence root.

Business-rule reliability remains 8/10 because semantic fidelity and coverage
are still limited.
