# Dashboard Plan 2 — Read Surface (Overview · Graph · Inspector · Activity)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the four read-only lenses — Overview, Graph explorer, Inspector, Activity log — end-to-end, plus the OpenAPI/typed-client pipeline. After this plan, a developer can: see project KPIs and hotspots on the landing page, pan/zoom the graph and inspect any node, search for a method and see its full Phase-38 access-layer roll-up, and watch live tool calls stream in the Activity log.

**Architecture:** Each lens = one `routes/*.rs` thin handler module + one SvelteKit route. Handlers compose over existing `engram_server::services::*`. OpenAPI schema emitted at build time via `utoipa`; a frontend build step generates TS types from it for end-to-end type safety.

**Tech Stack:** same as Plan 1 + `utoipa-swagger-ui` (dev only), `openapi-typescript`, `cytoscape` (JS package), `cytoscape-cose-bilkent`, `codemirror` (reserved for Plan 4), `@codemirror/lang-sql` (reserved).

**Spec sections:** §4.1 Overview, §4.2 Graph, §4.3 Inspector, §4.8 Activity, §5 API (the read endpoints).

**Prerequisite:** Plan 1 complete.

---

## File map

**Created (backend):**
- `crates/engram_dashboard/src/routes/overview.rs`
- `crates/engram_dashboard/src/routes/graph.rs`
- `crates/engram_dashboard/src/routes/inspector.rs`
- `crates/engram_dashboard/src/routes/activity.rs`
- `crates/engram_dashboard/src/routes/openapi.rs`
- `crates/engram_dashboard/src/activity_ring.rs` — bounded 10k ring buffer
- `crates/engram_dashboard/tests/overview_api_tests.rs`
- `crates/engram_dashboard/tests/graph_api_tests.rs`
- `crates/engram_dashboard/tests/inspector_api_tests.rs`
- `crates/engram_dashboard/tests/activity_api_tests.rs`
- `crates/engram_dashboard/tests/openapi_schema_tests.rs`
- `crates/engram_dashboard/tests/mcp_contract_tests.rs` — assert API ↔ MCP tool parity

**Created (frontend):**
- `web/src/lib/api/generated.ts` (generated from OpenAPI)
- `web/src/lib/components/KpiCard.svelte`
- `web/src/lib/components/GraphViewer.svelte` (Cytoscape wrapper)
- `web/src/lib/components/NodeDetail.svelte`
- `web/src/lib/components/MethodInfo.svelte`
- `web/src/lib/components/ActivityStream.svelte`
- `web/src/lib/components/SearchBox.svelte`
- `web/src/routes/+page.svelte` (real Overview — replaces stub)
- `web/src/routes/graph/+page.svelte` (real)
- `web/src/routes/inspector/+page.svelte` (real)
- `web/src/routes/activity/+page.svelte` (real)

**Modified:**
- `crates/engram_dashboard/src/server.rs` (merge new routers, register openapi)
- `crates/engram_dashboard/src/routes/mod.rs`
- `crates/engram_dashboard/web/package.json` (add `cytoscape`, `cytoscape-cose-bilkent`, `openapi-typescript` script)
- `crates/engram_dashboard/build.rs` (run openapi generator before `pnpm build`)

---

## Task 1 — OpenAPI schema emission

**Files:** `routes/openapi.rs`, `build.rs`, test.

- [ ] **Step 1:** Annotate existing Plan-1 handlers with `#[utoipa::path]` (non-breaking; compiles on top of the existing signatures). Example for `system::health`:

```rust
#[utoipa::path(get, path = "/api/v1/health", responses((status = 200, body = Health)))]
async fn health() -> Json<Health> { ... }
```

Similarly annotate `csrf`, `list_projects`.

- [ ] **Step 2:** Create `routes/openapi.rs`:

```rust
use utoipa::OpenApi;
use axum::{routing::get, Json, Router};
use engram_server::state::AppState;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::routes::system::health,
        crate::routes::system::csrf,
        crate::routes::projects::list_projects,
        // Later tasks add: overview, graph, inspector, activity endpoints
    ),
    components(schemas(/* populated as endpoints grow */))
)]
pub struct ApiDoc;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/openapi.json", get(spec))
}

async fn spec() -> Json<utoipa::openapi::OpenApi> { Json(ApiDoc::openapi()) }
```

- [ ] **Step 3:** Write test `tests/openapi_schema_tests.rs`:

```rust
mod common;
use common::build_test_appstate;
use engram_dashboard::{spawn_dashboard, DashboardConfig};

#[tokio::test]
async fn openapi_spec_has_health() {
    let h = build_test_appstate();
    let handle = spawn_dashboard(h.state.clone(), DashboardConfig::default()).await.unwrap();
    let spec: serde_json::Value = reqwest::get(format!("http://{}/api/v1/openapi.json", handle.bound_addr())).await.unwrap().json().await.unwrap();
    assert!(spec["paths"]["/api/v1/health"].is_object());
    handle.shutdown().await;
}
```

- [ ] **Step 4:** Extend `build.rs` to write the spec to disk at build time AND run `openapi-typescript`:

```rust
// addendum to build.rs
// After pnpm install (or unconditionally before `pnpm build`):
run(&web, "pnpm", &["exec", "openapi-typescript", "../src/openapi.json", "-o", "src/lib/api/generated.ts"]);
```

But the spec lives inside the Rust binary, not as a file. **Better approach:** add a small standalone binary target in `engram_dashboard/src/bin/emit_openapi.rs` that prints the JSON; `build.rs` invokes `cargo run --quiet -p engram_dashboard --bin emit_openapi > web/src/openapi.json` before the frontend build.

```rust
// crates/engram_dashboard/src/bin/emit_openapi.rs
use utoipa::OpenApi;
use engram_dashboard::routes::openapi::ApiDoc;
fn main() {
    println!("{}", ApiDoc::openapi().to_json().unwrap());
}
```

(Needs `pub use crate::routes::openapi::ApiDoc;` re-export in lib.)

- [ ] **Step 5:** `web/package.json` scripts:

```json
"generate:api": "openapi-typescript src/openapi.json -o src/lib/api/generated.ts"
```

- [ ] **Step 6:** Run test. Commit. `feat(dashboard): OpenAPI schema emission + TS codegen pipeline`.

---

## Task 2 — Activity ring buffer + `GET /api/v1/activity`

**Files:** `activity_ring.rs`, `routes/activity.rs`, tests.

- [ ] **Step 1:** `activity_ring.rs`:

```rust
use engram_core::DashboardEvent;
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;

pub struct ActivityRing {
    inner: Mutex<VecDeque<(u64, DashboardEvent)>>,
    capacity: usize,
    seq: std::sync::atomic::AtomicU64,
}

impl ActivityRing {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(VecDeque::with_capacity(capacity)), capacity, seq: Default::default() })
    }
    pub fn push(&self, ev: DashboardEvent) {
        let id = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut q = self.inner.lock().unwrap();
        if q.len() == self.capacity { q.pop_front(); }
        q.push_back((id, ev));
    }
    pub fn snapshot(&self, since_seq: Option<u64>, kind: Option<&str>, limit: usize) -> Vec<(u64, DashboardEvent)> {
        let q = self.inner.lock().unwrap();
        q.iter()
            .filter(|(s, _)| since_seq.map_or(true, |cutoff| *s > cutoff))
            .filter(|(_, ev)| kind.map_or(true, |k| event_tag(ev) == k))
            .take(limit)
            .cloned()
            .collect()
    }
}

fn event_tag(ev: &DashboardEvent) -> &'static str { /* same match as ws.rs — factor out */ ... }
```

- [ ] **Step 2:** Add `pub activity_ring: Arc<ActivityRing>` to `AppState` (init in `AppState::new` with capacity 10_000). Subscribe to `dashboard_events_tx` in a dedicated tokio task (spawned from `AppState::new` or `spawn_dashboard`) that calls `ring.push(ev)`.

- [ ] **Step 3:** `routes/activity.rs`:

```rust
use axum::{extract::{Query, State}, routing::get, Json, Router};
use engram_server::state::AppState;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct Params { pub since: Option<u64>, pub kind: Option<String>, pub limit: Option<usize> }

#[derive(Serialize, utoipa::ToSchema)]
pub struct ActivityItem { pub seq: u64, pub event: engram_core::DashboardEvent }

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/activity", get(list_activity))
}

#[utoipa::path(get, path = "/api/v1/activity", params(("since" = Option<u64>, Query), ("kind" = Option<String>, Query), ("limit" = Option<usize>, Query)))]
async fn list_activity(State(s): State<AppState>, Query(p): Query<Params>) -> Json<Vec<ActivityItem>> {
    let limit = p.limit.unwrap_or(100).min(1000);
    let items = s.activity_ring.snapshot(p.since, p.kind.as_deref(), limit)
        .into_iter().map(|(seq, event)| ActivityItem { seq, event }).collect();
    Json(items)
}
```

- [ ] **Step 4:** Test — send 3 events via bus, GET `/api/v1/activity`, expect 3 items.

- [ ] **Step 5:** Commit. `feat(dashboard): activity ring buffer + GET /api/v1/activity`.

---

## Task 3 — `GET /api/v1/graph/stats`

**Files:** `routes/graph.rs`.

- [ ] **Step 1:** Failing test:

```rust
// tests/graph_api_tests.rs
mod common;
use common::build_test_appstate;

#[tokio::test]
async fn graph_stats_returns_counts() {
    let h = common::build_test_appstate();
    let handle = engram_dashboard::spawn_dashboard(h.state.clone(), Default::default()).await.unwrap();
    let resp = reqwest::get(format!("http://{}/api/v1/graph/stats?project=default", handle.bound_addr())).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["nodes_by_type"].is_object());
    assert!(v["edges_by_kind"].is_object());
    handle.shutdown().await;
}
```

- [ ] **Step 2:** Implementation using existing `GraphStore::count_nodes_by_type` and `count_edges_by_kind` (confirmed present in `MEMORY.md`):

```rust
// crates/engram_dashboard/src/routes/graph.rs
use axum::{extract::{Query, State}, routing::get, Json, Router};
use engram_server::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Deserialize)]
pub struct ProjectQuery { pub project: String }

#[derive(Serialize, utoipa::ToSchema)]
pub struct GraphStats {
    pub nodes_by_type: HashMap<String, u64>,
    pub edges_by_kind: HashMap<String, u64>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/graph/stats", get(stats))
}

async fn stats(State(s): State<AppState>, Query(q): Query<ProjectQuery>) -> Result<Json<GraphStats>, crate::error::DashboardError> {
    let g = s.graph.clone();
    let pid = q.project.clone();
    let out = tokio::task::spawn_blocking(move || -> anyhow::Result<GraphStats> {
        Ok(GraphStats {
            nodes_by_type: g.count_nodes_by_type(&pid)?,
            edges_by_kind: g.count_edges_by_kind(&pid)?,
        })
    }).await.map_err(|e| crate::error::DashboardError::Internal(anyhow::anyhow!(e)))??;
    Ok(Json(out))
}
```

- [ ] **Step 3:** Run & commit. `feat(dashboard): GET /api/v1/graph/stats`.

---

## Task 4 — `GET /api/v1/graph/search`

**Files:** `routes/graph.rs` (extend).

- [ ] **Step 1:** Failing test — indexes a minimal project first (use the existing ingest flow from a test helper), then searches.

- [ ] **Step 2:** Handler delegates to `HybridSearchEngine::lexical_search` (per `MEMORY.md`). Use `state.projects.get(&project)?.search`.

```rust
#[derive(Deserialize)]
pub struct SearchQuery { pub project: String, pub q: String, pub kind: Option<String>, pub limit: Option<usize> }

#[derive(Serialize, utoipa::ToSchema)]
pub struct SearchHit { pub node_id: String, pub label: String, pub kind: String, pub score: f32 }

async fn search(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> Result<Json<Vec<SearchHit>>, crate::error::DashboardError> {
    let project = s.projects.get(&q.project).ok_or_else(|| crate::error::DashboardError::NotFound("project".into()))?.clone();
    let limit = q.limit.unwrap_or(25).min(200);
    let needle = q.q.clone();
    let filter_kind = q.kind.clone();
    let hits = tokio::task::spawn_blocking(move || project.search.lexical_search(&needle, limit))
        .await
        .map_err(|e| crate::error::DashboardError::Internal(anyhow::anyhow!(e)))??;
    let out = hits.into_iter()
        .filter(|h| filter_kind.as_ref().map_or(true, |k| h.kind == *k))
        .map(|h| SearchHit { node_id: h.node_id, label: h.label, kind: h.kind, score: h.score })
        .collect();
    Ok(Json(out))
}
```

Note: field names on `lexical_search` return shape may differ — consult `HybridSearchEngine`.

- [ ] **Step 3:** Run & commit. `feat(dashboard): GET /api/v1/graph/search`.

---

## Task 5 — `GET /api/v1/graph/node/:id` and `/neighbors/:id`

**Files:** `routes/graph.rs` (extend).

- [ ] **Step 1:** Failing tests for both.

- [ ] **Step 2:** Handler for node detail:

```rust
#[derive(Serialize, utoipa::ToSchema)]
pub struct NodeDetail {
    pub node: serde_json::Value,
    pub in_edges: Vec<EdgeSummary>,
    pub out_edges: Vec<EdgeSummary>,
}
#[derive(Serialize, utoipa::ToSchema)]
pub struct EdgeSummary { pub kind: String, pub other_id: String, pub other_label: String }

async fn node(State(s): State<AppState>, axum::extract::Path(id): axum::extract::Path<String>, Query(p): Query<ProjectQuery>) -> Result<Json<NodeDetail>, crate::error::DashboardError> {
    let g = s.graph.clone(); let pid = p.project.clone();
    let detail = tokio::task::spawn_blocking(move || -> anyhow::Result<NodeDetail> {
        let node = g.get_node(&pid, &id)?.ok_or_else(|| anyhow::anyhow!("no such node"))?;
        let in_edges = g.in_edges(&pid, &id)?.into_iter().map(|e| EdgeSummary{ kind: e.kind.to_string(), other_id: e.from_id, other_label: String::new() }).collect();
        let out_edges = g.out_edges(&pid, &id)?.into_iter().map(|e| EdgeSummary{ kind: e.kind.to_string(), other_id: e.to_id, other_label: String::new() }).collect();
        Ok(NodeDetail { node: serde_json::to_value(node)?, in_edges, out_edges })
    }).await.map_err(|e| crate::error::DashboardError::Internal(anyhow::anyhow!(e)))??;
    Ok(Json(detail))
}
```

Check exact `GraphStore` methods (`get_node`, `in_edges`, `out_edges`) — names per `MEMORY.md` are close to this; adapt.

- [ ] **Step 3:** Neighbors handler returns `{ nodes, edges }` for 1-hop neighborhood, capped at 500 total items. Implementation walks `out_edges` + `in_edges`, dedupes, and loads summaries.

- [ ] **Step 4:** Run & commit. `feat(dashboard): graph node/neighbors endpoints`.

---

## Task 6 — Inspector endpoints

**Files:** `routes/inspector.rs`.

- [ ] **Step 1:** Three endpoints that thinly wrap the existing Phase-38 access-layer tools:

```
GET /api/v1/inspect/method/:node_id   → delegates to get_method_info
GET /api/v1/inspect/file/:doc_id      → file summary (call existing aggregator or compose)
GET /api/v1/inspect/page/:doc_id      → delegates to get_page_context
```

Find and import the Phase-38 service functions from `engram_server::services` or `engram_server::handlers::access_layer_tools`. If they're currently *only* exposed as MCP tool handlers (taking `Parameters<T>`), extract the core logic into a service fn (pure `(state, args) -> Result<T>`) and call from both places. Follow the pattern in `services/` already used by other tools.

- [ ] **Step 2:** Failing test for `/inspect/method/:id` — small indexed project, a known method `node_id`, assert roll-up contains expected fields.

- [ ] **Step 3:** Implementation — each route is ~20 lines (deserialize params, spawn_blocking into service fn, map result to JSON).

- [ ] **Step 4:** Run & commit. `feat(dashboard): inspector endpoints (method/file/page)`.

---

## Task 7 — Overview endpoint

**Files:** `routes/overview.rs`.

- [ ] **Step 1:** Failing test: GET `/api/v1/overview?project=X` returns JSON with `kpis`, `hotspots`, `recent_calls`, `graph_preview`.

- [ ] **Step 2:** Implementation — composes:
  - KPIs: migration progress % (from `MigrationProgressStore`), coverage % (from `coverage_service`), anti-pattern count (graph query: nodes with `AntiPattern` edges), edit-safety-red count (query `check_edit_safety` output cache, OR compute over top-100 centrality nodes; start with a simpler "count of files with ≥1 RED verdict in last scan").
  - Hotspots top-10: nodes ordered by centrality descending + edit-safety verdict.
  - Recent calls: `activity_ring.snapshot(None, Some("tool_call_completed"), 20)`.
  - Graph preview: `count_nodes_by_type` + top-20 centrality nodes.

All work done in `spawn_blocking` since it hits Redb.

```rust
// sketch
#[derive(Serialize, utoipa::ToSchema)]
pub struct Overview {
    pub project_id: String,
    pub kpis: Kpis,
    pub hotspots: Vec<Hotspot>,
    pub recent_calls: Vec<serde_json::Value>,
    pub graph_preview: GraphPreview,
}
```

- [ ] **Step 3:** Run & commit. `feat(dashboard): GET /api/v1/overview aggregator`.

---

## Task 8 — MCP contract tests (drift guard)

**Files:** `tests/mcp_contract_tests.rs`.

- [ ] **Step 1:** For each of these pairs, assert the dashboard endpoint returns the same data shape as the MCP tool:

| MCP tool | Dashboard endpoint |
|---|---|
| `get_method_info` | `GET /api/v1/inspect/method/:id` |
| `get_page_context` | `GET /api/v1/inspect/page/:id` |
| `get_graph_statistics` (or whatever the existing one is) | `GET /api/v1/graph/stats` |

- [ ] **Step 2:** Each test: build an in-memory AppState with a tiny fixture, call both paths, compare canonical JSON. Difference on any key = fail.

- [ ] **Step 3:** Commit. `test(dashboard): MCP ↔ dashboard contract parity tests`.

---

## Task 9 — Frontend: KPI card component

**Files:** `web/src/lib/components/KpiCard.svelte`.

- [ ] **Step 1:**

```svelte
<script lang="ts">
  export let label: string;
  export let value: string | number;
  export let delta: string | null = null;
  export let tone: 'neutral' | 'good' | 'bad' = 'neutral';
  $: toneClass = tone === 'good' ? 'text-green-400' : tone === 'bad' ? 'text-red-400' : 'text-text-dim';
</script>

<div class="bg-bg-card border border-line rounded p-3">
  <div class="text-xs uppercase text-text-dim">{label}</div>
  <div class="text-2xl font-bold text-white">{value}</div>
  {#if delta}<div class="text-xs {toneClass}">{delta}</div>{/if}
</div>
```

- [ ] **Step 2:** vitest smoke test. Commit. `feat(dashboard-ui): KpiCard component`.

---

## Task 10 — Frontend: Overview page

**Files:** `web/src/routes/+page.svelte` (replace stub).

- [ ] **Step 1:** Fetch `/api/v1/overview?project=<currentProjectId>` on mount, render KPI row + hotspot list + recent calls + graph preview thumbnail. Mirror the spec §4.1 layout. Use `KpiCard` × 4.

- [ ] **Step 2:** Commit. `feat(dashboard-ui): Overview page`.

---

## Task 11 — Frontend: GraphViewer component

**Files:** `web/src/lib/components/GraphViewer.svelte`.

- [ ] **Step 1:** `pnpm add cytoscape cytoscape-cose-bilkent`.

- [ ] **Step 2:**

```svelte
<script lang="ts">
  import cytoscape from 'cytoscape';
  import coseBilkent from 'cytoscape-cose-bilkent';
  import { onMount, onDestroy } from 'svelte';

  cytoscape.use(coseBilkent);

  export let nodes: Array<{id:string; label:string; kind:string}> = [];
  export let edges: Array<{source:string; target:string; kind:string}> = [];
  export let onSelect: ((id: string) => void) | null = null;

  let el: HTMLDivElement;
  let cy: cytoscape.Core | null = null;

  const styleByKind: Record<string,string> = {
    function: '#4f8cff', class: '#a78bfa', file: '#6b7280',
    db_table: '#4ade80', control: '#facc15', default: '#9ca3af',
  };

  onMount(() => {
    cy = cytoscape({
      container: el,
      elements: [
        ...nodes.map(n => ({ data: { id: n.id, label: n.label, kind: n.kind } })),
        ...edges.map(e => ({ data: { source: e.source, target: e.target, kind: e.kind } })),
      ],
      style: [
        { selector: 'node', style: { 'background-color': (ele: any) => styleByKind[ele.data('kind')] ?? styleByKind.default, 'label': 'data(label)', 'color': '#d0d4dc', 'font-size': '9px' } },
        { selector: 'edge', style: { 'line-color': '#2a3344', 'width': 1, 'curve-style': 'bezier' } },
      ],
      layout: { name: 'cose-bilkent', animate: false },
    });
    if (onSelect) cy.on('tap', 'node', (e) => onSelect!(e.target.id()));
  });
  onDestroy(() => cy?.destroy());

  $: if (cy) {
    cy.elements().remove();
    cy.add([
      ...nodes.map(n => ({ data: { id: n.id, label: n.label, kind: n.kind } })),
      ...edges.map(e => ({ data: { source: e.source, target: e.target, kind: e.kind } })),
    ]);
    cy.layout({ name: 'cose-bilkent', animate: false }).run();
  }
</script>

<div bind:this={el} class="w-full h-full"></div>
```

- [ ] **Step 3:** Commit. `feat(dashboard-ui): GraphViewer component (Cytoscape)`.

---

## Task 12 — Frontend: Graph explorer page

**Files:** `web/src/routes/graph/+page.svelte`.

- [ ] **Step 1:** Page layout: left filter panel (node kinds, edge kinds, search input) · center `GraphViewer` · right `NodeDetail` pane.

- [ ] **Step 2:** On search, call `/api/v1/graph/search`, pick first result, fetch neighbors via `/neighbors/:id?depth=1`, pass to `GraphViewer`. On node click, fetch `/node/:id`, render in `NodeDetail`.

- [ ] **Step 3:** Commit. `feat(dashboard-ui): Graph explorer page`.

---

## Task 13 — Frontend: NodeDetail + MethodInfo components

**Files:** `web/src/lib/components/NodeDetail.svelte`, `MethodInfo.svelte`.

- [ ] **Step 1:** `NodeDetail`: renders node props (label, kind, file_path, line range), in/out edge lists with click-through.
- [ ] **Step 2:** `MethodInfo`: renders the inspector roll-up shape (callers, callees, SQL, state, anti-patterns, tests, blast radius, edit-safety). Tabs component for the sections.
- [ ] **Step 3:** Commit. `feat(dashboard-ui): NodeDetail + MethodInfo components`.

---

## Task 14 — Frontend: Inspector page

**Files:** `web/src/routes/inspector/+page.svelte`.

- [ ] **Step 1:** Page layout: top `SearchBox` · left source-code pane (read-only CodeMirror showing the method body) · right `MethodInfo` tabs.

- [ ] **Step 2:** On search submit, fetch `/api/v1/graph/search?kind=function`, pick first, fetch `/api/v1/inspect/method/:id`, render. URL-share: `?node=<id>` restores state on load.

- [ ] **Step 3:** Commit. `feat(dashboard-ui): Inspector page`.

---

## Task 15 — Frontend: ActivityStream component + page

**Files:** `web/src/lib/components/ActivityStream.svelte`, `web/src/routes/activity/+page.svelte`.

- [ ] **Step 1:** `ActivityStream` subscribes to `bus` topics `tool_call_*`, `job_*`, `index_delta`, `adp_verdict`, `activity_event`. Renders a rolling list (cap 500 items client-side, newest on top). Per-event styling by type.

- [ ] **Step 2:** Activity page: mounts the stream plus filter controls (kind dropdown, outcome dropdown, pause/resume) and a "load historical" button that calls `GET /api/v1/activity?since=…` to prefill the buffer on first load.

- [ ] **Step 3:** Commit. `feat(dashboard-ui): Activity log page + live stream`.

---

## Task 16 — Performance budgets enforced as tests

**Files:** `tests/perf_budgets_tests.rs`.

- [ ] **Step 1:** Build a large fixture project (use an existing test-fixture builder if present; otherwise generate synthetic nodes: 100k). Measure:
  - Overview endpoint wall-clock.
  - Graph `/neighbors?depth=1` for a 2k-neighborhood node.
  - OpenAPI spec render.

- [ ] **Step 2:** Assertions allow 3× slack over budgets in CI (flaky-machine allowance):
  - Overview p95 < 600ms (budget: 200ms).
  - Neighbors < 2400ms (budget: 800ms).
  - OpenAPI < 150ms (budget: 50ms).

Actual budgets in production ship as doc; these tests catch regressions.

- [ ] **Step 3:** Mark `#[ignore]` by default, run in a dedicated CI job `cargo test -- --ignored perf_`.

- [ ] **Step 4:** Commit. `test(dashboard): performance budget regression tests (ignored by default)`.

---

## Task 17 — Self-review

- [ ] **Step 1:** Grep the plan for `TBD`, `TODO`, `fill in`, `adapt to the real`. Any occurrences: replace with either concrete code or an explicit "verify and adapt in first task" note like we already have on `ProjectRecord`.
- [ ] **Step 2:** Verify every endpoint in §5 marked "read" in the spec has a backing task. If not, add a task.
- [ ] **Step 3:** Verify every task ends with a commit step.
- [ ] **Step 4:** No step — this is a review gate before executing.

**Completion gate:** All four read lenses render real data end-to-end against an indexed project. Contract tests pass. OpenAPI schema generates TS types consumed by the frontend without drift errors.
