# Dashboard Plan 4 — Data Browser + EQL + CRUD + Audit/Undo

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the Data browser lens end-to-end: a typed table picker, a visual query builder stacked over a raw EQL editor with bidirectional sync, a result grid with inline edit/delete, bulk export (CSV/JSON/Parquet), full CRUD with a confirmation + audit-log + undo flow, and a reconcile action for drift-from-source.

**Architecture:** A new `engram_dashboard::query` module parses, type-checks, and plans EQL, then executes against `GraphStore` / `DocStore` / `Registry` tables. All writes go through the existing store validation layers. A new Redb table `dashboard_ops` in a dedicated audit DB records every write operation. CRUD endpoints are project-scoped and CSRF-protected.

**Tech Stack:** additions: `nom` or `chumsky` for EQL parsing (`chumsky` recommended — better errors, already pulls a clean grammar definition), `csv` crate for export, `arrow`+`parquet` (arrow already workspace-pinned) for Parquet. Frontend: `@codemirror/*` packages for the EQL editor with custom language support.

**Spec sections:** §4.7 Data browser, §5 data endpoints, §6 EQL.

**Prerequisite:** Plans 1–3 complete.

---

## File map

**Created (backend):**
- `crates/engram_dashboard/src/query/mod.rs`
- `crates/engram_dashboard/src/query/ast.rs`
- `crates/engram_dashboard/src/query/parser.rs`
- `crates/engram_dashboard/src/query/typecheck.rs`
- `crates/engram_dashboard/src/query/plan.rs`
- `crates/engram_dashboard/src/query/exec.rs`
- `crates/engram_dashboard/src/query/schema.rs` — table-metadata registry
- `crates/engram_dashboard/src/audit.rs` — `dashboard_ops` Redb store
- `crates/engram_dashboard/src/routes/data.rs`
- `crates/engram_dashboard/src/routes/query.rs`
- `crates/engram_dashboard/src/routes/export.rs`
- `crates/engram_dashboard/tests/eql_parser_tests.rs`
- `crates/engram_dashboard/tests/eql_planner_tests.rs`
- `crates/engram_dashboard/tests/eql_exec_tests.rs`
- `crates/engram_dashboard/tests/data_crud_tests.rs`
- `crates/engram_dashboard/tests/audit_undo_tests.rs`
- `crates/engram_dashboard/tests/reconcile_tests.rs`
- `crates/engram_dashboard/tests/eql_proptest.rs`

**Created (frontend):**
- `web/src/lib/components/eql/editor.ts` (CodeMirror language)
- `web/src/lib/components/eql/EqlEditor.svelte`
- `web/src/lib/components/eql/QueryBuilder.svelte`
- `web/src/lib/components/eql/ResultTable.svelte`
- `web/src/lib/components/eql/ConfirmModal.svelte`
- `web/src/lib/components/eql/ExplainPlan.svelte`
- `web/src/routes/data/+page.svelte` (real)

**Modified:**
- `crates/engram_dashboard/Cargo.toml` (add `chumsky`, `csv`, `parquet`)
- `crates/engram_dashboard/src/server.rs` (merge new routers)
- `crates/engram_dashboard/src/routes/mod.rs`
- `crates/engram_dashboard/web/package.json` (CodeMirror packages)

---

## Task 1 — Table schema registry

**Files:** `query/schema.rs`, test.

- [ ] **Step 1:** Define the metadata one place, one time:

```rust
// crates/engram_dashboard/src/query/schema.rs
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct TableSchema {
    pub name: &'static str,
    pub pk_fields: &'static [&'static str],
    pub fields: &'static [FieldSchema],
    pub indices: &'static [IndexSchema],
    pub row_count_hint: RowCountSource,
}

#[derive(Debug, Clone)]
pub struct FieldSchema {
    pub name: &'static str,
    pub ty: FieldType,
    pub nullable: bool,
    pub description: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType { String, I64, U64, F64, Bool, Timestamp, Enum(&'static [&'static str]), Array(Box<FieldType>) }

#[derive(Debug, Clone)]
pub struct IndexSchema {
    pub name: &'static str,
    pub key_fields: &'static [&'static str],
}

#[derive(Debug, Clone)]
pub enum RowCountSource { Exact(fn(&engram_server::state::AppState, &str) -> u64), Estimate }

pub fn all() -> Vec<TableSchema> { vec![
    nodes_schema(),
    edges_schema(),
    docs_schema(),
    vectors_schema(),
    insights_schema(),
    rules_schema(),
    watches_schema(),
    jobs_schema(),
    checkpoints_schema(),
    memory_bank_schema(),
    dashboard_ops_schema(),
] }

fn nodes_schema() -> TableSchema { /* concrete definition per current Redb layout */ TableSchema {
    name: "nodes",
    pk_fields: &["node_id"],
    fields: &[
        FieldSchema { name: "node_id", ty: FieldType::String, nullable: false, description: "Primary key" },
        FieldSchema { name: "project", ty: FieldType::String, nullable: false, description: "Project id" },
        FieldSchema { name: "kind", ty: FieldType::Enum(&["function","class","interface","file","db_table","db_column","global_state","control","ui_container","control_layout","web_service","http_handler","wcf_service","application","http_module","route_handler","app_setting","connection_string","binding_field","insight","memory_bank_section"]), nullable: false, description: "Node type" },
        FieldSchema { name: "label", ty: FieldType::String, nullable: false, description: "Display label" },
        FieldSchema { name: "file_path", ty: FieldType::String, nullable: true, description: "Source file" },
        FieldSchema { name: "start_line", ty: FieldType::U64, nullable: true, description: "Start line" },
        FieldSchema { name: "end_line", ty: FieldType::U64, nullable: true, description: "End line" },
        FieldSchema { name: "centrality", ty: FieldType::F64, nullable: true, description: "PageRank centrality" },
    ],
    indices: &[
        IndexSchema { name: "nodes_by_project", key_fields: &["project"] },
        IndexSchema { name: "nodes_by_project_kind", key_fields: &["project","kind"] },
    ],
    row_count_hint: RowCountSource::Exact(|s, p| s.graph.count_nodes(p).unwrap_or(0)),
}}

// ... similar functions for edges_schema, docs_schema, etc.
```

**Implementer note:** field names and index names must match the **actual Redb table layouts** in `engram_graph::store` and `engram_index::docstore`. The above is the *source of truth* for EQL — when the Redb layout adds a field, add it here too.

- [ ] **Step 2:** Test — `all()` returns exactly 11 tables, each has ≥1 field, PK fields exist in fields list.

- [ ] **Step 3:** Commit. `feat(dashboard): typed EQL schema registry for 11 tables`.

---

## Task 2 — EQL AST

**Files:** `query/ast.rs`.

- [ ] **Step 1:**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub table: String,
    pub filter: Option<Predicate>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Compare(String, CmpOp, Value),
    In(String, Vec<Value>),
    StartsWith(String, String),
    Contains(String, String),
    Matches(String, String),      // regex
    Exists(String),
    IsNull(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CmpOp { Eq, Neq, Lt, Lte, Gt, Gte }

#[derive(Debug, Clone, PartialEq)]
pub enum Value { Str(String), Int(i64), UInt(u64), Float(f64), Bool(bool), Timestamp(i64), Null }

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy { pub field: String, pub direction: Direction }

#[derive(Debug, Clone, PartialEq)]
pub enum Direction { Asc, Desc }
```

- [ ] **Step 2:** Commit. `feat(dashboard): EQL AST types`.

---

## Task 3 — EQL parser (chumsky)

**Files:** `query/parser.rs`.

- [ ] **Step 1:** Add `chumsky = "0.10"` to `Cargo.toml`.

- [ ] **Step 2:** Implement `pub fn parse(src: &str) -> Result<Query, ParseError>`. Grammar:

```
query    := "from" ident (where_clause)? (order_clause)? (limit_clause)? (offset_clause)?
where_clause := "where" predicate
predicate    := term (("and"|"or") term)*
term         := "(" predicate ")" | atom
atom         := ident cmp value
              | ident "in" "(" value ("," value)* ")"
              | ident "starts_with" string
              | ident "contains" string
              | ident "matches" string
              | "exists" ident
              | ident "is" "null"
cmp          := "==" | "!=" | "<" | "<=" | ">" | ">="
value        := string | number | bool | timestamp | "null"
order_clause := "order by" ident ("asc"|"desc")?
limit_clause := "limit" number
offset_clause:= "offset" number
```

Return rich error type with byte span for UI highlighting:

```rust
#[derive(Debug, thiserror::Error)]
#[error("parse error at {range:?}: {message}")]
pub struct ParseError { pub range: std::ops::Range<usize>, pub message: String }
```

- [ ] **Step 3:** Test `eql_parser_tests.rs`:
  - Simple: `from nodes limit 10` parses.
  - Nested: `from edges where (kind == 'Contains' or kind == 'Imports') and project == 'x' order by ts desc limit 50` parses.
  - Error: `from` without identifier errors with span 0..4.
  - All 9 predicate forms have a test.

- [ ] **Step 4:** Commit. `feat(dashboard): EQL parser with chumsky + span-carrying errors`.

---

## Task 4 — EQL type-checker

**Files:** `query/typecheck.rs`.

- [ ] **Step 1:** `pub fn check(q: &Query, schemas: &[TableSchema]) -> Result<&TableSchema, TypeError>`. Rules:
  - Table must exist.
  - Every field reference (in predicate, order_by) must exist on the table.
  - Comparison ops valid for field type (e.g., `starts_with` only on String).
  - Values must match field type (or be coercible — int to u64 if non-negative).
  - `enum`-typed field only accepts strings from the enum set.
  - `IsNull` / `Exists` only on `nullable = true` fields (else it's a static error — always true).

- [ ] **Step 2:** Tests: each error class has one failing case.

- [ ] **Step 3:** Commit. `feat(dashboard): EQL type-checker`.

---

## Task 5 — EQL planner

**Files:** `query/plan.rs`.

- [ ] **Step 1:** `pub struct Plan { pub index: Option<&'static IndexSchema>, pub prefix_match: Vec<(String, Value)>, pub residual_filter: Option<Predicate>, pub estimated_rows_scanned: u64 }`.

- [ ] **Step 2:** `pub fn plan(q: &Query, schema: &TableSchema, stats: &TableStats) -> Plan`.

Algorithm:
1. Flatten top-level AND into a list of conjuncts.
2. Collect equality conjuncts (`Compare(field, Eq, value)`) indexed by field.
3. For each index (sorted by key-field-count descending), try to match its key_fields against equality conjuncts; if all key fields match, select this index, record prefix matches.
4. Remaining conjuncts become `residual_filter`.
5. If no index matches: `index: None`, `residual_filter: Some(q.filter)`, `estimated_rows_scanned: stats.total`.

- [ ] **Step 3:** Tests covering: full-scan, single-key index, compound index, OR blocks → no index selected.

- [ ] **Step 4:** Commit. `feat(dashboard): EQL planner (index selection + residual filter)`.

---

## Task 6 — EQL executor

**Files:** `query/exec.rs`.

- [ ] **Step 1:** `pub async fn execute(plan: &Plan, q: &Query, state: &AppState) -> Result<QueryResult>`.

Implementation: dispatch on `q.table` to the correct Redb scan helper. For `nodes`:
- If `plan.index == Some(nodes_by_project)` with `prefix_match = [("project", "legacyerp")]`, use `GraphStore::range_nodes_by_project("legacyerp", ...)`.
- If `index == Some(nodes_by_project_kind)`, use the compound scan.
- Else: full scan.

For each row fetched, apply `residual_filter` in-process. Apply `order_by` if it matches the index order (free); otherwise collect-then-sort with a memory cap (error if >500k rows to sort — force LIMIT).

- [ ] **Step 2:** Return shape:

```rust
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub total_scanned: u64,
    pub returned: u64,
    pub index_used: Option<String>,
    pub duration_ms: u64,
}
```

- [ ] **Step 3:** Tests: each table has one round-trip test (index-hit and index-miss).

- [ ] **Step 4:** Commit. `feat(dashboard): EQL executor over Redb`.

---

## Task 7 — Property-based tests

**Files:** `tests/eql_proptest.rs`.

- [ ] **Step 1:** Add `proptest` to dev-deps.

- [ ] **Step 2:** Generators:
  - `arb_query()` generates `Query` values against the `nodes` schema.
  - `arb_malformed_string()` generates noise strings.

- [ ] **Step 3:** Properties:
  - Round-trip: `parse(print(q)) == Ok(q)` for any `arb_query()`.
  - No panic: `parse(arb_malformed_string())` never panics.
  - Type-check + plan + execute: for any `arb_query()` where `check()` succeeds, `execute()` returns `Ok` (on an empty graph, an empty result set).

- [ ] **Step 4:** Commit. `test(dashboard): property-based EQL tests`.

---

## Task 8 — `POST /api/v1/data/query`

**Files:** `routes/query.rs`.

- [ ] **Step 1:** Failing test: POST body `{"eql":"from nodes limit 0", "project":"x"}`, expect `{"rows":[],"columns":[...],"index_used":...}`.

- [ ] **Step 2:** Handler:

```rust
#[derive(serde::Deserialize)]
pub struct QueryBody { pub eql: String, pub project: String, pub explain: Option<bool> }

async fn run_query(State(s): State<AppState>, Json(body): Json<QueryBody>) -> Result<Json<serde_json::Value>, crate::error::DashboardError> {
    let ast = crate::query::parser::parse(&body.eql).map_err(|e| crate::error::DashboardError::BadRequest(e.to_string()))?;
    let schemas = crate::query::schema::all();
    let table = crate::query::typecheck::check(&ast, &schemas).map_err(|e| crate::error::DashboardError::BadRequest(e.to_string()))?;
    let stats = crate::query::schema::table_stats(&s, table, &body.project).await?;
    let plan = crate::query::plan::plan(&ast, table, &stats);

    // Guardrail
    if plan.estimated_rows_scanned > 1_000_000 {
        return Err(crate::error::DashboardError::TooLarge(format!("query would scan ~{} rows; add filters or use LIMIT", plan.estimated_rows_scanned)));
    }
    if body.explain.unwrap_or(false) {
        return Ok(Json(serde_json::to_value(&plan).unwrap()));
    }
    let result = crate::query::exec::execute(&plan, &ast, &s).await?;
    Ok(Json(serde_json::to_value(&result).unwrap()))
}
```

- [ ] **Step 3:** Register route behind CSRF. Commit. `feat(dashboard): POST /api/v1/data/query + explain`.

---

## Task 9 — Table list + per-table rows + row detail

**Files:** `routes/data.rs`.

- [ ] **Step 1:** `GET /api/v1/data/tables` — returns `[{ name, row_count, schema }]` using `schema::all()` and per-table `row_count_hint`.

- [ ] **Step 2:** `GET /api/v1/data/tables/:table/rows?project=&q=&cursor=&limit=` — wraps `run_query` with a pre-built EQL (`from :table where project == '<p>' [WHERE parsed from q]`) and paginates.

- [ ] **Step 3:** `GET /api/v1/data/tables/:table/row/:pk` — single-row read by PK.

- [ ] **Step 4:** Tests each. Commit. `feat(dashboard): data table browse endpoints`.

---

## Task 10 — Audit store

**Files:** `audit.rs`, test.

- [ ] **Step 1:** New Redb file: `<data_dir>/dashboard/ops.redb` with one table `dashboard_ops` keyed by `op_id: Uuid` and valued as:

```rust
#[derive(serde::Serialize, serde::Deserialize)]
pub struct OpRecord {
    pub op_id: String,
    pub table: String,
    pub pk: String,
    pub operation: Operation,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub ts: i64,
    pub project_id: Option<String>,
    pub reverted_by: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub enum Operation { Create, Update, Delete, Undo(String) }
```

Store on `AppState::dashboard_audit: Arc<DashboardAudit>` (extend AppState).

- [ ] **Step 2:** API: `append(record) -> op_id`, `get(op_id)`, `recent(limit=200)`, `set_reverted_by(op_id, by_op_id)`.

- [ ] **Step 3:** Tests. Commit. `feat(dashboard): dashboard_ops audit store`.

---

## Task 11 — CRUD endpoints with audit

**Files:** `routes/data.rs` (extend).

- [ ] **Step 1:** For each of POST / PUT / PATCH / DELETE:
  - Validate body against `TableSchema`.
  - Read current value (for `before`).
  - Call the underlying `GraphStore::upsert_node` / `delete_node` / `DocStore::...` / `Registry::...` through `spawn_blocking`.
  - Append `OpRecord` to audit.
  - Broadcast `ActivityEvent { kind: "data_crud", level: "info", message: ... }`.
  - Return new row + `op_id`.

- [ ] **Step 2:** Row-count confirmation precheck on bulk operations:

```
POST /api/v1/data/tables/:table/preview-delete  body: { where: <predicate JSON or EQL> }
   → { estimated_rows: N, sample: [first 10 rows] }
```

Frontend calls this before showing the confirmation modal.

- [ ] **Step 3:** Destructive rules:
  - DELETE on `nodes` with `cascade: false` refuses if live edges exist → 409 Conflict with the count.
  - `cascade: true` requires a secondary flag `confirm: true` in the body.

- [ ] **Step 4:** Tests for each verb + cascade + conflict cases.

- [ ] **Step 5:** Commit. `feat(dashboard): full CRUD endpoints with audit + cascade rules`.

---

## Task 12 — Undo

**Files:** `routes/data.rs` (extend).

- [ ] **Step 1:** `GET /api/v1/data/undo?limit=50` → last N ops newest-first.

- [ ] **Step 2:** `POST /api/v1/data/undo/:op_id`:
  - Load op record. If `reverted_by.is_some()` → 409.
  - Reverse operation:
    - `Create` → `Delete`.
    - `Update` → `Update(before)`.
    - `Delete` → `Create(before)`.
    - `Undo(other)` → re-apply `other`'s forward direction.
  - Apply through the same store write paths + validations.
  - Append a new op with `operation: Undo(original_op_id)` and set `reverted_by` on the original.
  - Emit activity event.

- [ ] **Step 3:** Tests: create → undo → row gone; update → undo → row has `before`; undo-of-undo → round-trip.

- [ ] **Step 4:** Commit. `feat(dashboard): audit-log undo with reverse-op chain`.

---

## Task 13 — Reconcile

**Files:** `routes/data.rs` (extend).

- [ ] **Step 1:** `POST /api/v1/data/reconcile` body: `{ project, tables?: string[] }`. Runs the existing indexer's incremental re-derivation for the named tables (or all). Returns a `job_id`; progress via WS `job_progress` events.

- [ ] **Step 2:** Drift detection flag: add `GET /api/v1/data/drift?project=X` that returns `{ drifted: [{ table, rows_edited_after_source_change: N }] }` — counts dashboard_ops entries whose `ts > source_file.mtime`.

- [ ] **Step 3:** Tests (integration). Commit. `feat(dashboard): reconcile + drift detection`.

---

## Task 14 — Export (CSV / JSON / Parquet)

**Files:** `routes/export.rs`.

- [ ] **Step 1:** `GET /api/v1/data/export?table=X&format=csv|json|parquet&project=Y&q=<EQL>` — streams the query result.

- [ ] **Step 2:** CSV via `csv::Writer` over an axum `StreamBody`. JSON as newline-delimited JSON (ndjson). Parquet: collect into an Arrow `RecordBatch`, write with `parquet::arrow::AsyncArrowWriter` to an in-memory buffer, then stream.

- [ ] **Step 3:** Tests each format. Commit. `feat(dashboard): bulk export CSV/JSON/Parquet`.

---

## Task 15 — Frontend: EQL CodeMirror language

**Files:** `web/src/lib/components/eql/editor.ts`.

- [ ] **Step 1:** Add packages: `pnpm add @codemirror/state @codemirror/view @codemirror/language @codemirror/autocomplete @lezer/highlight`.

- [ ] **Step 2:** Define a minimal Lezer grammar for EQL (or use a `StreamLanguage` shim — sufficient for v1):

```typescript
import { StreamLanguage } from '@codemirror/language';
import { CompletionContext } from '@codemirror/autocomplete';

const keywords = ['from','where','and','or','order','by','asc','desc','limit','offset','in','starts_with','contains','matches','exists','is','null','true','false'];

export const eqlLanguage = StreamLanguage.define({
  token(stream) {
    if (stream.match(/\s+/)) return null;
    if (stream.match(/'[^']*'/) || stream.match(/"[^"]*"/)) return 'string';
    if (stream.match(/-?\d+(\.\d+)?/)) return 'number';
    if (stream.match(/\b(?:==|!=|<=|>=|<|>)\b/)) return 'operator';
    for (const kw of keywords) { if (stream.match(new RegExp(`\\b${kw}\\b`, 'i'))) return 'keyword'; }
    if (stream.match(/\b[a-zA-Z_][a-zA-Z0-9_]*\b/)) return 'variableName';
    stream.next();
    return null;
  }
});

export function eqlCompletion(tableSchema: any) {
  return (context: CompletionContext) => {
    const word = context.matchBefore(/\w*/);
    if (!word) return null;
    const options = [
      ...keywords.map(k => ({ label: k, type: 'keyword' })),
      ...tableSchema.fields.map((f: any) => ({ label: f.name, type: 'property', info: f.description })),
    ];
    return { from: word.from, options };
  };
}
```

- [ ] **Step 3:** Commit. `feat(dashboard-ui): CodeMirror EQL language + autocomplete`.

---

## Task 16 — Frontend: EqlEditor + QueryBuilder + bidirectional sync

**Files:** `eql/EqlEditor.svelte`, `eql/QueryBuilder.svelte`, `eql/editor.ts` (extend with AST helpers).

- [ ] **Step 1:** `EqlEditor.svelte` — wraps CodeMirror 6 `EditorView` with `eqlLanguage` + `autocompletion(eqlCompletion(schema))`. Emits `change` with the current source.

- [ ] **Step 2:** `QueryBuilder.svelte` — panels for FROM (table select), WHERE (list of rows: field / op / value), ORDER BY (field + direction), LIMIT. Emits `change` with an AST.

- [ ] **Step 3:** Bidirectional sync lives in the parent page:
  - Parent keeps `source: string` and `ast: Query | null`.
  - On editor change → call `/api/v1/data/query?explain=true` only for parse; if ok, update `ast`. (Or ship a JS parser; for v1, server round-trip is acceptable with ~100ms debounce.)
  - On builder change → render AST to source via a local printer (pure TS).
  - Both inputs bind to the same state; last edit wins.

- [ ] **Step 4:** Commit. `feat(dashboard-ui): EqlEditor + QueryBuilder with bidirectional sync`.

---

## Task 17 — Frontend: ResultTable + inline CRUD

**Files:** `eql/ResultTable.svelte`, `eql/ConfirmModal.svelte`.

- [ ] **Step 1:** `ResultTable.svelte`:
  - Columns from `QueryResult.columns`.
  - Virtualized rows (add `svelte-virtual-list` or hand-roll) — handles 500-row pages.
  - Each row: hover reveals "edit" / "delete" icons.
  - Edit → inline row editor with per-column input (driven by `TableSchema.fields`); Save → `PATCH /data/tables/:table/row/:pk`; Cancel → revert.
  - Delete → `ConfirmModal` with row count ("delete 1 row from nodes?") → `DELETE`.

- [ ] **Step 2:** Bulk operations UI: select multiple rows → "Delete N" button → calls `preview-delete` first, shows count in modal, then applies via a loop (or a dedicated bulk endpoint if we ship one in v2).

- [ ] **Step 3:** "Export" dropdown at the top of the table → CSV/JSON/Parquet navigation to `/api/v1/data/export?...&q=<current_eql>`.

- [ ] **Step 4:** Commit. `feat(dashboard-ui): ResultTable with inline CRUD + bulk delete + export`.

---

## Task 18 — Frontend: Data browser page

**Files:** `web/src/routes/data/+page.svelte`.

- [ ] **Step 1:** Three-column layout mirroring the spec mockup: table picker · query builder + editor · result table.

- [ ] **Step 2:** "Explain plan" button — calls `/api/v1/data/query` with `explain: true`, opens a side drawer showing chosen index, estimated rows scanned, residual filter.

- [ ] **Step 3:** Drift banner — on page load, fetch `/api/v1/data/drift?project=X`; if any entries, show a yellow banner offering "Reconcile".

- [ ] **Step 4:** Audit/undo drawer — button in the header opens a panel listing last 50 ops via `/api/v1/data/undo`. Each op has an "Undo" button.

- [ ] **Step 5:** Commit. `feat(dashboard-ui): Data browser page`.

---

## Task 19 — Self-review

- [ ] **Step 1:** Every write endpoint is under CSRF middleware (Plan 1 Task 6). Confirm by grep.
- [ ] **Step 2:** The `audit` module's Redb writes are inside `spawn_blocking`. Confirm.
- [ ] **Step 3:** EQL error messages include spans usable for UI highlighting. Spot-check with a hand-written malformed input.
- [ ] **Step 4:** `dashboard_ops` row size is bounded by `before`/`after` JSON sizes — add a documented 1 MB cap per op with an error path when the captured value is larger (should be extremely rare given the size of graph nodes; pragmatic ceiling).

**Completion gate:** Can open Data browser, type `from nodes where kind == 'function' and project == 'X' order by centrality desc limit 50`, see the rows, edit one, see it update, undo, see the revert, export CSV. All audit entries visible.
