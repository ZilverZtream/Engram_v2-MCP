# Tool Contract (v1 parity)

Engram MCP v2 keeps v1 tool names and basic intent.

## Project tools
- `index_project(directory, project_name, project_type, wait=true, dedupe_by_directory=true)`
- `update_project(project_id, wait=true, max_commits=200, index_antipatterns=false)`
- `list_projects()`
- `project_info(project_id)`
- `project_health(project_id)`
- `repair_project(project_id)`
- `delete_project(project_id)`
- `watch_project(project_id, enabled=true)`
- `unwatch_project(project_id)`

## Search/tools
- `search_memory(project_id, query, top_k=10)`
- `get_chunk(project_id, chunk_id, namespace="memory")`

## Memory bank + repo rules
- `update_memory_bank(project_id, section_id?, title, content)`
- `list_memory_bank(project_id)`
- `read_memory_bank(project_id, section_id)`
- `delete_memory_bank(project_id, section_id)`
- `add_repo_rule(project_id, file_pattern, rule_text)`
- `list_repo_rules(project_id)`
- `delete_repo_rule(project_id, rule_id)`

## Graph
- `query_graph_nodes(project_id, query, top_k=10)`
- `find_references(project_id, node_id, top_k=10)`
- `graph_search(project_id, query, top_k=10)`

## Git/history
- `index_git_history(project_id, max_commits=200, index_antipatterns=false, wait=true)`
- `search_history(project_id, query, top_k=10)`
- `analyze_temporal_couplings(project_id, file_path, top_k=10)`
- `analyze_reverts(project_id, max_commits=200)`

## Cognitive / agent
- `dream_project(project_id)`
- `trigger_rem_cycle(project_id)` (alias)
- `analyze_file_coding_style(project_id, file_path, max_commits=200)`
- `get_codebase_overview(project_id)`
- `find_symbol_references(project_id, symbol, top_k=10)`
- `analyze_error_stack(project_id, stacktrace, top_k=10)`

## Jobs
- `list_jobs(project_id?)`
- `cancel_job(job_id)`

### Notes
- Some tools are **scaffolded** (placeholders) but compile and define the intended boundaries.
- v2 adds optional extra tools (e.g. `immune_check`) without breaking v1 parity.
