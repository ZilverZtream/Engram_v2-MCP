# Tool description accuracy — 2026-09-10

The compiled MCP descriptions now qualify two claims that exceeded the available evidence. `find_merged_work.merged_before` filters merge dates; it does not date-bound later edits to imported records. `get_change_set` suggests candidate scope from indexed evidence; its results do not establish complete scope, required edits or reviewer approval.

Acceptance compared all 147 compiled schemas against the preceding installed release. Only the `get_change_set` top-level description and the `find_merged_work.merged_before` property description differed. Request shapes and parser source hashes were unchanged. No behavioral improvement is claimed from this wording change.

Release `5c8de1f12dd5732bd37a0934231b5d3ad3d0617124498e089cd540c9a7cc598e` was installed on 2026-09-10 at 07:22:31 UTC. Live health returned OK, all 147 tools were advertised, and the configuration hash was unchanged. A rollback binary was retained. Ongoing isolated trials retain their frozen preceding binaries and schemas.

Reproducible probe, exact schema comparison, source hashes, deployment checks and rollback receipt: `C:\ai-projects\audits\Engram\schema-claims-20260910\`.
