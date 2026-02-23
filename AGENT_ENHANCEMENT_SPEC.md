# Engram Agent Enhancement Specification
## "Engram as a Copilot for AI Agents in Legacy VB.NET WebForms"

**Version:** 1.0
**Date:** 2026-02-23
**Purpose:** Equip Claude Code (and future AI agents) to implement features in a legacy
VB.NET / ASP.NET WebForms / ADO.NET / jQuery / GIS codebase with near-zero risk of
regressions. Every tool in this spec is designed to answer one question:
**"Is it safe to touch this? And if so, exactly how?"**

---

## Table of Contents

1. [Phase 37 Wiring — Expose Existing Services](#phase-37-wiring)
2. [Phase 38 — Agent Pre-Edit Safety Kit](#phase-38)
3. [Phase 39 — Database Oracle](#phase-39)
4. [Phase 40 — VB.NET Deep Intelligence](#phase-40)
5. [Phase 41 — WebForms Control Brain](#phase-41)
6. [Phase 42 — Agentic Supercharger](#phase-42)
7. [Phase 43 — Codebase Oracle](#phase-43)
8. [Phase 44 — Automated Quality Gates](#phase-44)
9. [Phase 45 — Edit Session Protocol](#phase-45)
10. [Cross-Cutting Notes](#cross-cutting)

---

## Phase 37 Wiring — Expose Existing Services {#phase-37-wiring}

> **Status:** Services exist, zero tool handlers, zero capability registrations.
> **Effort:** Low (1–2 days). Wire existing code to MCP surface.

### 37-W1: `analyze_database_intelligence`

**What it is:** Full database analysis wrapping `database_intelligence_service`. Produces
the complete `DatabaseIntelligence` struct (SP logic, SP call chains, triggers, schema).

**Request struct:**
```rust
pub struct AnalyzeDatabaseIntelligenceRequest {
    pub project_id: String,
    /// Optional: path to a specific .sql file to analyze in isolation.
    /// If omitted, uses all .sql files already indexed for the project.
    pub sql_file_path: Option<String>,
    /// Include LLM-powered SP summaries (requires dreaming engine).
    #[serde(default = "default_true")]
    pub include_sp_logic: bool,
    /// Maximum stored procedures to summarize (avoid runaway LLM cost).
    #[serde(default = "default_50")]
    pub sp_limit: usize,
    /// Output as JSON instead of Markdown report.
    #[serde(default)]
    pub output_json: bool,
}
```

**Returns:** Markdown report with sections:
- **Schema Summary** — table list, column counts, FK relationships, views
- **Stored Procedures** — per-SP: purpose, parameters, tables touched, calls other SPs
- **SP Call Graph** — chains + cycles (cycles are red-flag: potential infinite recursion)
- **Triggers** — table, event type (AFTER INSERT/UPDATE/DELETE), body summary
- **Cross-Reference Warnings** — tables in code but not schema, schema tables never queried
- **Business Rules Inferred** — CHECK constraints translated to plain English

**Implementation:**
Handler in `crates/engram_server/src/handlers/migration_tools.rs`.
Call `database_intelligence_service::analyze_database_intelligence(sp_catalog, schema_sql)`.
The SP catalog comes from `full_project_migration_service`'s `StoredProcedureCatalog` which
is already built during `analyze_full_project_migration`.

---

### 37-W2: `get_sp_details`

**What it is:** Retrieve deep analysis for a single stored procedure by name.

**Request struct:**
```rust
pub struct GetSpDetailsRequest {
    pub project_id: String,
    pub sp_name: String,
    /// If true, re-analyze even if cached.
    #[serde(default)]
    pub force_refresh: bool,
}
```

**Returns:**
```rust
pub struct SpDetailsResult {
    pub sp_name: String,
    pub purpose: String,
    pub parameters: Vec<SpParameter>,     // name, type, direction, default
    pub steps: Vec<String>,               // numbered business logic steps
    pub tables_read: Vec<String>,
    pub tables_written: Vec<String>,
    pub calls_sps: Vec<String>,           // other SPs called
    pub called_by_sps: Vec<String>,       // SPs that call this one (reverse lookup)
    pub called_by_code: Vec<CodeLocation>, // code files that call this SP
    pub triggers_on_affected_tables: Vec<String>, // triggers that may fire
    pub side_effects: Vec<String>,
    pub has_transaction: bool,
    pub has_cursor: bool,
    pub has_dynamic_sql: bool,            // EXEC(@sql) — injection risk flag
    pub complexity_estimate: String,      // "low" | "medium" | "high"
    pub content_hash: String,
}
```

---

### 37-W3: `list_triggers`

**What it is:** Get all triggers for a project, optionally filtered by table.

**Request struct:**
```rust
pub struct ListTriggersRequest {
    pub project_id: String,
    pub table_name: Option<String>, // filter to a specific table
}
```

**Returns:** `Vec<TriggerInfo>` with which code paths indirectly fire each trigger
(via table write → trigger → side effect chain). This is critical: if I update a row
in `Orders`, I need to know if a trigger runs that inserts into `AuditLog` or sends
an email via `sp_send_dbmail`.

---

### 37-W4: `analyze_sync_hazards`

**What it is:** Expose `sync_hazard_detector::detect_sync_hazards()` as a callable tool.
Already fully implemented — just missing a handler.

**Request struct:**
```rust
pub struct AnalyzeSyncHazardsRequest {
    pub project_id: String,
    /// Specific file to analyze. If omitted, scans all indexed .vb/.cs files.
    pub file_path: Option<String>,
    /// Only return hazards at or above this severity.
    #[serde(default = "default_medium")]
    pub min_severity: String, // "medium" | "high" | "critical"
}
```

**Returns:** `SyncHazardReport` per file:
- Per-hazard: pattern type, line number, matched text, severity, modern equivalent, risk type
- `async_readiness` score (0.0–1.0) per file — "how ready is this file to be converted to async"
- Project-level summary: total critical/high/medium counts, files with most hazards

**Why critical for this use case:** The #1 source of bugs when adding async/await patterns
to a WebForms codebase is deadlocks from `.Result`/`.Wait()` inside `SynchronizationContext`.
Before I add any async code, I need to see this report.

---

### 37-W5: `get_jquery_inventory`

**What it is:** Expose `jquery_inventory::scan_jquery_usage()` as a tool.

**Request struct:**
```rust
pub struct GetJQueryInventoryRequest {
    pub project_id: String,
    /// Filter to files matching this glob pattern.
    pub file_filter: Option<String>,
}
```

**Returns:** `JQueryInventory`:
- Core version + vulnerability flags
- UI widgets used (datepicker, dialog, autocomplete, etc.) with file:line locations
- Third-party plugins (DataTables, jQuery Validate, Select2, etc.)
- Custom plugins defined in the project
- Deprecated patterns (`.live()`, `.die()`, `.bind()`, `.size()`, etc.)
- Per-item: `modern_equivalent` and `migration_complexity`

---

### Capability Registration (Phase 37 additions)

Add to `capabilities.rs`:
```rust
CapabilityFlag { key: "analyze_database_intelligence", status: CapabilityStatus::Implemented },
CapabilityFlag { key: "get_sp_details",               status: CapabilityStatus::Implemented },
CapabilityFlag { key: "list_triggers",                 status: CapabilityStatus::Implemented },
CapabilityFlag { key: "analyze_sync_hazards",          status: CapabilityStatus::Implemented },
CapabilityFlag { key: "get_jquery_inventory",          status: CapabilityStatus::Implemented },
```

---

## Phase 38 — Agent Pre-Edit Safety Kit {#phase-38}

> **Theme:** Before touching a single line, give me everything I need to know in one call.
> **Effort:** Medium (3–5 days per tool). All deterministic; no LLM required.

### 38-1: `get_method_edit_context` ⭐ HIGHEST PRIORITY

**The single most impactful tool in this entire spec.**

**Purpose:** One call, everything about a method. Before I edit anything, I call this first.
Without it, I'd need 6–8 separate tool calls to assemble the same picture. Each extra round-trip
is an opportunity to miss something.

**Request struct:**
```rust
pub struct GetMethodEditContextRequest {
    pub project_id: String,
    pub file_path: String,
    pub method_name: String,
    /// Optional class name to disambiguate overloads.
    pub class_name: Option<String>,
    /// Include LLM business logic summary if available.
    #[serde(default = "default_true")]
    pub include_business_logic: bool,
    /// Maximum callers to enumerate (default 50 — cap prevents huge responses).
    #[serde(default = "default_50")]
    pub max_callers: usize,
}
```

**Returns:** `MethodEditContext` — a complete pre-edit briefing:

```rust
pub struct MethodEditContext {
    // Identity
    pub file_path: String,
    pub class_name: String,
    pub method_name: String,
    pub fqn: String,                        // "MyNamespace.MyClass.MyMethod"
    pub signature: String,                   // Full VB.NET signature
    pub line_start: u32,
    pub line_end: u32,
    pub language: String,                    // "vb.net" | "csharp"

    // Who calls this method
    pub direct_callers: Vec<CallerLocation>, // file:line + snippet
    pub aspx_event_bindings: Vec<String>,    // "Handles Button1.Click in checkout.aspx.vb"
    pub jquery_callers: Vec<JqueryCaller>,   // __doPostBack or $.ajax hitting this
    pub is_web_service_method: bool,
    pub is_http_handler_method: bool,

    // What this method calls
    pub direct_callees: Vec<String>,         // FQNs of methods this calls
    pub stored_procs_called: Vec<SpCallSite>,// SP name + parameters passed
    pub inline_sql: Vec<InlineSqlSite>,      // raw SQL + parameters

    // Database footprint
    pub tables_read: Vec<String>,
    pub tables_written: Vec<String>,
    pub columns_accessed: Vec<ColumnAccess>, // table.column + read/write
    pub triggers_that_may_fire: Vec<String>, // triggers on written tables

    // Shared state footprint
    pub session_keys_read: Vec<StateKeyAccess>,
    pub session_keys_written: Vec<StateKeyAccess>,
    pub viewstate_keys: Vec<StateKeyAccess>,
    pub application_keys: Vec<StateKeyAccess>,
    pub cache_keys: Vec<StateKeyAccess>,

    // UI footprint
    pub controls_bound: Vec<String>,         // controls whose DataSource/Text etc. this sets
    pub controls_read: Vec<String>,          // control values this reads (e.g., txtSearch.Text)
    pub script_injections: Vec<String>,      // RegisterStartupScript calls

    // Risk signals
    pub blast_radius_score: f32,             // 0.0–100.0
    pub has_on_error_resume_next: bool,
    pub has_on_error_goto: bool,
    pub has_late_binding: bool,              // "Dim x As Object" / CreateObject
    pub has_byref_params: bool,
    pub has_optional_params: bool,
    pub sync_hazards: Vec<SyncHazard>,       // .Result/.Wait() etc.
    pub vbnet_quirks: Vec<VbNetQuirk>,       // semantic traps (see Phase 40)

    // Test coverage
    pub test_methods_covering_this: Vec<String>, // names of test methods that exercise this

    // Business logic
    pub business_logic_summary: Option<String>,  // from Phase 36 analyze_business_logic

    // Edit safety verdict
    pub edit_safety: EditSafety,
}

pub struct EditSafety {
    /// "green" | "yellow" | "red"
    pub verdict: String,
    /// Human-readable reasons for the verdict.
    pub reasons: Vec<String>,
    /// Specific things to check before editing.
    pub pre_edit_checklist: Vec<String>,
    /// Specific things to verify after editing.
    pub post_edit_checklist: Vec<String>,
}
```

**Edit Safety Scoring Logic:**
- `green`: ≤2 callers, no shared state writes, no triggers, no async hazards, test coverage exists
- `yellow`: 3–10 callers OR shared state OR triggers OR no tests
- `red`: >10 callers OR public API surface OR is a web service method OR `On Error Resume Next`
  (exception swallowing makes it impossible to know what used to fail silently)

**Implementation notes:**
- Assemble from existing services: `blast_radius_service`, `trace_state_usage`,
  `graph_service.find_references()`, `sp_extractor`, `sync_hazard_detector`,
  `business_logic_service`
- The whole assembly must run in a single `spawn_blocking` call on the graph
- Cache result keyed on `(project_id, file_path, method_name, content_hash)` in DocStore
  under namespace `method_edit_context`

---

### 38-2: `check_edit_safety`

**Purpose:** "I'm about to do X to method Y — what will break?"
More targeted than `get_method_edit_context`: takes a proposed change description and
returns a concrete go/no-go with specific breakage predictions.

**Request struct:**
```rust
pub struct CheckEditSafetyRequest {
    pub project_id: String,
    pub file_path: String,
    pub method_name: String,
    pub change_type: EditChangeType,
    /// Optional: describe the change in natural language for LLM-enhanced analysis.
    pub change_description: Option<String>,
}

pub enum EditChangeType {
    RenameMethod { new_name: String },
    AddParameter { param_name: String, param_type: String, has_default: bool },
    RemoveParameter { param_name: String },
    ChangeReturnType { old_type: String, new_type: String },
    ChangeSqlQuery { new_query_fragment: Option<String> },
    ExtractToSeparateMethod { lines_start: u32, lines_end: u32 },
    MakeAsync,
    AddNullCheck { param_name: String },
    ChangeAccessModifier { new_modifier: String }, // Public → Private
    DeleteMethod,
    Other { description: String },
}
```

**Returns:** `EditSafetyReport`:
```rust
pub struct EditSafetyReport {
    pub verdict: String,              // "safe" | "risky" | "breaking"
    pub confidence: f32,              // 0.0–1.0
    pub breaking_changes: Vec<BreakingChange>,
    pub risky_changes: Vec<RiskyChange>,
    pub files_requiring_updates: Vec<String>, // files you MUST also edit
    pub suggested_test_plan: Vec<String>,
}

pub struct BreakingChange {
    pub location: String,             // file:line
    pub description: String,          // "Caller passes 3 args, new signature requires 4"
    pub severity: String,             // "compile_error" | "runtime_error" | "silent_behavior_change"
}
```

**Change-type-specific logic:**
- `RenameMethod` → find all `Handles X.Y` clauses + direct calls + ASPX declarative bindings
- `AddParameter` without default → enumerate all call sites; any that don't pass the new arg = compile error
- `RemoveParameter` → any call site passing that arg = compile error; check SP if forwarded to SQL
- `ChangeSqlQuery` → re-validate against schema (invoke `validate_sql_fragment`)
- `MakeAsync` → run `sync_hazard_detector` on callers; flag any `.Result`/`.Wait()` in callers
- `DeleteMethod` → blast radius; if blast_radius > 0, verdict = "breaking"
- `ChangeAccessModifier` to Private → any callers outside the class = breaking

---

### 38-3: `get_global_state_map`

**Purpose:** Project-wide map of every shared state key — Session, ViewState, Application,
Cache, HttpContext.Items. Before I touch any code that reads/writes shared state, I need the
full picture. `trace_state_usage` exists but requires knowing the key name upfront.

**Request struct:**
```rust
pub struct GetGlobalStateMapRequest {
    pub project_id: String,
    /// Filter by state store type.
    pub state_type: Option<StateStoreType>, // None = all
    /// Only include keys with >= this many access locations.
    #[serde(default = "default_1")]
    pub min_access_count: usize,
}

pub enum StateStoreType {
    Session,
    ViewState,
    Application,
    Cache,
    HttpContextItems,
    All,
}
```

**Returns:** `GlobalStateMap`:
```rust
pub struct GlobalStateMap {
    pub session_keys: Vec<StateKeyInfo>,
    pub viewstate_keys: Vec<StateKeyInfo>,
    pub application_keys: Vec<StateKeyInfo>,
    pub cache_keys: Vec<StateKeyInfo>,
    pub httpcontext_items_keys: Vec<StateKeyInfo>,
    pub orphaned_writes: Vec<StateKeyInfo>, // written but never read — dead state
    pub orphaned_reads: Vec<StateKeyInfo>,  // read but never written — potential null bug
}

pub struct StateKeyInfo {
    pub key: String,
    pub inferred_type: Option<String>,    // e.g., "Integer", "String", "DataTable"
    pub write_locations: Vec<CodeLocation>,
    pub read_locations: Vec<CodeLocation>,
    pub pages_that_use_it: Vec<String>,   // deduplicated .aspx file names
    pub is_cross_page: bool,              // written on one page, read on another
    pub null_risk: NullRisk,              // can it be Nothing when read?
}

pub enum NullRisk {
    Safe,       // always written before read in same event chain
    Possible,   // sometimes written conditionally
    Likely,     // read before write, or on different pages without guaranteed write path
}
```

**Why this is transformative:** Cross-page Session state is the #1 source of hard-to-reproduce
bugs in WebForms apps. Knowing `Session["CartItems"]` is written in 3 places and read in 7,
with 2 of the reads on pages that don't have a guaranteed write path, tells me exactly where
`NullReferenceException` bugs are hiding.

---

### 38-4: `find_dead_methods`

**Purpose:** Enumerate methods with zero reachable callers from any entry point.
Safe to delete, or safe to modify without fear of unexpected callers.

**Request struct:**
```rust
pub struct FindDeadMethodsRequest {
    pub project_id: String,
    pub file_path: Option<String>,    // scope to one file, or entire project
    /// Include methods reachable only via reflection or string-based invocation.
    /// Default false — conservative: assume reflectively-called = live.
    #[serde(default)]
    pub include_reflection_risk: bool,
    /// Minimum confidence that a method is truly dead (0.0–1.0). Default 0.8.
    #[serde(default = "default_0_8")]
    pub min_confidence: f32,
}
```

**Returns:** `DeadMethodReport`:
```rust
pub struct DeadMethodReport {
    pub confirmed_dead: Vec<DeadMethod>,
    pub probably_dead: Vec<DeadMethod>,    // high confidence but reflection risk
    pub false_positive_risks: Vec<String>, // patterns that could defeat static analysis
    pub summary: DeadMethodSummary,
}

pub struct DeadMethod {
    pub fqn: String,
    pub file_path: String,
    pub line_start: u32,
    pub confidence: f32,
    pub reason: String,    // "no callers in graph, no Handles clause, not exposed as endpoint"
    pub dead_since: Option<String>, // git commit hash when it was last touched (from git service)
    pub safe_to_delete: bool,
}
```

**Reachability definition:** A method is LIVE if any of the following is true:
1. It appears as a callee in any call-graph edge
2. It has a `Handles ControlX.EventY` clause
3. It is bound via `AddHandler` anywhere
4. It is a `Protected Sub Page_Load` (WebForms lifecycle method)
5. It has `<WebMethod>` or `<OperationContract>` attribute
6. Its name matches an ASPX event binding (`OnClick="ButtonSave_Click"`)
7. It is a `Public` method of a class that implements an `Interface`
8. It matches a jQuery `$.ajax` URL pattern indexed by the JS extractor
9. Its name appears in a string literal that looks like reflection: `"MethodName"` + `GetType().GetMethod`

---

### 38-5: `validate_sql_fragment`

**Purpose:** Before writing any ADO.NET code, validate that table names, column names,
and parameter names in the SQL are consistent with the indexed schema.

**Request struct:**
```rust
pub struct ValidateSqlFragmentRequest {
    pub project_id: String,
    pub sql: String,
    /// Context: where will this SQL be executed.
    pub context: SqlContext,
    /// File and line for error reporting.
    pub source_file: Option<String>,
    pub source_line: Option<u32>,
}

pub enum SqlContext {
    AdoNetInline,         // cmd.CommandText = "..."
    StoredProcBody,       // inside a CREATE PROCEDURE
    DapperQuery,          // Dapper.Query<T>("...")
    Other(String),
}
```

**Returns:** `SqlValidationResult`:
```rust
pub struct SqlValidationResult {
    pub is_valid: bool,
    pub errors: Vec<SqlValidationError>,
    pub warnings: Vec<SqlValidationWarning>,
    pub schema_coverage: f32,  // fraction of referenced tables/columns found in schema
}

pub struct SqlValidationError {
    pub error_type: SqlErrorType,
    pub object_name: String,     // the table/column/param that failed
    pub suggestion: Option<String>, // "did you mean 'UserName'?"
    pub position_hint: Option<String>,
}

pub enum SqlErrorType {
    TableNotFound,
    ColumnNotFound { table: String },
    ColumnTypeConflict { column: String, expected: String, actual: String },
    AmbiguousColumn { found_in_tables: Vec<String> },
    StoredProcNotFound,
    StoredProcParamNotFound { sp: String },
    StoredProcParamTypeMismatch { sp: String, param: String },
    NullableColumnUsedWithNonNullableParam,
}

pub struct SqlValidationWarning {
    pub warning_type: String,
    pub message: String,
}
// e.g. "SELECT * is fragile — if schema changes, bound code may break"
// e.g. "Column 'DeletedAt' exists but is not referenced in WHERE clause — soft deletes?"
// e.g. "Table 'Orders' has a trigger on UPDATE — this query will fire it"
```

**"Did you mean" logic:** Levenshtein distance ≤ 2 on table/column names against schema.

**Suggestion examples:**
- `ColumnNotFound("UserNam")` → `"did you mean 'UserName' in table 'Users'?"`
- `TableNotFound("Ordes")` → `"did you mean 'Orders'?"`
- `StoredProcNotFound("spGetUser")` → `"did you mean 'sp_GetUser'?"`

---

## Phase 39 — Database Oracle {#phase-39}

> **Theme:** The `database.sql` file the user drops in the root becomes a first-class
> citizen. The schema is the ground truth for all DB-touching code.

### 39-1: Auto-detect `database.sql` during `index_project`

**When `index_project` or `update_project` runs:**
1. Scan the project root for files matching: `database.sql`, `schema.sql`, `db.sql`,
   `*_schema.sql`, `*_db.sql`, `DDL.sql`
2. If found, automatically run `ddl_extractor::extract_ddl()` on the file
3. Run `database_intelligence_service::analyze_database_intelligence()` on it
4. Store results in a dedicated `schema` namespace in DocStore (KeepForever)
5. Log: `[schema] Auto-ingested database.sql: N tables, M stored procedures, K triggers`

**No separate tool call needed.** The user drops the file, indexes the project, done.

---

### 39-2: `index_database_schema`

**Purpose:** Explicitly ingest a standalone SQL file (when auto-detection isn't sufficient,
or when there are multiple SQL files to combine).

**Request struct:**
```rust
pub struct IndexDatabaseSchemaRequest {
    pub project_id: String,
    pub sql_file_path: String,
    /// If multiple SQL files, combine them all into one schema context.
    pub additional_files: Vec<String>,
    /// Schema name/label for disambiguation when multiple schemas exist.
    pub schema_label: Option<String>,
    /// Force re-index even if content hash matches cached version.
    #[serde(default)]
    pub force_refresh: bool,
}
```

**Returns:** `SchemaIndexResult`:
```rust
pub struct SchemaIndexResult {
    pub tables_indexed: usize,
    pub columns_indexed: usize,
    pub stored_procs_indexed: usize,
    pub views_indexed: usize,
    pub triggers_indexed: usize,
    pub foreign_keys_indexed: usize,
    pub warnings: Vec<String>,    // parse failures, ambiguous statements
    pub graph_nodes_created: usize,
    pub graph_edges_created: usize,
    pub code_table_links_created: usize,  // code→schema links established
}
```

**Post-index action:** After schema is indexed, re-run cross-reference analysis to link
existing code-level SQL references (from `vb_extractor`'s `QueriesTable` edges) to actual
`db_table` graph nodes. This closes the gap between "this method uses table X" (string)
and "this method → db_table:Orders node" (typed graph edge).

---

### 39-3: `query_schema`

**Purpose:** Query the indexed schema interactively. The lightweight version of
`get_table_schema` with richer output.

**Request struct:**
```rust
pub struct QuerySchemaRequest {
    pub project_id: String,
    pub query_type: SchemaQueryType,
}

pub enum SchemaQueryType {
    Table { name: String },
    Column { table: String, column: String },
    TablesThatReferenceForeignKey { table: String, column: String },
    TablesWithColumn { column_name_pattern: String },
    AllTables,
    TriggersOnTable { table: String },
    StoredProc { sp_name: String },
    ViewsSourcedFrom { table: String },
    IndexesOnTable { table: String },
    NullableColumns { table: String },
    ComputedColumns { table: String },
}
```

**Returns:** Structured data appropriate to the query type, plus a narrative summary.
Example for `Table { name: "Orders" }`:
```
Table: Orders
Columns (12):
  OrderID          int           NOT NULL  PK
  CustomerID       int           NOT NULL  FK → Customers.CustomerID (CASCADE DELETE)
  OrderDate        datetime      NOT NULL  DEFAULT GETDATE()
  ShippedDate      datetime      NULL
  TotalAmount      decimal(18,2) NOT NULL
  Status           varchar(20)   NOT NULL  DEFAULT 'Pending'
  ...

Indexes: IX_Orders_CustomerID, IX_Orders_OrderDate
Triggers: TR_Orders_Audit (AFTER INSERT, UPDATE)
Views sourcing this table: vw_PendingOrders, vw_OrderSummary
Code files querying this table: checkout.aspx.vb (3), orders.aspx.vb (7), ...
Stored procs operating on this table: sp_GetOrder, sp_CreateOrder, sp_UpdateStatus
```

---

### 39-4: `detect_n_plus_one`

**Purpose:** Find N+1 query patterns — loading a list then querying each item individually.
The #1 performance bug in ADO.NET WebForms code.

**Detection patterns:**
```
' Classic N+1: loop containing a DB call
For Each row In dt.Rows
    Dim details = GetItemDetails(CInt(row("ItemID")))  ← SQL inside loop
Next

' GridView with per-row DB call in RowDataBound
Protected Sub GridView1_RowDataBound(...) Handles GridView1.RowDataBound
    Dim id = e.Row.Cells(0).Text
    lblExtra.Text = GetExtraData(id)  ← SQL per row
End Sub
```

**Returns:** Per-location:
- File:line of the loop/row-binding
- The inner SQL call
- Estimated severity (loop iteration estimate based on table size if known)
- Suggested fix: JOIN the extra data in the initial query, or use a batch fetch

---

### 39-5: `find_missing_indexes`

**Purpose:** From the pattern of SQL queries in the codebase, suggest DB indexes that
don't exist but would dramatically improve performance.

**Logic:**
1. Collect all `WHERE` clause columns from indexed SQL (inline + SPs)
2. Collect all `JOIN` conditions
3. Collect all `ORDER BY` columns
4. Cross-reference with indexed schema's existing indexes
5. Rank by estimated frequency of use (more call sites = more important)

**Returns:** Prioritized list of `CREATE INDEX` suggestions with:
- Table and columns
- Reason (which queries would benefit, with file:line)
- Type recommendation (non-clustered, covering, filtered)
- Estimated selectivity warning ("this column has low cardinality — index may not help")

---

### 39-6: `generate_typed_model`

**Purpose:** From a table or stored procedure result set, generate a fully-typed VB.NET
or C# model class. Saves time and eliminates typos when creating entity classes.

**Request struct:**
```rust
pub struct GenerateTypedModelRequest {
    pub project_id: String,
    pub source: ModelSource,
    pub target_language: TargetLanguage, // VbNet | CSharp
    pub output_style: ModelOutputStyle,  // PlainClass | Record | DapperPoco | EfEntity
    pub namespace: Option<String>,
}

pub enum ModelSource {
    Table { name: String },
    StoredProc { name: String },   // uses SP parameter list as input model
    InlineSql { sql: String },     // parse SELECT columns to derive output model
}
```

**Returns:** Complete class definition as a string, including:
- VB.NET: `Public Class OrderModel` / `Public Property OrderID As Integer`
- C#: `public class OrderModel { public int OrderID { get; set; } }`
- For EF: navigation properties inferred from FK relationships
- For Dapper: constructor with all fields, `IEnumerable<OrderModel>` factory

---

## Phase 40 — VB.NET Deep Intelligence {#phase-40}

> **Theme:** VB.NET is not C#. Semantic differences are silent, not compile errors.
> This phase makes every VB.NET trap visible before I write a single line.

### 40-1: `list_vbnet_quirks`

**Purpose:** Per-file or per-project report of VB.NET-specific semantic traps.

**Request struct:**
```rust
pub struct ListVbNetQuirksRequest {
    pub project_id: String,
    pub file_path: Option<String>,  // None = entire project
    pub include_severity: Vec<QuirkSeverity>, // default: all
}
```

**Returns:** Categorized quirk inventory:

```rust
pub struct VbNetQuirkReport {
    pub file_path: String,
    pub quirks: Vec<VbNetQuirk>,
    pub risk_score: f32,   // 0.0–10.0, weighted by severity
}

pub struct VbNetQuirk {
    pub quirk_type: VbNetQuirkType,
    pub line: u32,
    pub snippet: String,
    pub severity: QuirkSeverity,
    pub explanation: String,
    pub csharp_trap: String,  // "In C# this would be: ..."
    pub mitigation: String,
}
```

**Quirk catalog (20 patterns):**

| QuirkType | Severity | Explanation |
|-----------|----------|-------------|
| `OnErrorResumeNext` | Critical | Silently swallows ALL exceptions in scope. Any code after this is execution-continuation-unreliable. |
| `OnErrorGoto` | High | Non-local control flow. The `Err` object state is implicit. |
| `CaseInsensitiveStringComparison` | High | `If str = "Admin"` in VB is case-insensitive by default. C# `==` is case-sensitive. |
| `LateBoundObject` | High | `Dim x As Object` — runtime dispatch, no compile-time type safety. |
| `CreateObjectCOM` | High | `CreateObject("Excel.Application")` — COM interop, not portable. |
| `ByRefParameter` | Medium | Callers's variable mutated in place. C# requires explicit `ref`. |
| `OptionalWithIsMissing` | Medium | `Optional ByVal x As Object = Nothing` + `IsMissing(x)` — no C# equivalent. |
| `WithBlock` | Medium | `.Property` inside `With obj` block. Easy to lose scope in complex nesting. |
| `IntegerDivision` | Medium | `5 \ 2 = 2` (integer) vs `5 / 2 = 2.5` (float). C# uses `/` for both with type inference. |
| `StringConcatenation` | Low | `&` operator for strings; C# uses `+`. Different null handling. |
| `NothingComparison` | Low | `If x Is Nothing` vs C# `x == null`. |
| `DateLiteral` | Medium | `#1/1/2023#` date literal — no C# equivalent. |
| `TypeConversionFunctions` | Medium | `CInt()`, `CStr()`, `CDbl()` — implicit truncation/rounding differs from `(int)` cast. |
| `StringNullEmpty` | Medium | `""` and `Nothing` behave differently in string contexts in VB. |
| `SharedMutableState` | High | `Shared` (static) mutable field — thread safety issue in ASP.NET. |
| `DeclareStatement` | High | `Declare Function` — P/Invoke to unmanaged DLL. |
| `ModuleLevel` | Medium | VB `Module` = static class in C# but has different instantiation semantics. |
| `RedimPreserve` | Medium | `ReDim Preserve arr(newSize)` — O(n) array resize. |
| `GoSub` | High | `GoSub`/`Return` — dead pattern, no C# equivalent. |
| `CharConversion` | Medium | `Asc()`, `Chr()` — explicit character conversion assumptions. |

---

### 40-2: `translate_vbnet_snippet`

**Purpose:** Translate a VB.NET code snippet to C#, with semantic equivalence notes.
Uses LLM (dreaming engine) with a VB.NET-specific system prompt and the quirk catalog
as context. Deterministic pre/post-processing to flag known traps.

**Request struct:**
```rust
pub struct TranslateVbNetSnippetRequest {
    pub project_id: String,
    pub vbnet_code: String,
    /// Additional context: containing class, imports, etc.
    pub context: Option<String>,
    /// Specific translation concerns to highlight.
    pub highlight_concerns: Vec<String>,
    /// Output style for generated C#.
    pub target_style: CSharpStyle, // Modern | NetFramework48 | Net8
}
```

**Returns:**
```rust
pub struct TranslationResult {
    pub csharp_code: String,
    pub semantic_notes: Vec<SemanticNote>,  // "string comparison is now case-sensitive"
    pub unresolved_patterns: Vec<String>,   // patterns that couldn't be translated cleanly
    pub test_suggestions: Vec<String>,      // "verify behavior with empty string input"
}
```

**Deterministic pre-pass:** Before sending to LLM, scan for all known quirks and inject
them as explicit instructions: "This snippet contains `On Error Resume Next` — translate
to a try/catch that logs the exception rather than swallowing it."

---

### 40-3: `analyze_error_handling_coverage`

**Purpose:** Map where error handling exists vs. where it doesn't. In VB.NET legacy code,
`On Error Resume Next` is catastrophic for reliability. `On Error GoTo` creates non-obvious
control flow. Both need to be mapped before I add new code paths.

**Returns per file:**
```rust
pub struct ErrorHandlingCoverage {
    pub file_path: String,
    pub methods: Vec<MethodErrorHandling>,
    pub project_risk_score: f32,
    pub unhandled_sql_methods: Vec<String>,  // methods with SQL but no try/catch
    pub on_error_resume_next_scope: Vec<OnErrorScope>, // exactly which lines are under Resume Next
}

pub struct OnErrorScope {
    pub start_line: u32,
    pub end_line: u32,   // until End Sub / next On Error
    pub methods_in_scope: Vec<String>,
    pub sql_calls_in_scope: usize,
    pub risk_level: String, // "critical" if SQL calls exist in scope
}
```

---

### 40-4: `find_implicit_contracts`

**Purpose:** Infer constraints that callers always respect but that are never checked in
the method body. These are invisible preconditions that, if violated, cause silent failures.

**Detection patterns:**
- Parameter always passed as non-Nothing → infer `NotNull` contract
- Integer parameter always > 0 → infer `Positive` contract
- String parameter always matches a specific pattern (e.g., "INV-XXXXXX") → infer `Format` contract
- Method always called with Session["UserID"] set → infer `RequiresAuthentication` contract

**Returns:**
```rust
pub struct ImplicitContractReport {
    pub contracts: Vec<ImplicitContract>,
}

pub struct ImplicitContract {
    pub method_fqn: String,
    pub param_name: String,
    pub inferred_constraint: String,   // "always non-null", "always positive integer"
    pub confidence: f32,
    pub evidence: Vec<String>,         // call sites that support this inference
    pub risk_if_violated: String,      // "NullReferenceException", "SQL syntax error"
    pub suggested_guard: String,       // "If param Is Nothing Then Throw New ArgumentNullException(...)"
}
```

---

## Phase 41 — WebForms Control Brain {#phase-41}

> **Theme:** WebForms has a complex control lifecycle, mangled client IDs, and UpdatePanel
> magic that makes DOM-level jQuery work completely opaque. This phase makes it transparent.

### 41-1: `render_control_tree`

**Purpose:** Render the complete server-side control hierarchy for a page as a tree.
Critical for understanding what `FindControl("GridView1")` returns, what IDs get rendered,
and what UpdatePanel boundaries contain what controls.

**Returns:**
```
Page: checkout.aspx
└── MasterPage: Site.Master
    ├── ContentPlaceHolder: head
    └── ContentPlaceHolder: MainContent
        ├── UpdatePanel: upOrderSummary [triggers: btnRefresh.Click]
        │   ├── GridView: gvOrderItems [DataKeyNames: OrderItemID]
        │   │   └── TemplateField: [col 3]
        │   │       └── Button: btnRemove [CommandName: Remove, CommandArgument: <%# Eval("OrderItemID") %>]
        │   └── Label: lblTotal
        ├── Button: btnCheckout [CausesValidation: true, ValidationGroup: OrderGroup]
        ├── ValidationSummary: vsOrder [ValidationGroup: OrderGroup]
        └── HiddenField: hdnCartID
```

Plus per-control:
- Rendered ClientID (computed from naming container hierarchy)
- jQuery selector to target it
- Event handlers bound to it (from `Handles` clauses)
- ViewState enabled/disabled

---

### 41-2: `resolve_webforms_clientid`

**Purpose:** Two-way ClientID resolver.

**Forward** (server → client): Given a control's server-side ID and page, return the
rendered HTML client ID and jQuery selector.

**Reverse** (client → server): Given a jQuery selector like
`$('#ctl00_MainContent_UpdatePanel1_gvItems_ctl03_btnEdit')`, return the server-side
control path: `checkout.aspx > UpdatePanel1 > gvItems > Row[3] > btnEdit (ButtonField)`.

**Request struct:**
```rust
pub struct ResolveClientIdRequest {
    pub project_id: String,
    pub direction: ClientIdDirection,
}

pub enum ClientIdDirection {
    Forward {
        aspx_file: String,
        server_control_id: String,          // e.g., "gvItems"
        naming_container_path: Vec<String>, // e.g., ["MainContent", "UpdatePanel1"]
    },
    Reverse {
        jquery_selector: String,  // e.g., "#ctl00_MainContent_gvItems"
    },
}
```

**Returns:**
```rust
pub struct ClientIdResolution {
    pub server_control_id: String,
    pub client_id: String,
    pub jquery_selector: String,    // "#ctl00_MainContent_gvItems"
    pub naming_container_chain: Vec<String>,
    pub aspx_file: String,
    pub control_type: String,       // "GridView", "Button", etc.
    pub is_inside_updatepanel: bool,
    pub updatepanel_id: Option<String>,
    pub event_handlers: Vec<String>,
}
```

---

### 41-3: `detect_viewstate_bloat`

**Purpose:** Identify controls and data that are inflating the ViewState to an unreasonable
size. ViewState bloat is a common WebForms performance killer.

**Heuristics for detection:**
- `GridView` / `DataGrid` with `EnableViewState=true` and large data sources
- `DataSet` stored in ViewState directly
- Large string values serialized into ViewState
- Nested repeaters with ViewState enabled at each level
- Controls with `ViewState["key"] = largeObject`

**Returns per page:**
- Estimated ViewState size (from control analysis, not runtime)
- Top 5 ViewState contributors
- Safe-to-disable ViewState recommendations
- Controls where `EnableViewState=False` would not break functionality

---

### 41-4: `trace_postback_chain`

**Purpose:** Given a control event (e.g., `Button1.Click`), trace the complete server-side
execution chain including UpdatePanel partial rendering.

**For a standard button click:**
```
User clicks btnSave
  → __doPostBack('ctl00$MainContent$btnSave', '')
  → Page.IsPostBack = True
  → Page_Load fires (IsPostBack branch)
  → btnSave_Click fires
      → SaveOrder() called
          → sp_SaveOrder executed (Orders table: INSERT)
          → TR_Orders_Audit trigger fires (AuditLog: INSERT)
      → Session["LastOrderID"] = orderId
      → Response.Redirect("confirmation.aspx")
```

**Returns:** Execution chain as a structured trace with timing estimates and risk flags
(e.g., "Response.Redirect inside try/catch will throw ThreadAbortException — known VB.NET issue").

---

### 41-5: `find_updatepanel_boundaries`

**Purpose:** Map all UpdatePanel regions, their triggers (explicit and implicit), and
what happens when partial rendering fires.

**Returns per page:**
- UpdatePanel ID and ContentTemplate contents
- Trigger list: `AsyncPostBackTrigger`, `PostBackTrigger`
- Controls inside the panel that cause implicit triggers
- Nested UpdatePanel detection (problematic pattern)
- ScriptManager configuration
- Whether `EnablePartialRendering=false` is set (rare but breaks everything)

---

## Phase 42 — Agentic Supercharger {#phase-42}

> **Theme:** Stop being a code-editing assistant. Become a co-developer that plans,
> generates, verifies, and iterates. These are the "crazy" features.

### 42-1: `one_shot_feature_plan` ⭐ GAME-CHANGER

**Purpose:** Given a natural language feature description, produce a complete, file-level
implementation plan that I can execute directly.

**Request struct:**
```rust
pub struct OneShotFeaturePlanRequest {
    pub project_id: String,
    pub feature_description: String,
    /// Optional: reference to a similar existing feature to model after.
    pub model_after: Option<String>,
    /// Hint about which part of the app this feature belongs to.
    pub area_hint: Option<String>,  // e.g., "checkout", "user management", "reporting"
    /// Target technology stack.
    pub target_stack: String, // "webforms_vbnet" | "aspnet_mvc_csharp" | "blazor"
    /// How detailed should implementation steps be?
    pub detail_level: PlanDetailLevel, // Sketch | Detailed | ReadyToCode
}
```

**Process:**
1. **Intent Analysis** (LLM): Parse the feature description into structured requirements:
   - Affected UI pages, controls, events
   - New/modified DB tables and columns
   - New/modified stored procedures
   - Session/ViewState requirements
   - Authentication requirements
   - jQuery/AJAX interactions

2. **Pattern Discovery** (graph): Search the existing codebase for similar features.
   E.g., "add discount code support to checkout" → find how promo codes are already
   handled, if at all; find how similar form-submission-with-validation works.

3. **Impact Analysis** (graph): Which existing files will need modification?
   Run `check_edit_safety` on each.

4. **SQL Schema Integration**: What schema changes are needed? Generate DDL.

5. **Plan Assembly** (LLM + graph context): Generate the implementation plan.

**Returns:** `FeatureImplementationPlan`:
```rust
pub struct FeatureImplementationPlan {
    pub feature_name: String,
    pub summary: String,
    pub estimated_complexity: String,  // "small" | "medium" | "large"

    pub schema_changes: Vec<SchemaChange>,
    pub new_files: Vec<PlannedNewFile>,
    pub modified_files: Vec<PlannedFileModification>,
    pub implementation_order: Vec<String>, // ordered list of steps

    pub existing_patterns_to_follow: Vec<PatternExample>,
    pub risks: Vec<String>,
    pub pre_conditions: Vec<String>,  // things that must be true before starting
    pub acceptance_criteria: Vec<String>,

    pub ready_to_code_steps: Vec<ReadyToCodeStep>, // if detail_level = ReadyToCode
}

pub struct ReadyToCodeStep {
    pub step_number: usize,
    pub file_path: String,
    pub action: String,               // "Add method", "Modify SQL", "Add control"
    pub description: String,          // what to do in plain English
    pub code_template: Option<String>,// skeleton code to start from
    pub validation: String,           // how to verify this step is correct
}
```

**Example output for "Add a discount code field to the checkout page":**
```
Step 1: Schema — Add DiscountCodes table
  File: database.sql (append)
  Action: CREATE TABLE DiscountCodes (Code varchar(20) PK, ...)

Step 2: SP — Create sp_ValidateDiscountCode
  File: database.sql (append)
  Template: CREATE PROCEDURE sp_ValidateDiscountCode @Code VARCHAR(20), @OrderTotal DECIMAL ...

Step 3: checkout.aspx — Add TextBox + Button
  File: checkout.aspx
  Action: Add inside upOrderSummary UpdatePanel (UpdatePanel boundary detected — this will be AJAX)
  Template: <asp:TextBox ID="txtDiscount" ... /><asp:Button ID="btnApplyDiscount" .../>

Step 4: checkout.aspx.vb — Add btnApplyDiscount_Click handler
  File: checkout.aspx.vb
  Action: Add method after existing btnCheckout_Click
  Template: [complete VB.NET method skeleton]
  Pre-check: call get_method_edit_context(checkout.aspx.vb, btnCheckout_Click) first

Step 5: checkout.aspx.vb — Update Page_Load to restore discount from Session
  File: checkout.aspx.vb
  Action: Modify Page_Load IsPostBack=False branch
  Risk: Session["DiscountCode"] — add to global state map
```

---

### 42-2: `find_implementation_pattern`

**Purpose:** "How does this codebase do X?" — Find existing examples of a pattern
I can model my new code after. Never write from scratch when the codebase already
has a working pattern.

**Request struct:**
```rust
pub struct FindImplementationPatternRequest {
    pub project_id: String,
    pub pattern_description: String,  // "form validation with server-side postback"
                                       // "GridView with edit/delete row buttons"
                                       // "ADO.NET stored procedure call with output param"
                                       // "jQuery AJAX calling a WebMethod"
    pub top_k: usize,                 // default 3
}
```

**Process:**
1. Vector search on pattern description → candidate files
2. Graph search for structural patterns (e.g., "GridView with RowCommand handler")
3. LLM-powered code extraction from candidate files
4. Rank by completeness and recency (git history)

**Returns:**
```rust
pub struct PatternMatch {
    pub file_path: String,
    pub description: String,          // "This is how checkout.aspx does form validation"
    pub code_excerpt: String,         // the relevant code snippet
    pub usage_frequency: usize,       // how many times this pattern appears in project
    pub notes: Vec<String>,           // e.g., "Note: this uses Session for state, not ViewState"
    pub can_copy_directly: bool,      // false if it has bespoke dependencies
}
```

**Example patterns it can find:**
- "How does this project authenticate a user?"
- "How does this project call stored procedures with parameters?"
- "How does this project use GridView with paging?"
- "How does this project do master-page content injection?"
- "How does this project do AJAX file upload?"
- "How does this project send emails?"
- "How does this project log errors?"
- "How does this project cache database results?"

---

### 42-3: `generate_pre_edit_tests`

**Purpose:** Before I edit a method, generate characterization tests that capture its
current behavior. If my edit breaks anything, the tests will catch it.

**Unlike `generate_characterization_tests` (Phase 30)**, this tool:
1. Generates tests specifically for the method I'm about to edit (not the whole file)
2. Includes test cases derived from actual call sites (real parameters observed)
3. Includes boundary conditions derived from the method's implicit contracts
   (from `find_implicit_contracts`)
4. Generates the test in the same test framework already used in the project

**Request struct:**
```rust
pub struct GeneratePreEditTestsRequest {
    pub project_id: String,
    pub file_path: String,
    pub method_name: String,
    pub class_name: Option<String>,
    /// Extract parameter examples from actual call sites in the codebase.
    #[serde(default = "default_true")]
    pub use_real_call_site_params: bool,
    /// Test framework to target.
    pub test_framework: Option<String>, // auto-detect if None
}
```

**Returns:**
```rust
pub struct PreEditTestSuite {
    pub test_file_path: String,          // suggested path for the test file
    pub test_code: String,               // complete, runnable test file
    pub test_count: usize,
    pub coverage_rationale: String,      // why these specific test cases were chosen
    pub real_param_examples_used: usize, // how many came from actual call sites
    pub boundary_cases_added: usize,     // null/empty/zero/max edge cases
    pub mocking_requirements: Vec<String>, // "requires SqlConnection mock" etc.
}
```

---

### 42-4: `verify_semantic_equivalence`

**Purpose:** After I've edited a method, verify the new version handles all the cases
the original did. Catch semantic regressions before they reach production.

**Request struct:**
```rust
pub struct VerifySemanticEquivalenceRequest {
    pub project_id: String,
    pub original_code: String,     // the original method body
    pub new_code: String,          // the new method body
    pub language: String,          // "vb.net" | "csharp"
    pub method_context: Option<String>, // surrounding class/imports for context
}
```

**Returns:** `SemanticEquivalenceReport`:
```rust
pub struct SemanticEquivalenceReport {
    pub verdict: String,             // "equivalent" | "functionally_changed" | "behavior_risk"
    pub confidence: f32,
    pub preserved_behaviors: Vec<String>,   // "returns null when input is empty"
    pub changed_behaviors: Vec<String>,     // "now throws instead of returning null"
    pub potential_regressions: Vec<String>, // "On Error Resume Next was removed — exceptions now propagate"
    pub new_behaviors: Vec<String>,         // "now validates input before processing"
    pub test_suggestions: Vec<String>,      // specific test cases to add
}
```

**Analysis dimensions:**
- Return value handling (null returns, exception throwing vs returning)
- Error handling changes (On Error Resume Next removal, try/catch additions)
- SQL query changes (cross-referenced via `validate_sql_fragment`)
- Session state changes (new keys written/read)
- Control flow (early returns, missing else branches)
- String comparison semantics (VB case-insensitive → C# case-sensitive)

---

### 42-5: `generate_rollback_plan`

**Purpose:** Before a risky multi-file edit, generate a structured rollback plan.

**Request struct:**
```rust
pub struct GenerateRollbackPlanRequest {
    pub project_id: String,
    pub planned_changes: Vec<PlannedChange>,
}

pub struct PlannedChange {
    pub file_path: String,
    pub change_description: String,
    pub change_type: String,  // "add_method" | "modify_method" | "add_table" | "modify_sp" ...
}
```

**Returns:**
```rust
pub struct RollbackPlan {
    pub rollback_steps: Vec<RollbackStep>,
    pub db_rollback_sql: Option<String>,  // ALTER TABLE DROP COLUMN etc.
    pub estimated_rollback_time: String,
    pub things_that_cannot_be_rolled_back: Vec<String>, // e.g., sent emails
    pub recommended_snapshot_points: Vec<String>,        // git commit after each phase
}
```

---

### 42-6: `consistency_check`

**Purpose:** After making a set of edits, verify cross-file consistency. Did I update
all the callers? Did I forget to update the corresponding stored procedure? Did I break
any contracts?

**Request struct:**
```rust
pub struct ConsistencyCheckRequest {
    pub project_id: String,
    pub changed_files: Vec<String>,   // files that were just modified
    /// Re-index these files before checking.
    #[serde(default = "default_true")]
    pub re_index_first: bool,
}
```

**Returns:** `ConsistencyCheckResult`:
```rust
pub struct ConsistencyCheckResult {
    pub verdict: String,       // "consistent" | "inconsistencies_found"
    pub issues: Vec<ConsistencyIssue>,
    pub stale_references: Vec<String>,    // callers that reference old signature
    pub missing_updates: Vec<String>,     // "You renamed GetOrder but sp_GetOrder still has old params"
    pub new_dead_code_created: Vec<String>, // methods that became unreachable after your change
}
```

---

### 42-7: `backlog_resolver` ⭐ MOST AMBITIOUS

**Purpose:** Give me a backlog item (bug report, feature request, or TODO comment),
and I return a complete, ready-to-implement solution using all of Engram's knowledge
about the codebase. This is the one-shot implementation oracle.

**Request struct:**
```rust
pub struct BacklogResolverRequest {
    pub project_id: String,
    pub item: BacklogItem,
    /// How deeply to analyze before proposing a solution.
    pub analysis_depth: AnalysisDepth, // Quick | Standard | Deep
}

pub struct BacklogItem {
    pub item_type: BacklogItemType,
    pub title: String,
    pub description: String,
    pub priority: Option<String>,
    pub reported_by: Option<String>,
    pub related_files: Vec<String>,    // files the reporter mentioned
    pub error_message: Option<String>, // if it's a bug with a stack trace
    pub steps_to_reproduce: Vec<String>,
}

pub enum BacklogItemType {
    Bug,
    Feature,
    TechDebt,
    Performance,
    Security,
    Todo { source_file: String, line: u32 },
}
```

**Process for `Bug` type:**
1. If `error_message` is present → run `analyze_error_stack`
2. Identify the most likely root cause method via graph traversal
3. Run `get_method_edit_context` on the root cause location
4. Check if similar bugs were fixed before (git history search)
5. Synthesize a proposed fix using `find_implementation_pattern`

**Process for `Feature` type:**
1. Run `one_shot_feature_plan` with full project context
2. Check `find_implementation_pattern` for similar features already built
3. Validate schema impact with `validate_sql_fragment`
4. Check `check_edit_safety` on all files to be modified

**Returns:** `BacklogResolution`:
```rust
pub struct BacklogResolution {
    pub item_summary: String,
    pub root_cause_analysis: Option<String>,    // for bugs
    pub proposed_solution: String,
    pub implementation_plan: FeatureImplementationPlan,
    pub confidence: f32,
    pub unknowns: Vec<String>,   // things that need human input/decision
    pub risks: Vec<String>,
    pub estimated_effort: String, // "~30 min" | "~2 hours" | "~half-day"
    pub similar_past_fixes: Vec<String>, // from git history
}
```

---

## Phase 43 — Codebase Oracle {#phase-43}

> **Theme:** Natural language queries about the codebase. No more "what file does X?" hunting.

### 43-1: `ask_codebase`

**Purpose:** Natural language Q&A about the codebase. The answer is synthesized from
the graph, DocStore, git history, and schema — not hallucinated.

**Request struct:**
```rust
pub struct AskCodebaseRequest {
    pub project_id: String,
    pub question: String,
    /// Include source citations in the answer.
    #[serde(default = "default_true")]
    pub include_citations: bool,
    /// How many sources to synthesize from.
    #[serde(default = "default_10")]
    pub max_sources: usize,
}
```

**Example questions it can answer:**
- "How does user authentication work in this application?"
- "What happens when a user adds an item to their cart?"
- "Which stored procedures does the reporting module use?"
- "What tables are involved in the order fulfillment process?"
- "Why is Session being used for X instead of ViewState?"
- "What does the GIS mapping feature do?"
- "Which pages access the Users table directly?"
- "When was the last time OrderService was significantly changed?"
- "Are there any TODO comments related to the payment system?"
- "What third-party libraries does this application depend on?"
- "How does this app handle concurrent updates to the same order?"

**Returns:**
```rust
pub struct CodebaseAnswer {
    pub answer: String,
    pub confidence: f32,
    pub citations: Vec<Citation>,    // file:line evidence for each claim
    pub related_questions: Vec<String>, // suggested follow-up questions
    pub uncertainty_notes: Vec<String>, // things the analysis couldn't determine
}
```

**Implementation:** Two-stage retrieval:
1. Graph-structured query (known entities, known relationships)
2. Vector similarity search for narrative context
3. LLM synthesis with citations from both sources

---

### 43-2: `trace_user_request`

**Purpose:** Full end-to-end trace of an HTTP request, from URL to database and back.
The ultimate "how does this feature work?" tool.

**Request struct:**
```rust
pub struct TraceUserRequestRequest {
    pub project_id: String,
    /// URL pattern or page name.
    pub entry_point: String,       // "checkout.aspx" or "/checkout" or "POST checkout.aspx"
    /// Specific user action (optional for more focused trace).
    pub action: Option<String>,    // "click btnCheckout" | "submit form" | "load page"
    /// Include database operations in trace.
    #[serde(default = "default_true")]
    pub include_db: bool,
    /// Include trigger side effects.
    #[serde(default = "default_true")]
    pub include_triggers: bool,
}
```

**Returns:** A structured execution trace:
```
Request: POST checkout.aspx (btnCheckout clicked)
│
├─ Routing: checkout.aspx (IsPostBack=true)
├─ Page_Load (IsPostBack branch)
│   └─ RestoreCartFromSession() → Session["Cart"] → CartService.Load()
│       └─ SELECT * FROM CartItems WHERE SessionID = @id [CartItems: READ]
│
├─ btnCheckout_Click (checkout.aspx.vb:142)
│   ├─ ValidateCart() → returns True
│   ├─ ProcessPayment(amount) → PaymentGateway.Charge() [external call]
│   ├─ CreateOrder(cartId, userId)
│   │   └─ EXEC sp_CreateOrder @CartID, @UserID
│   │       ├─ INSERT INTO Orders ... [Orders: WRITE]
│   │       ├─ INSERT INTO OrderItems ... [OrderItems: WRITE]
│   │       ├─ UPDATE CartItems SET Status='Converted' [CartItems: WRITE]
│   │       └─ TR_Orders_Audit fires → INSERT INTO AuditLog [AuditLog: WRITE]
│   ├─ Session["LastOrderID"] = orderId [STATE WRITE]
│   └─ Response.Redirect("confirmation.aspx?id=" + orderId)
│
└─ Response: 302 → confirmation.aspx
```

---

### 43-3: `find_error_swallowing`

**Purpose:** Find all places where exceptions are caught and not re-thrown, not logged,
or silently converted to misleading return values. These are bugs waiting to happen when
I add new code paths.

**Patterns detected:**
```vb
' Pattern 1: Catch with no action
Try
    DoSomething()
Catch ex As Exception
    ' nothing
End Try

' Pattern 2: Catch + false return (silently signals failure)
Try
    ...
    Return True
Catch
    Return False  ← caller never knows what went wrong
End Try

' Pattern 3: On Error Resume Next with no Err.Number check
On Error Resume Next
CallDangerousMethod()
' never checks If Err.Number <> 0

' Pattern 4: Catch specific, swallow general
Catch ex As SqlException
    lblError.Text = ex.Message
    ' falls through, page continues rendering in broken state
```

**Returns:** Per-location risk score + what information is being lost + suggested fix.

---

### 43-4: `find_permission_checks`

**Purpose:** Map all authorization checks in the codebase. Before adding a new page
or feature, understand what the authentication/authorization pattern is.

**Returns:**
```rust
pub struct PermissionCheckMap {
    pub auth_strategy: String,  // "Forms Authentication", "Windows Auth", "Custom"
    pub global_checks: Vec<GlobalAuthCheck>,   // in Global.asax or HttpModules
    pub page_level_checks: Vec<PageAuthCheck>, // per-page checks
    pub method_level_checks: Vec<MethodAuthCheck>,
    pub unprotected_pages: Vec<String>,        // pages with no apparent auth check
    pub unprotected_methods: Vec<String>,      // web methods with no auth
    pub role_based_checks: Vec<RoleCheck>,     // If User.IsInRole("Admin") patterns
    pub custom_check_patterns: Vec<String>,    // bespoke auth patterns found
}
```

---

## Phase 44 — Automated Quality Gates {#phase-44}

> **Theme:** Before I ship code, run automated quality checks that would catch the most
> common categories of bugs in a legacy VB.NET app.

### 44-1: `check_nuget_compatibility`

**Purpose:** Parse the project's `.vbproj`/`.csproj` and report which NuGet packages
will not work on .NET 8 (or the target framework).

**Returns per package:**
- Current version
- .NET 8 compatibility status (✅ / ⚠️ deprecated / ❌ incompatible)
- CVE advisories for the installed version
- Modern replacement recommendation + migration notes
- Whether the package is transitively required

**Curated compatibility table (built-in, no network required):**

| Package | Status | Replacement |
|---------|--------|-------------|
| Microsoft.AspNet.WebFormsDependencyInjection | ❌ | Microsoft.Extensions.DependencyInjection |
| Antlr | ⚠️ | Antlr4 |
| log4net | ⚠️ | Microsoft.Extensions.Logging + Serilog |
| Newtonsoft.Json | ✅ | System.Text.Json (recommended for new code) |
| EntityFramework (v5/v6) | ⚠️ | Entity Framework Core |
| WebGrease | ❌ | Bundler & Minifier / webpack |
| System.Web.* | ❌ | Microsoft.AspNetCore.* |
| Ajax Control Toolkit | ❌ | No direct equivalent — requires redesign |
| DevExpress.Web | ⚠️ | DevExpress.AspNetCore (separate license) |
| Telerik.Web.UI | ⚠️ | Telerik UI for Blazor (separate license) |
| Google.Maps.* (legacy) | ⚠️ | GoogleMapsComponents or JS API directly |

---

### 44-2: `detect_security_vulnerabilities`

**Purpose:** OWASP-style scan for the most common vulnerabilities in ASP.NET WebForms code.

**Categories:**
- **SQL Injection**: Inline SQL with string concatenation, not parameterized
- **XSS**: `Response.Write(userInput)`, `Label.Text = Request.QueryString["x"]` without encoding
- **CSRF**: Forms without ViewStateMAC or anti-forgery tokens
- **Path Traversal**: `File.ReadAllText(Server.MapPath(Request["file"]))` style patterns
- **Insecure Direct Object Reference**: URL parameter used directly as DB key without auth check
- **Session Fixation**: Session not regenerated after login
- **Open Redirect**: `Response.Redirect(Request["returnUrl"])` without validation
- **Sensitive Data in ViewState**: Unencrypted sensitive data in ViewState
- **Debug Information Exposure**: Custom errors disabled, stack traces visible
- **Hardcoded Credentials**: See 44-3

**Returns:** OWASP Top 10 compliance report with file:line for each finding.

---

### 44-3: `find_hardcoded_secrets`

**Purpose:** Scan for hardcoded credentials, connection strings, API keys, and other
secrets that should be in web.config or environment variables.

**Patterns:**
```vb
' Password in code
Dim conn As String = "Server=prod;Database=Orders;User=sa;Password=Admin123!"

' API key literal
Dim apiKey As String = "sk-XXXXXXXXXXXXXXXXXXXXXXXXXXXX"

' Connection string in code (not in web.config)
Dim cs As String = "Data Source=192.168.1.100;Initial Catalog=..."

' Hardcoded admin check
If username = "admin" AndAlso password = "secret" Then
```

**Returns per finding:** File:line, secret type, severity, recommended fix
(e.g., "Move to web.config `<connectionStrings>` section").

---

### 44-4: `calculate_change_risk_score`

**Purpose:** Multi-factor risk score for a proposed set of changes, combining all
available signals into a single number with explanation.

**Input:** List of `(file, method, change_type)` tuples.

**Scoring dimensions (each 0–10, weighted):**
- Blast radius (weight 3.0): how many callers are affected
- Test coverage (weight 2.0): covered = low risk, uncovered = high risk
- Error handling (weight 1.5): On Error Resume Next in scope = +3
- Shared state (weight 1.5): writes to Session/Application = +2
- DB mutation (weight 2.0): writes to tables with triggers = +2
- Async hazards (weight 2.0): .Result/.Wait() in call chain = +4
- Pattern novelty (weight 1.0): no similar pattern in codebase = +1
- Recent churn (weight 1.0): file changed many times recently in git = +1
- Late binding (weight 0.5): Object-typed variables = +0.5

**Returns:** Score (0–100), per-dimension breakdown, human-readable summary,
and recommended mitigation steps before proceeding.

---

### 44-5: `suggest_safe_refactors`

**Purpose:** Identify low-risk refactors that improve maintainability without changing
behavior. I can apply these alongside feature work without introducing risk.

**Patterns detected:**
- Duplicate SQL queries (same SQL in multiple methods → extract to shared SP)
- Magic strings (e.g., `Session("cartItems")` in 12 places → extract to constant)
- Long methods (> 50 lines) with clearly separable sections → suggest extraction points
- Dead code blocks (if false, commented-out code, unreachable after Return)
- Repeated null checks for the same variable → early return pattern
- Copy-pasted event handlers that differ only in one parameter

**Returns:** Per-refactor:
- Description of the refactor
- Estimated risk (should all be "low")
- Files affected
- `check_edit_safety` pre-result
- Suggested implementation sketch

---

## Phase 45 — Edit Session Protocol {#phase-45}

> **Theme:** Turn ad-hoc editing into a structured, verifiable process with pre/post
> snapshots, consistency verification, and automated regression detection.

### 45-1: `begin_edit_session`

**Purpose:** Snapshot the pre-edit state of all files I'm about to touch. Creates a
named "session" that later tools can reference.

**Request struct:**
```rust
pub struct BeginEditSessionRequest {
    pub project_id: String,
    pub session_name: String,        // e.g., "add-discount-code-feature"
    pub files_to_edit: Vec<String>,  // files I plan to touch
    pub description: String,         // what am I doing
}
```

**Actions:**
1. Record content hashes of all listed files → stored in DocStore under `edit_session` namespace
2. Run `get_method_edit_context` on all methods in the listed files → snapshot
3. Run `get_global_state_map` → snapshot current state key inventory
4. Run `calculate_change_risk_score` → baseline risk assessment
5. Generate `pre_edit_checklist` items

**Returns:** `EditSessionHandle` with a session ID used by subsequent tools.

---

### 45-2: `complete_edit_session`

**Purpose:** After making edits, verify consistency and detect regressions.

**Request struct:**
```rust
pub struct CompleteEditSessionRequest {
    pub project_id: String,
    pub session_id: String,
    /// Actually re-index the changed files before checking.
    #[serde(default = "default_true")]
    pub re_index_changed_files: bool,
}
```

**Actions:**
1. Re-index all files in the session
2. Run `consistency_check` on changed files
3. Compare new call graph against snapshot → find broken callers
4. Compare state key inventory → find new/removed keys
5. Run `validate_sql_fragment` on any new SQL in changed files
6. Run `find_dead_methods` for newly created dead code
7. Run `analyze_sync_hazards` on changed files

**Returns:** `EditSessionReport`:
```rust
pub struct EditSessionReport {
    pub session_name: String,
    pub files_changed: usize,
    pub methods_added: usize,
    pub methods_modified: usize,
    pub methods_removed: usize,
    pub consistency_issues: Vec<ConsistencyIssue>,
    pub broken_callers: Vec<String>,
    pub new_dead_methods: Vec<String>,
    pub sql_validation_issues: Vec<SqlValidationError>,
    pub new_state_keys: Vec<String>,
    pub removed_state_keys: Vec<String>,
    pub sync_hazards_introduced: Vec<SyncHazard>,
    pub overall_verdict: String,    // "clean" | "issues_found" | "requires_attention"
    pub post_edit_checklist: Vec<String>,
}
```

---

### 45-3: `detect_incomplete_changes`

**Purpose:** After renaming/refactoring, find all the places that still need updating.
The "did you forget to update the callers?" check.

**Request struct:**
```rust
pub struct DetectIncompleteChangesRequest {
    pub project_id: String,
    pub change_summary: Vec<ChangeRecord>,
}

pub struct ChangeRecord {
    pub change_type: IncompleteChangeType,
    pub old_value: String,
    pub new_value: Option<String>,
}

pub enum IncompleteChangeType {
    RenamedMethod { file: String, class: String },
    RenamedClass,
    AddedRequiredParameter { method: String, param: String },
    RenamedTableColumn { table: String },
    RenamedSessionKey,
    RenamedStoredProcedure,
    RemovedMethod { file: String },
}
```

**Returns:** For each change, the complete list of locations that still reference
the old name, with file:line and the exact text that needs updating.

---

## Cross-Cutting Notes {#cross-cutting}

### Shared Data Structures

All phases share these common types (define in `engram_core/src/agent_types.rs`):

```rust
pub struct CodeLocation {
    pub file_path: String,
    pub line: u32,
    pub column: Option<u32>,
    pub snippet: Option<String>,   // short excerpt for context
}

pub struct ColumnAccess {
    pub table: String,
    pub column: String,
    pub access_type: ColumnAccessType, // Read | Write | ReadWrite
}

pub struct SpCallSite {
    pub sp_name: String,
    pub file_path: String,
    pub line: u32,
    pub params_passed: Vec<String>,
}

pub struct JqueryCaller {
    pub file_path: String,
    pub line: u32,
    pub call_type: String,  // "__doPostBack" | "$.ajax" | "$.post" | "$.get"
    pub url_pattern: String,
}
```

### Caching Strategy

All Phase 38–45 tools should cache results in DocStore under purpose-specific namespaces:

| Tool | Namespace | Retention | Cache Key |
|------|-----------|-----------|-----------|
| `get_method_edit_context` | `method_edit_ctx` | 24h | `project_id:file:method:content_hash` |
| `get_global_state_map` | `global_state_map` | 1h | `project_id:index_generation` |
| `find_dead_methods` | `dead_methods` | 1h | `project_id:graph_generation` |
| `one_shot_feature_plan` | `feature_plans` | KeepForever | `project_id:feature_hash` |
| `find_implementation_pattern` | `pattern_cache` | 24h | `project_id:query_hash` |

### Test Coverage Requirements

Each phase should include integration tests in `engram_server/tests/`:

| Phase | Test file | Minimum tests |
|-------|-----------|---------------|
| 37 wiring | `db_intelligence_test.rs` | 15 |
| 38 safety kit | `pre_edit_safety_test.rs` | 25 |
| 39 db oracle | `database_oracle_test.rs` | 20 |
| 40 vbnet intel | `vbnet_quirks_test.rs` | 30 |
| 41 webforms brain | `webforms_control_test.rs` | 20 |
| 42 supercharger | `agentic_supercharger_test.rs` | 15 |
| 43 oracle | `codebase_oracle_test.rs` | 10 |
| 44 quality gates | `quality_gates_test.rs` | 25 |
| 45 edit session | `edit_session_test.rs` | 15 |

### Priority Order for Implementation

Based on impact vs. effort:

```
┌─────────────────────────────────────────────────────────────────────┐
│  Priority 1 (implement first — highest ROI, lowest effort)          │
│  Phase 37: Wire existing services (5 tools, ~1 day work)            │
│  38-1: get_method_edit_context (1 tool, 3 days — highest value)     │
│  38-5: validate_sql_fragment (1 tool, 2 days — prevents DB bugs)    │
│  39-1: Auto-detect database.sql (no new tool, 1 day)                │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│  Priority 2 (implement next — high value, medium effort)            │
│  38-3: get_global_state_map (session/ViewState map)                 │
│  38-4: find_dead_methods (know what's safe to change boldly)        │
│  38-2: check_edit_safety (prospective breakage analysis)            │
│  40-1: list_vbnet_quirks (VB.NET semantic trap catalog)             │
│  41-1: render_control_tree (control hierarchy visualization)        │
│  41-2: resolve_webforms_clientid (jQuery↔server bridge)            │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│  Priority 3 (high capability, higher effort — "crazy" features)     │
│  42-1: one_shot_feature_plan (full implementation plan)             │
│  42-2: find_implementation_pattern (follow existing patterns)       │
│  42-7: backlog_resolver (one-shot bug/feature resolution)           │
│  43-1: ask_codebase (natural language Q&A)                          │
│  43-2: trace_user_request (HTTP→DB trace)                           │
│  44-2: detect_security_vulnerabilities (OWASP scan)                 │
│  45-1/2: begin/complete_edit_session (structured edit protocol)     │
└─────────────────────────────────────────────────────────────────────┘
```

---

*End of specification. Total new tools: ~45. Estimated total phases: 37 (wiring) + 38-45 (8 phases).*
*All deterministic tools first; LLM-enhanced tools use dreaming engine as already established.*
