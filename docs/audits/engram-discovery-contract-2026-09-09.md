# Discovery contract correction — 2026-09-09

Tool discovery now qualifies what indexed evidence can establish and distinguishes project orientation from task-specific retrieval.

- `ingest_merged_prs` and `find_merged_work` describe Git-derived merged work and possible direct commits. They no longer imply verified review approvals or proven correctness.
- `get_concept_footprint` describes indexed, bounded coverage and directs callers to inspect reported caps and failures before treating scope as complete.
- `get_codebase_overview` remains the project orientation tool. Its description points concrete questions to `ask_codebase` and known identifiers to `search_memory` or `resolve_id`.

No ingestion, retrieval, graph, or ranking algorithm changed. These corrections do not establish improved agent outcomes or justify a score increase.

Validation: four existing tool-surface integration tests passed. Release build and fresh compiled MCP discovery passed with 147 tools. After installation, a fresh live MCP client returned the verified descriptions and project health was OK. Configuration remained unchanged; a rollback binary was retained. Historical replay binaries were not modified during their trials.

Installed binary SHA256: `cca2cc516bcff661b723107744d6955bc98a0590446ee0c95244c9a9c7128e0a`.

Machine-readable build, MCP acceptance, and deployment receipts are under `C:/ai-projects/audits/Engram/discovery-contract-20260909/`. Historical application artifacts remain outside this repository.
