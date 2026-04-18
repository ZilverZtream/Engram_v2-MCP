# Engram Dashboard — Design

**Status:** Draft, pending implementation plan
**Date:** 2026-04-18
**Owner:** Dennis Östling
**Scope:** New `engram_dashboard` crate — a browser-based workbench that surfaces the full Engram MCP capability set as 8 lenses over a shared `AppState`.

---

## 1. Goals & non-goals

### Goals

1. Give the developer using Engram a single, coherent UI that exposes **everything** the MCP server can do — without requiring Claude as an intermediary.
2. Make daily tasks (inspect a method, explore the graph, run a tool, browse data) faster than the equivalent MCP-tool-call-through-Claude roundtrip.
3. Ship one binary: the existing Engram MCP server plus the dashboard. No extra runtime (no Node, no external web server).
4. Stay a second UI over existing services — never a second data path. If MCP and the dashboard disagree, that is a bug.
5. Support full CRUD + a typed query DSL (EQL) over the Redb-backed store, with an audit/undo log for write safety.

### Non-goals (v1)

- Multi-user / multi-tenant service. One user, one local session.
- TLS / cert provisioning. Loopback is the supported mode; remote exposure is documented but on-your-own.
- Mobile-responsive design. Desktop-first, assume ≥1280px.
- Durable activity history (>10 000 events). In-memory ring buffer only.
- Joins in the query language. Structured navigation helpers only.
- Playwright full-browser E2E tests. Manual smoke checklist instead.

---

## 2. Audience & primary use case

**Audience:** the developer (you) using Engram mid-migration. The dashboard is a daily tool, optimized for density and speed over polish — though it should look professional enough to screenshot.

**Primary use case:** "simplified access to the tool" — the user wants one place to explore, inspect, query, and run everything Engram has gathered about a project.

---

## 3. Architecture

### 3.1 Crate layout

One new crate: `crates/engram_dashboard/`. Depends on `engram_server`, `engram_graph`, `engram_index`, `engram_core`. Does not invert any existing dependency.

```
crates/engram_dashboard/
├── Cargo.toml
├── build.rs                  # runs pnpm build, checks dist/ freshness
├── src/
│   ├── lib.rs                # pub fn spawn_dashboard(state, cfg) -> JoinHandle
│   ├── cli.rs                # `engram dashboard` subcommand wiring
│   ├── server.rs             # axum router composition, loopback bind
│   ├── config.rs             # DashboardConfig, env/flag merge
│   ├── auth.rs               # CSRF middleware, origin check
│   ├── ws.rs                 # WebSocket hub + event bus subscriber
│   ├── error.rs              # DashboardError + IntoResponse
│   ├── events.rs             # DashboardEvent enum, broadcast tx
│   └── routes/               # one module per lens (overview, graph, inspector,
│                             # tools, migration, business_logic, data, query,
│                             # activity, settings, assets)
└── web/                      # SvelteKit app (ignored by cargo)
    ├── package.json
    ├── svelte.config.js
    ├── vite.config.ts
    └── src/
        ├── app.html
        ├── lib/              # api/, ws/, components/, stores/
        └── routes/           # +layout, overview, graph, inspector, tools,
                              # migration, business-logic, data, activity, settings
```

### 3.2 Process model

The dashboard runs in the **same process** as the MCP server, sharing one `Arc<AppState>`. There is no second database connection, no second indexer, no cached mirror of data. Every read/write goes through the same services the MCP tools use.

```
Browser SPA  ──HTTP JSON──►  axum router  ──►  Arc<AppState>  ──►  services/handlers
                                   │                              (same as MCP tools)
             ◄────WebSocket────────┘
             (tool-call events, index progress, graph updates)
```

### 3.3 Stack

**Backend:**
- `axum 0.7` (HTTP + WebSocket) on tokio.
- `tower-http` for `CompressionLayer`, `TraceLayer`, static file serving.
- `rust-embed` to bake the built SvelteKit assets (`web/dist/`) into the binary.
- `tower-sessions` with `MemoryStore` for short-lived session cookies.
- `utoipa` for OpenAPI schema generation (emitted at build time, consumed by the frontend's typed client generator).

**Frontend:**
- SvelteKit with `adapter-static`, TypeScript, Tailwind CSS.
- `Cytoscape.js` for graph viz (handles 10k+ nodes).
- `CodeMirror 6` for the EQL editor.
- `shadcn-svelte` for form primitives.
- `openapi-typescript` generates the HTTP client types from `utoipa`'s OpenAPI JSON.

**Build integration:**
- `build.rs` in `engram_dashboard` shells out to `pnpm install && pnpm build` in `web/`. Caches output. Incremental: Rust-only changes skip the frontend build.
- `cargo build --release` runs production frontend build with Brotli compression.
- Dev mode: `ENGRAM_DASH_DEV=1` makes the Rust server proxy unknown paths to Vite on `:5173` for HMR without rebuilding the binary.

---

## 4. The 8 lenses

Each lens is one `routes/*.rs` backend module + one `web/src/routes/<lens>/+page.svelte` frontend route. Sidebar order top-to-bottom:

### 4.1 Overview — landing
KPI tiles (migration progress, coverage, anti-patterns, edit-safety reds), top-10 hotspots, recent tool calls (live via WS), graph preview that deep-links to the Graph lens.

### 4.2 Graph explorer
Cytoscape.js renderer with `cose-bilkent` layout. Filter by node type, edge kind, project, namespace. Search → focus node; neighbors on click. Node detail pane with props, edges, and an "open in Inspector" link. Saved views, color-by (centrality, blast radius, anti-pattern density).

### 4.3 Inspector
Search bar with fuzzy match across methods/files/classes/tables. Side-by-side source code + all access-layer tool outputs (Phase 38). Tabs: Callers, Callees, SQL, State, Anti-patterns, Tests, Blast radius, Edit-safety. One-click "prepare implementation context" copies a dossier for Claude. Opens parent page/class/control.

### 4.4 Tool runner
All 99 MCP tools as auto-generated forms (from request schemas). Tool catalog grouped by capability. Execution history with re-run. Pinned favorites. Results render as JSON tree + formatted views per tool shape. "Send to Inspector" and "Pipe to query" helpers.

### 4.5 Migration
Full-project migration report (Phase 32/33 output) rendered as an interactive document. Per-file dossiers (Phase 17). Migration order plan with rationale (Phase 31). Coverage heatmap. Characterization test gen status. Rollout / kill-switch controls (Phase 27).

### 4.6 Business logic
Front-end for `query_business_logic` — ask in plain English, stream the answer. Domain concept map (entities, processes, rules). Business-logic → code location links. Confidence and provenance visible on every answer. Editable curated summaries per node.

### 4.7 Data browser
Typed table picker (nodes, edges, docs, vectors, insights, rules, watches, jobs, checkpoints, memory_bank, dashboard_ops). Visual query builder stacked over a raw EQL editor with bidirectional sync (see §6). Result table with inline edit/delete. Export CSV/JSON/Parquet. Re-index reconcile. Schema viewer.

### 4.8 Activity log
Live WS stream of tool calls, jobs, ADP verdicts, index progress. Filter by kind/outcome/duration. Drill into a call → full request + response + timing. Error log with stack traces. Prometheus-style metrics page at `/api/v1/metrics`.

### 4.9 Settings (ninth item, bottom of sidebar)
Project switcher, rules, watches, memory bank CRUD, `boundaries.yaml` editor, ignore patterns, embedder config, rollout kill-switch, export/import project state, appearance/theme.

---

## 5. HTTP API surface

All endpoints under `/api/v1/`. Every response includes `project_id`. Errors use RFC 7807 `problem+json`. Pagination via `?cursor=&limit=` (default 50, max 500). Writes that cost >200ms or mutate >1k rows return `202 Accepted` with a `job_id`; progress via WebSocket.

```
# --- Overview ---------------------------------------------------------------
GET  /api/v1/overview                         → kpis, hotspots[10], recent_calls[20], graph_preview
GET  /api/v1/projects                         → [{ project_id, name, indexed_at, file_count, size_bytes }]

# --- Graph explorer ---------------------------------------------------------
GET  /api/v1/graph/search?q=&kind=&limit=     → [{ node_id, label, kind, score }]
GET  /api/v1/graph/node/:id                   → node, in_edges[], out_edges[], props
GET  /api/v1/graph/neighbors/:id?depth=1      → { nodes, edges } (capped)
GET  /api/v1/graph/stats                      → nodes_by_type, edges_by_kind, centrality_top
POST /api/v1/graph/view                       → save named view
GET  /api/v1/graph/view/:id                   → saved view

# --- Inspector --------------------------------------------------------------
GET  /api/v1/inspect/method/:node_id          → full access-layer roll-up
GET  /api/v1/inspect/file/:doc_id             → methods[], controls[], anti_patterns[]
GET  /api/v1/inspect/page/:doc_id             → page_context

# --- Tool runner ------------------------------------------------------------
GET  /api/v1/tools                            → [{ name, group, schema, description }]
GET  /api/v1/tools/:name                      → full schema + example + recent calls
POST /api/v1/tools/:name/run                  → { request_id, result | job_id }
GET  /api/v1/tools/history?cursor=            → recent executions
POST /api/v1/tools/:name/favorite
POST /api/v1/tools/:name/pin-params

# --- Migration --------------------------------------------------------------
GET  /api/v1/migration/report
GET  /api/v1/migration/dossier/:doc_id
GET  /api/v1/migration/order
GET  /api/v1/migration/coverage
GET  /api/v1/migration/characterization-tests
POST /api/v1/migration/rollout                → { action: "enable"|"kill-switch", ... }

# --- Business logic ---------------------------------------------------------
GET  /api/v1/bl/concepts
POST /api/v1/bl/query                         → SSE stream
GET  /api/v1/bl/summary/:node_id
PATCH /api/v1/bl/summary/:node_id

# --- Data browser + CRUD + query -------------------------------------------
GET    /api/v1/data/tables                    → [{ name, row_count, schema }]
GET    /api/v1/data/tables/:table/schema
GET    /api/v1/data/tables/:table/rows?q=     → paginated rows (q = compiled EQL)
GET    /api/v1/data/tables/:table/row/:pk
POST   /api/v1/data/tables/:table/row
PUT    /api/v1/data/tables/:table/row/:pk
PATCH  /api/v1/data/tables/:table/row/:pk
DELETE /api/v1/data/tables/:table/row/:pk
POST   /api/v1/data/query                     → compiled EQL (§6)
GET    /api/v1/data/export?table=&format=     → streamed CSV/JSON/Parquet
POST   /api/v1/data/reconcile                 → re-derive table from source
GET    /api/v1/data/undo                      → recent write ops
POST   /api/v1/data/undo/:op_id               → revert one operation

# --- Activity log -----------------------------------------------------------
GET  /api/v1/activity?since=&kind=&outcome=
GET  /api/v1/activity/:event_id
GET  /api/v1/metrics                          → Prometheus text

# --- Settings ---------------------------------------------------------------
GET  /api/v1/settings
PUT  /api/v1/settings
GET|POST|PATCH|DELETE /api/v1/settings/rules[/:id]
GET|POST|PATCH|DELETE /api/v1/settings/watches[/:id]
GET|POST|PATCH|DELETE /api/v1/settings/memory[/:id]
GET|PUT /api/v1/settings/boundaries
GET|PUT /api/v1/settings/ignore-patterns

# --- WebSocket --------------------------------------------------------------
GET /ws  (upgrade)  subprotocol: "engram-dash.v1"

# --- Auth / system ----------------------------------------------------------
GET  /api/v1/csrf                             → { token }; sets session cookie
GET  /api/v1/health                           → { ok, version, uptime_s }
```

Every `routes/*.rs` handler is a thin composition over existing `engram_server/services/*` modules — no logic duplication.

---

## 6. EQL — Engram Query Language

A typed subset-of-SQL DSL compiled to Redb index/range scans.

### 6.1 Syntax

```
from <table>
[where <predicate> (and|or <predicate>)*]
[order by <field> (asc|desc)]
[limit <n>]
[offset <n>]
```

**Predicate operators:** `==`, `!=`, `<`, `<=`, `>`, `>=`, `in (...)`, `starts_with`, `contains`, `matches <regex>`, `exists`, `is null`.

**Types:** string, int, float, bool, timestamp (RFC3339), enum (schema-checked), array (only `in` / `contains`).

**No joins in v1.** Structured navigation helpers (open in Inspector, show neighbors in Graph, fetch doc content) replace joins. A restricted `join on edge_kind = ...` can come in v2 if it's actually missed.

### 6.2 Compiler

1. **Parse** → typed AST; field names checked against the target table's schema, types enforced.
2. **Plan** → choose the best index for the leftmost equality-prefix of the WHERE clause. E.g., `type == 'function' and project == 'legacyerp'` uses `nodes_by_type_project`; `centrality > 0.4` alone requires full scan.
3. **Explain** → `POST /api/v1/data/query` with `?explain=true` returns the chosen index, estimated rows scanned, and filter selectivity.
4. **Guardrail** → any query estimated to scan >1M rows requires a client-side confirmation modal before execution.

### 6.3 Bidirectional builder ↔ editor

The visual query builder and the raw EQL editor compile to the same AST. Edits in either surface round-trip losslessly. The editor uses CodeMirror 6 with an EQL language definition (syntax highlighting + autocomplete against the current table's schema).

### 6.4 Writes & audit

- Every write goes through `GraphStore` / `DocStore` validation — no orphan edges, no nodes with live edges deleted without `cascade: true`, types enforced.
- Every write appends to a `dashboard_ops` Redb table: `{op_id, user, table, operation, before_blob, after_blob, ts}`.
- The Undo panel shows the last 200 ops; undo applies the inverse and records *that* as a new op (discoverable).
- Destructive ops (DELETE, bulk update, `cascade: true`) require a confirmation modal with row-count preview. Queries touching >100 rows show the count before applying.
- **Reconcile.** Extracted data is a derivation of source code; edits drift from truth on the next reindex. A banner appears on any table where dashboard writes exist but source files have changed since, with a "reconcile (re-derive from source)" action.

---

## 7. Realtime — WebSocket

### 7.1 Event bus

A new `tokio::sync::broadcast::Sender<DashboardEvent>` lives inside `AppState` (added in `engram_server/src/state.rs`). Existing event sources publish:

| Source | Event |
|---|---|
| MCP tool dispatcher (middleware) | `tool_call_started`, `tool_call_completed` |
| Ingest service | `index_delta`, `job_progress` |
| ADP pipeline | `adp_verdict` |
| Graph store writes | `graph_delta` (coalesced ≤10/s) |
| Generic activity | `activity_event` |

### 7.2 Protocol

One endpoint: `/ws`, subprotocol `engram-dash.v1`. JSON frames, heartbeat every 20s. Broadcast channel capacity 4096. Per-client outbound buffer 256 frames; overflow → close with code 1008. Per-client max subscribe topics: 32. Topics support a single trailing wildcard (`tool_call_*`, `*`).

**Server → client:** `tool_call_started`, `tool_call_completed`, `job_progress`, `job_completed`, `index_delta`, `adp_verdict`, `graph_delta`, `activity_event`, `lagged { skipped: N }`.

**Client → server:** `subscribe { topics: [...] }`, `unsubscribe { topics: [...] }`.

### 7.3 Persistence

None for WS frames. The `activity` lens reads from a bounded in-memory ring buffer (10 000 events) that events are mirrored into. Durable history is out of scope for v1.

### 7.4 Reconnect

Client: exponential backoff 1s → 30s. On reconnect, client includes `?last_event_ts=`; server replays whatever's still in the ring buffer.

---

## 8. Security

### 8.1 Bind

Default `127.0.0.1:<auto>`. `--host 0.0.0.0` requires explicit flag, prints a red warning, and auto-generates a 32-char bearer token printed once to stderr. Loopback mode needs no token.

### 8.2 Origin check (CSRF)

Every non-`GET` request is rejected unless:
- `Origin` matches `http://localhost:<port>` or `http://127.0.0.1:<port>`, **and**
- `X-Engram-CSRF` header matches the session cookie's CSRF token.

Bootstrap: SPA calls `GET /api/v1/csrf` on load; server sets `engram_sess=...; HttpOnly; SameSite=Strict; Path=/`.

### 8.3 GET safety

No `GET` endpoint mutates state or triggers expensive work. Running a tool requires `POST /api/v1/tools/:name/run`.

### 8.4 WebSocket

Upgrade handshake requires the session cookie *and* an `Origin` match. Subprotocol `engram-dash.v1` required.

### 8.5 Headers

```
Content-Security-Policy:
  default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline';
  img-src 'self' data:; connect-src 'self' ws://localhost:<port> ws://127.0.0.1:<port>;
  frame-ancestors 'none'; base-uri 'self'; form-action 'self';
X-Frame-Options: DENY
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
```

### 8.6 Input validation

- `serde` structs use `#[serde(deny_unknown_fields)]`.
- EQL → typed compiler (no string interpolation).
- File paths → `PathContext::resolve_path` / `safe_join` (existing, Phase 27-tested).
- Project IDs → `validate_project_id` (existing).

### 8.7 Rate limits

- Per-IP: 100 req/s burst, 20 req/s sustained (relevant for `--host 0.0.0.0`).
- `POST /api/v1/tools/:name/run`: 10 concurrent executions regardless of source.
- WS: 32 topics/client, 256-frame outbound buffer.

### 8.8 Audit

- Every write → `dashboard_ops`.
- Every tool execution → existing activity log.
- Remote mode additionally logs source IP.

### 8.9 Out of scope

- Malicious local user. Multi-user isolation. TLS cert provisioning. Defending against a compromised `AppState`.

---

## 9. Deployment & launch UX

### 9.1 CLI

```
engram dashboard                 # --port 0 default, loopback, auto-open browser
engram dashboard --port 53541    # explicit port
engram dashboard --no-open
engram dashboard --host 0.0.0.0  # remote mode; warning + bearer token
DASHBOARD_AUTOSTART=1 engram mcp # both stdio-MCP + dashboard in same process
```

Library use:
```rust
use engram_dashboard::spawn_dashboard;
let handle = spawn_dashboard(state.clone(), DashboardConfig::default());
```

### 9.2 First-run

1. Bind port, print connection banner to stdout.
2. If `--open` (default), launch browser.
3. SPA calls `GET /api/v1/csrf` → sets cookie; then `GET /api/v1/projects`.
4. Landing route `/` = Overview. Empty state if no projects indexed.

### 9.3 URL state

Every lens takes `project_id` as a query/path param (no implicit "current project"). Example: `/inspector?project=legacyerp&node=n_18a2` is a shareable link.

### 9.4 Shutdown

`Ctrl-C` → graceful: close WS with 1001, drain in-flight requests (5s timeout, then hard-cancel), flush audit log, close axum listener.

### 9.5 Multi-instance

Same port → clear "port in use" error. Different ports on the same `AppState` → share via Redb single-writer file lock (existing behavior).

### 9.6 Docs

New `docs/dashboard/`: first-run guide, EQL reference, security notes, auto-generated OpenAPI reference, WS event reference. Linked from the README.

---

## 10. Testing strategy

### 10.1 Backend

- **Unit tests** per `routes/*.rs` for handler logic. Target ≥80% line coverage.
- **Handler-level integration tests** in `crates/engram_dashboard/tests/`, one file per lens. Spin up an in-memory `AppState`, call `spawn_dashboard`, hit the real HTTP surface via `reqwest`.
- **Property-based tests (`proptest`)** for EQL: parse→print round-trip; type-check soundness; no-panic on any input.
- **Contract tests** asserting dashboard API responses match the underlying MCP tool outputs over the same graph state.
- **Audit log tests**: every write appends exactly one op; every undo applies the inverse.
- **Concurrency tests**: 10 parallel clients, Redb single-writer semantics, WS back-pressure with correct `lagged` signals.
- **Security regression tests**: CSP headers, origin check, rate limits, path validation.

### 10.2 Frontend

- **Component tests** with `@testing-library/svelte` + `vitest`.
- **Route-level tests** with MSW-mocked APIs, asserting user-visible flows.
- **Type safety**: CI fails if generated `openapi-typescript` types drift from usage.
- **No Playwright in v1**: codified manual smoke checklist in `docs/dashboard/smoke-checklist.md`.

### 10.3 End-to-end

One `dashboard_smoke_test.rs`: start real binary, wait for bind, hit `/health`, open WS, run one tool, read/write/undo one row, close. CI-gated on every PR.

### 10.4 CI

- Existing `cargo check --all-targets` + `cargo fmt --all` already covers base Rust.
- Add `cargo test -p engram_dashboard`.
- Separate `frontend` job: `pnpm install && pnpm test && pnpm build`.
- `dashboard_smoke_test` runs in release job.
- Memory mitigation: keep `CARGO_BUILD_JOBS=1` available for OOM scenarios.

### 10.5 Performance budgets (enforced)

- Overview lens p95 <200ms against 100k-node project.
- Graph render of 2k-node neighborhood p95 <800ms on commodity laptop.
- EQL `explain plan` <50ms.
- WS latency p95 <100ms on loopback.

---

## 11. Open questions

None remaining at design time. All scope decisions locked through brainstorming:

- Audience: developer using Engram.
- Delivery: big-bang, full design, ship complete.
- Write scope: full CRUD + typed query DSL + bulk export.
- Stack: axum + SvelteKit, single binary via `rust-embed`.

---

## 12. Risks

1. **Frontend build complexity in CI.** `build.rs` running pnpm is unusual. Mitigation: cache `node_modules` and `dist/` aggressively; allow `SKIP_FRONTEND_BUILD=1` to short-circuit if a prebuilt `dist/` exists.
2. **Cytoscape.js performance at 10k+ nodes.** Mitigation: the Graph lens only ever renders a neighborhood (`?depth=1..3`), never the whole graph. Overview preview uses a static snapshot.
3. **CRUD edits drifting from indexed truth.** Mitigation: reconcile banner + audit log + explicit warning in docs. Re-indexing is always the source of truth.
4. **Scope creep.** 8 lenses + full CRUD + EQL compiler is large. Big-bang delivery accepts this tradeoff explicitly. Writing-plans phase should break this into independently testable vertical slices even though we ship at the end.
