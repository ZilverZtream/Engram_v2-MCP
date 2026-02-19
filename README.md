# Engram MCP v2 (Scaffold)

This is a Rust workspace scaffold for **Engram MCP v2**: unified code search + graph cognition + git intelligence + cognitive features.

## Quick start

1) Install Rust (stable)

2) Create a config file (YAML), e.g. `engram_mcp.yaml`:

```yaml
allowed_roots:
  - /path/to/your/repos
data_dir: /path/to/engram-data
embedding_backend: local
```

3) Run the server:

```bash
export ENGRAM_CONFIG_PATH=/path/to/engram_mcp.yaml
cargo run -p engram_server
```

> MCP servers communicate over STDIO. Do not print to stdout from the server.

## v1 tool parity

See `docs/TOOL_CONTRACT.md` for the v1-compatible tool list.

## What’s implemented vs stubbed

- ✅ registry + generation semantics
- ✅ Tantivy lexical search + chunk retrieval
- ✅ dreamer actor + insight nodes (deterministic summarizer)
- ✅ temporal coupling edges (basic)
- 🟡 watchers / incremental reindex (planned)
- 🟡 vector search (planned)
- 🟡 immune anti-pattern index (planned)

## Repo structure

See `docs/DEVELOPER_SPEC.md` and `docs/ARCHITECTURE.md`.
