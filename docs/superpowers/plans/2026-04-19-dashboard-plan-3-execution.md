# Dashboard Plan 3 — Execution Surface (Tool runner · Migration · Business logic)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the three execution-oriented lenses — Tool runner (all MCP tools as forms), Migration cockpit (Phase 17/31/32/33 output rendered), Business logic (LLM-streamed Q&A). After this plan, a developer can run any of the 99+ MCP tools from the UI, see the full-project migration report as an interactive document, and query business logic in plain English with a streamed response.

**Architecture:** Tool runner does **not** re-implement tool logic — it calls the existing MCP dispatcher through a thin internal shim. Forms are auto-generated from each tool's JSON-schema request struct (using `schemars`, already in the workspace for `rmcp`). Migration lens renders existing service outputs. Business logic lens proxies `query_business_logic` with SSE streaming for token-by-token display.

**Tech Stack:** additions: `async-stream` (for SSE), `@json-editor/json-editor` or custom Svelte form generator. For the SSE client use the browser's native `EventSource`-style handling via `fetch` + `ReadableStream` (`EventSource` doesn't support POST bodies; we roll our own parser).

**Spec sections:** §4.4 Tool runner, §4.5 Migration, §4.6 Business logic, §5 execution endpoints.

**Prerequisite:** Plans 1 and 2 complete.

---

## File map

**Created (backend):**
- `crates/engram_dashboard/src/routes/tools.rs`
- `crates/engram_dashboard/src/routes/migration.rs`
- `crates/engram_dashboard/src/routes/business_logic.rs`
- `crates/engram_dashboard/src/tool_catalog.rs` — introspects rmcp tool registry
- `crates/engram_dashboard/src/sse.rs` — SSE response helper
- `crates/engram_dashboard/tests/tools_api_tests.rs`
- `crates/engram_dashboard/tests/migration_api_tests.rs`
- `crates/engram_dashboard/tests/business_logic_api_tests.rs`
- `crates/engram_dashboard/tests/tool_history_tests.rs`

**Created (frontend):**
- `web/src/lib/components/JsonSchemaForm.svelte`
- `web/src/lib/components/ToolResult.svelte`
- `web/src/lib/components/MigrationReport.svelte`
- `web/src/lib/components/Dossier.svelte`
- `web/src/lib/components/CoverageHeatmap.svelte`
- `web/src/lib/components/BlQueryBox.svelte`
- `web/src/lib/components/ConceptMap.svelte`
- `web/src/routes/tools/+page.svelte` (real)
- `web/src/routes/tools/[name]/+page.svelte` (per-tool detail/form/history)
- `web/src/routes/migration/+page.svelte` (real)
- `web/src/routes/migration/dossier/[doc_id]/+page.svelte`
- `web/src/routes/business-logic/+page.svelte` (real)

**Modified:**
- `crates/engram_dashboard/src/server.rs` (merge routers)
- `crates/engram_dashboard/src/routes/mod.rs`

---

## Task 1 — Tool catalog: introspect every registered MCP tool

**Files:** `src/tool_catalog.rs`, test.

- [ ] **Step 1:** In `engram_server`, locate where `rmcp` tools are registered (likely a macro invocation in `tools.rs` listing every handler). Each tool already has a JSON schema via `schemars` on its `Parameters<T>` request struct. Goal: produce `Vec<ToolDescriptor>` at runtime.

- [ ] **Step 2:** Add an associated function on the tool-dispatch type (or a free function in `engram_server`) that returns `Vec<ToolDescriptor>`:

```rust
// crates/engram_server/src/tool_catalog.rs (new)
use schemars::JsonSchema;

pub struct ToolDescriptor {
    pub name: &'static str,
    pub group: &'static str,
    pub description: &'static str,
    pub request_schema: serde_json::Value,
    pub response_schema: Option<serde_json::Value>,
}

pub fn all_tools() -> Vec<ToolDescriptor> { /* one entry per registered tool */ }
```

Each tool's entry is filled by calling `schemars::schema_for!(RequestStruct)`. The existing `capabilities.rs` file already enumerates the 99+ tools — mirror that list. This is mechanical.

- [ ] **Step 3:** Dashboard side — `tool_catalog.rs`:

```rust
// crates/engram_dashboard/src/tool_catalog.rs
pub use engram_server::tool_catalog::{all_tools, ToolDescriptor};
```

- [ ] **Step 4:** Test — every tool in `capabilities.rs` has a descriptor with non-empty `request_schema`.

- [ ] **Step 5:** Commit. `feat(dashboard): tool catalog — introspect all MCP tool schemas`.

---

## Task 2 — `GET /api/v1/tools` and `/api/v1/tools/:name`

**Files:** `routes/tools.rs`.

- [ ] **Step 1:** Failing test — `/api/v1/tools` returns an array, `/api/v1/tools/:name` returns the descriptor, unknown name → 404.

- [ ] **Step 2:**

```rust
// crates/engram_dashboard/src/routes/tools.rs
use axum::{extract::{Path, State}, routing::get, Json, Router};
use engram_server::state::AppState;
use serde::Serialize;

#[derive(Serialize, utoipa::ToSchema)]
pub struct ToolSummary { pub name: String, pub group: String, pub description: String }

#[derive(Serialize, utoipa::ToSchema)]
pub struct ToolDetail { pub name: String, pub group: String, pub description: String, pub request_schema: serde_json::Value }

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/tools", get(list_tools))
        .route("/api/v1/tools/:name", get(tool_detail))
}

async fn list_tools() -> Json<Vec<ToolSummary>> {
    Json(crate::tool_catalog::all_tools().into_iter().map(|d| ToolSummary {
        name: d.name.into(), group: d.group.into(), description: d.description.into(),
    }).collect())
}

async fn tool_detail(Path(name): Path<String>) -> Result<Json<ToolDetail>, crate::error::DashboardError> {
    crate::tool_catalog::all_tools().into_iter().find(|d| d.name == name)
        .map(|d| Json(ToolDetail { name: d.name.into(), group: d.group.into(), description: d.description.into(), request_schema: d.request_schema }))
        .ok_or(crate::error::DashboardError::NotFound(name))
}
```

- [ ] **Step 3:** Commit. `feat(dashboard): GET /api/v1/tools{,/:name}`.

---

## Task 3 — `POST /api/v1/tools/:name/run`

**Files:** `routes/tools.rs` (extend).

- [ ] **Step 1:** Design: the handler dispatches by tool name to the same function the MCP dispatcher calls. Two choices:
  - **A.** Add an internal `dispatch_tool(name, params, state) -> Result<serde_json::Value>` in `engram_server` that mirrors the rmcp macro dispatch. Dashboard calls that.
  - **B.** Construct an in-process rmcp client and round-trip through the actual MCP server surface.

Choice **A** is simpler and we recommend it.

- [ ] **Step 2:** Add `engram_server::tool_catalog::dispatch_tool(&name, params: serde_json::Value, state: &AppState) -> anyhow::Result<serde_json::Value>`. Implementation: a big match on tool name; each arm deserializes `params` into the tool's request struct and calls the existing handler function, serializing the response back to JSON. Share the match with the rmcp registration if possible (a macro emitting both).

- [ ] **Step 3:** Failing test — POST to `/api/v1/tools/get_method_info/run` with a known node id, expect 200 JSON body.

- [ ] **Step 4:** Handler:

```rust
use axum::{extract::{Path, State}, http::StatusCode, Json};

#[derive(serde::Deserialize)]
pub struct RunBody { pub params: serde_json::Value }

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct RunResult { pub request_id: String, pub duration_ms: u64, pub result: serde_json::Value }

async fn run_tool(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RunBody>,
) -> Result<Json<RunResult>, crate::error::DashboardError> {
    let req_id = uuid::Uuid::new_v4().simple().to_string();
    let start = std::time::Instant::now();
    let params_hash = blake3::hash(&serde_json::to_vec(&body.params).unwrap_or_default()).to_hex()[..16].to_string();

    s.dashboard_events_tx.send(engram_core::DashboardEvent::ToolCallStarted {
        request_id: req_id.clone(), tool: name.clone(), params_hash,
        project_id: body.params.get("project_id").and_then(|v| v.as_str()).map(String::from),
        ts: engram_core::dashboard_events::now_ts(),
    }).ok();

    let result = engram_server::tool_catalog::dispatch_tool(&name, body.params, &s).await
        .map_err(|e| crate::error::DashboardError::Internal(anyhow::anyhow!(e)))?;

    let dur = start.elapsed().as_millis() as u64;
    s.dashboard_events_tx.send(engram_core::DashboardEvent::ToolCallCompleted {
        request_id: req_id.clone(), tool: name, duration_ms: dur, outcome: "ok".into(),
        result_size: serde_json::to_vec(&result).map(|v| v.len()).unwrap_or(0), ts: engram_core::dashboard_events::now_ts(),
    }).ok();

    Ok(Json(RunResult { request_id: req_id, duration_ms: dur, result }))
}
```

- [ ] **Step 5:** Register route with the `require_csrf` middleware already in place from Plan 1 Task 6.

- [ ] **Step 6:** Add concurrency cap (10 simultaneous runs) via a shared `Arc<Semaphore>` on `AppState` or module-local `OnceLock`. Acquire in `run_tool`, release on drop.

- [ ] **Step 7:** Commit. `feat(dashboard): POST /api/v1/tools/:name/run with event emission`.

---

## Task 4 — Tool execution history + favorites + pinned params

**Files:** `routes/tools.rs` (extend).

- [ ] **Step 1:** Add three Redb tables in a new `ToolHistoryStore` module (or extend `Registry`): `tool_history` (seq-keyed), `tool_favorites` (set of names), `tool_pinned_params` (name → preset JSON).

- [ ] **Step 2:** Wire endpoints:

```
GET  /api/v1/tools/history?cursor=&tool=
POST /api/v1/tools/:name/favorite     → toggle
POST /api/v1/tools/:name/pin-params   → body: { preset_name, params }
GET  /api/v1/tools/:name/pinned       → [{ preset_name, params, saved_at }]
```

Every successful tool run appends to history (params **hashed** only, not stored — history stores `params_hash + result_hash + timestamp + duration + outcome`). Privacy + size.

- [ ] **Step 3:** Tests for each.

- [ ] **Step 4:** Commit. `feat(dashboard): tool history + favorites + pinned params`.

---

## Task 5 — Migration endpoints

**Files:** `routes/migration.rs`.

- [ ] **Step 1:** Thin handlers over existing services (paths from `MEMORY.md`):

```
GET  /api/v1/migration/report?project=X            → full_project_migration_service output
GET  /api/v1/migration/dossier/:doc_id?project=X   → dossier_service output
GET  /api/v1/migration/order?project=X             → migration_order_service output
GET  /api/v1/migration/coverage?project=X          → coverage_service output
GET  /api/v1/migration/characterization-tests?project=X → characterization_test_service output
POST /api/v1/migration/rollout                     → body: { project, action: "enable"|"kill-switch" }
```

Each handler delegates to the service function directly. Add `utoipa::path` annotations.

- [ ] **Step 2:** Failing tests — build a minimal WebForms fixture (existing tests likely have one; reuse), index it, call each endpoint, assert a known field.

- [ ] **Step 3:** `POST /migration/rollout` flips the kill-switch atomic on `AppState` (`adp_kill_switch`) and persists to registry — reuse the existing mutation function if present.

- [ ] **Step 4:** Commit. `feat(dashboard): migration endpoints (report, dossier, order, coverage, tests, rollout)`.

---

## Task 6 — Business logic endpoints

**Files:** `routes/business_logic.rs`, `src/sse.rs`.

- [ ] **Step 1:** `sse.rs` helper:

```rust
use axum::{response::{sse::{Event, Sse}, IntoResponse}, Json};
use futures::stream::Stream;

pub fn sse_stream<S: Stream<Item = Result<Event, std::convert::Infallible>> + Send + 'static>(s: S) -> Sse<S> {
    Sse::new(s).keep_alive(axum::response::sse::KeepAlive::default())
}
```

- [ ] **Step 2:** Endpoints:

```
GET  /api/v1/bl/concepts?project=X              → domain concept map
POST /api/v1/bl/query                           → SSE stream of tokens + final summary
GET  /api/v1/bl/summary/:node_id?project=X      → curated summary
PATCH /api/v1/bl/summary/:node_id                → edit
```

Concepts, summary GET/PATCH are synchronous service calls over `business_logic_service`.

Query POST is SSE. Implementation:

```rust
#[derive(serde::Deserialize)]
pub struct BlQuery { pub project: String, pub question: String }

async fn query(State(s): State<AppState>, Json(body): Json<BlQuery>) -> impl IntoResponse {
    use async_stream::stream;
    use axum::response::sse::Event;
    let stream = stream! {
        // business_logic_service::stream_answer returns an async stream of (token_str | final_json).
        // If the existing service is not yet streaming, we still SSE-wrap by emitting one "chunk" frame
        // per paragraph and a final "done" frame with citations.
        match engram_server::services::business_logic_service::stream_answer(&s, &body.project, &body.question).await {
            Ok(mut rx) => {
                while let Some(chunk) = rx.recv().await {
                    yield Ok::<_, std::convert::Infallible>(Event::default().event("chunk").data(chunk.text));
                }
                yield Ok(Event::default().event("done").data(serde_json::to_string(&rx.summary()).unwrap_or_default()));
            }
            Err(e) => {
                yield Ok(Event::default().event("error").data(e.to_string()));
            }
        }
    };
    crate::sse::sse_stream(stream)
}
```

**Adapt if `stream_answer` does not exist yet** — add a small synchronous wrapper that returns one final chunk until streaming is implemented in `business_logic_service`. Spec allows this fallback.

- [ ] **Step 3:** Test — POST to `/bl/query` with a mock project, read the SSE response, assert at least one `chunk` frame and one `done` frame.

- [ ] **Step 4:** Commit. `feat(dashboard): business logic endpoints (concepts + query SSE + summary CRUD)`.

---

## Task 7 — Frontend: JsonSchemaForm component

**Files:** `web/src/lib/components/JsonSchemaForm.svelte`.

- [ ] **Step 1:** A recursive Svelte component that takes a JSON schema + value and renders inputs. Supported types: `string`, `integer`, `number`, `boolean`, `array<primitive>`, `object`. Respects `enum`, `minLength`, `default`, `description` for tooltips. Unknown shapes fall back to a monospace textarea that accepts JSON.

Representative sketch (core logic; full component is ~200 lines):

```svelte
<script lang="ts">
  export let schema: any;
  export let value: any;
  export let onChange: (v: any) => void;

  function update(key: string | number, v: any) {
    if (Array.isArray(value)) { const next = [...value]; next[key as number] = v; onChange(next); }
    else { onChange({ ...(value ?? {}), [key]: v }); }
  }
</script>

{#if schema.type === 'object' && schema.properties}
  {#each Object.entries(schema.properties) as [k, sub]}
    <label class="block mb-2">
      <span class="text-xs text-text-dim">{k}{schema.required?.includes(k) ? ' *' : ''}</span>
      <svelte:self schema={sub} value={value?.[k]} onChange={(v) => update(k, v)} />
    </label>
  {/each}
{:else if schema.type === 'string' && schema.enum}
  <select class="bg-bg-card border border-line rounded p-1 text-xs" bind:value on:change={(e) => onChange((e.target as HTMLSelectElement).value)}>
    {#each schema.enum as opt}<option>{opt}</option>{/each}
  </select>
{:else if schema.type === 'boolean'}
  <input type="checkbox" checked={!!value} on:change={(e) => onChange((e.target as HTMLInputElement).checked)} />
{:else if schema.type === 'integer' || schema.type === 'number'}
  <input type="number" class="bg-bg-card border border-line rounded p-1 text-xs w-full" value={value ?? ''} on:input={(e) => onChange(Number((e.target as HTMLInputElement).value))} />
{:else}
  <input type="text" class="bg-bg-card border border-line rounded p-1 text-xs w-full" value={value ?? ''} on:input={(e) => onChange((e.target as HTMLInputElement).value)} />
{/if}
```

- [ ] **Step 2:** Vitest tests: primitive render, object nesting, enum dropdown, value change propagation.

- [ ] **Step 3:** Commit. `feat(dashboard-ui): JsonSchemaForm recursive component`.

---

## Task 8 — Frontend: Tool runner index page

**Files:** `web/src/routes/tools/+page.svelte`.

- [ ] **Step 1:** Fetch `/api/v1/tools`, render grouped list (group: Search, Graph, Migration, Inspection, ADP, ML, Index, Memory, …). Each entry links to `/tools/[name]`.

- [ ] **Step 2:** Sidebar filter: text search + favorites-only toggle.

- [ ] **Step 3:** Commit. `feat(dashboard-ui): Tool runner index`.

---

## Task 9 — Frontend: Per-tool page

**Files:** `web/src/routes/tools/[name]/+page.svelte`.

- [ ] **Step 1:** Fetch `/api/v1/tools/[name]` on load. Render description + `JsonSchemaForm` bound to a `params` state. "Run" button POSTs to `/tools/[name]/run`, renders `ToolResult`. History list under the form (`/api/v1/tools/history?tool=[name]`). "★ favorite" toggle. "Pin params" modal saves the current form state as a named preset.

- [ ] **Step 2:** "Send to Inspector" pipe — if result contains a `node_id` or `doc_id`, show a button that navigates to `/inspector?node=...`.

- [ ] **Step 3:** Commit. `feat(dashboard-ui): Tool runner per-tool page`.

---

## Task 10 — Frontend: ToolResult component

**Files:** `web/src/lib/components/ToolResult.svelte`.

- [ ] **Step 1:** Renders a JSON tree with collapse/expand, plus a "raw" tab that shows pretty-printed JSON. Copy-to-clipboard button. Tool-specific formatted views for the most common shapes:
  - `get_method_info` → same renderer as MethodInfo (reuse).
  - `analyze_blast_radius` → colored gauge + caller table.
  - anything else → JSON tree.

Detection: switch on the tool's name passed as a prop.

- [ ] **Step 2:** Commit. `feat(dashboard-ui): ToolResult with tool-specific views`.

---

## Task 11 — Frontend: Migration page + components

**Files:** `web/src/routes/migration/+page.svelte`, `MigrationReport.svelte`, `CoverageHeatmap.svelte`.

- [ ] **Step 1:** Page layout: top tabs — "Report" · "Order" · "Coverage" · "Characterization tests" · "Rollout". Each tab mounts the matching component.
- [ ] **Step 2:** `MigrationReport.svelte` — renders `GET /api/v1/migration/report` with collapsible sections matching the Phase 32/33 output structure. Each file listed links to `/migration/dossier/[doc_id]`.
- [ ] **Step 3:** `CoverageHeatmap.svelte` — SVG grid, color scale green→yellow→red, hover tooltip with filename + coverage %.
- [ ] **Step 4:** Rollout tab — big red "Activate kill-switch" button with a confirmation modal; POSTs to `/migration/rollout`.
- [ ] **Step 5:** Commit. `feat(dashboard-ui): Migration page (report + order + coverage + rollout)`.

---

## Task 12 — Frontend: Dossier page

**Files:** `web/src/routes/migration/dossier/[doc_id]/+page.svelte`, `Dossier.svelte`.

- [ ] **Step 1:** Fetch `/api/v1/migration/dossier/[doc_id]`, render the markdown dossier using a lightweight renderer (`marked` or similar; add as dep). Side panel shows related links: parent page, callers, anti-patterns.
- [ ] **Step 2:** "Copy to clipboard" button to hand the dossier to Claude.
- [ ] **Step 3:** Commit. `feat(dashboard-ui): Dossier page`.

---

## Task 13 — Frontend: Business logic page

**Files:** `web/src/routes/business-logic/+page.svelte`, `BlQueryBox.svelte`, `ConceptMap.svelte`.

- [ ] **Step 1:** `BlQueryBox.svelte` — large textarea + "Ask" button. On submit:
  - POSTs to `/api/v1/bl/query` with `Accept: text/event-stream`.
  - Parses the SSE stream (using `fetch` + `ReadableStream` since `EventSource` doesn't support POST):

```typescript
async function* sseStream(response: Response) {
  const reader = response.body!.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  while (true) {
    const { value, done } = await reader.read();
    if (done) return;
    buf += decoder.decode(value, { stream: true });
    let i;
    while ((i = buf.indexOf('\n\n')) >= 0) {
      yield buf.slice(0, i); buf = buf.slice(i + 2);
    }
  }
}
```

  - Appends each `chunk` event's data to the rendered answer. On `done`, displays citations + confidence.

- [ ] **Step 2:** `ConceptMap.svelte` — fetches `/api/v1/bl/concepts`, renders as a small graph (reuse `GraphViewer`) or a tree, depending on shape.

- [ ] **Step 3:** Page layout: top ConceptMap (collapsible) · middle BlQueryBox · below, recent queries history.

- [ ] **Step 4:** Commit. `feat(dashboard-ui): Business logic page (query + concepts + streamed answers)`.

---

## Task 14 — Integration: tool runner emits live events

**Files:** confirm Plan 1 Task 18 wiring covers dashboard-initiated runs. If not, extend.

- [ ] **Step 1:** Run: open Activity lens, in a second window open Tool runner, run `get_method_info`. Verify events stream in Activity.

- [ ] **Step 2:** Add automated test — subscribe WS to `tool_call_*`, call `POST /tools/get_method_info/run`, assert both started+completed events fire with matching `request_id`.

- [ ] **Step 3:** Commit. `test(dashboard): tool runner emits live events end-to-end`.

---

## Task 15 — Self-review

- [ ] **Step 1:** Verify `dispatch_tool` covers every tool name listed in `capabilities.rs`. If a macro emits both rmcp registration and `dispatch_tool`, a compile-time check asserts parity.
- [ ] **Step 2:** Every `utoipa::path` annotation added to new handlers → pick them up in `ApiDoc`.
- [ ] **Step 3:** Confirm no task ships a UI for a write operation without CSRF wiring (Plan 1 Task 6 middleware covers this, but verify new POST routes are *not* exempted).

**Completion gate:** Every tool listed in `capabilities.rs` is invokable from the UI, returns a result, and emits WS events. Migration lens renders the full Phase 32/33 output for a real indexed project. Business logic lens streams an answer.
