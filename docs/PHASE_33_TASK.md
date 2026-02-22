# Phase 33: `analyze_full_project_migration` — From 80% to 100%

## Problem Statement

Phase 32 brought `analyze_full_project_migration` from 60% to 80% by surfacing graph data that extractors already produced (JS analysis, GIS, web.config, service endpoints, Global.asax, anti-patterns, Classic ASP, reports). An AI agent calling this tool now receives a comprehensive report covering **what exists** in a legacy project.

But **80% is not 100%**. The report still has blind spots that would force an AI agent to make follow-up tool calls, guess at migration strategies, or miss critical dependencies entirely. These blind spots fall into eight categories:

1. **No code-behind method inventory** — The report lists pages and their controls but never tells the AI agent what methods exist in each `.aspx.vb`/`.aspx.cs` file, what parameters they take, or what they do. An AI agent writing replacement code cannot produce correct method signatures without this.

2. **No third-party control detection** — The control regex in `webforms.rs` only matches `asp:`, `ajaxToolkit:`, and `custom:` tag prefixes. Real enterprise apps use Telerik (`telerik:`/`rad:`), DevExpress (`dx:`), Infragistics (`ig:`/`igtbl:`/`igmisc:`), ComponentArt (`ComponentArt:`), and Kendo UI (`kendo:`). The control mapping table has 60 standard ASP.NET entries and zero third-party entries. An AI agent encountering `<telerik:RadGrid>` gets zero migration guidance.

3. **Assembly/NuGet references not surfaced** — `solution_parser.rs` already extracts `PackageRef { name, version }` and assembly references from `.csproj`/`.vbproj` files via `parse_project_file()`. This data exists but is never included in the full project migration report. An AI agent doesn't know what packages are installed, what versions they are, or what modern replacements exist.

4. **No OutputCache / caching pattern detection** — The `state_extractor.rs` detects `Cache["key"]` bracket access (emitting `reads_state`/`writes_state` edges) but misses `HttpRuntime.Cache.Insert()`, `HttpRuntime.Cache.Add()`, `HttpContext.Current.Cache.Get()`, and `<%@ OutputCache %>` page directives. The `webforms.rs` extractor has zero OutputCache detection. An AI agent cannot plan cache migration (to `IMemoryCache`/`IDistributedCache`) without knowing what caching strategies the legacy app uses.

5. **No URL routing / rewrite rules** — The `full_project_migration_service.rs` detects `RouteConfig.RegisterRoutes` and `RouteTable.Routes` in Global.asax, but misses classic URL rewriting (`<rewrite>` rules in web.config, `HttpContext.RewritePath()`, third-party rewriters like ISAPI_Rewrite, UrlRewriter.NET). An AI agent needs the complete URL mapping to build correct routing in ASP.NET Core.

6. **No VB.NET→C# translation flags** — The VB extractor detects method names, parameters, COM interop, and error handling, but does not flag VB-specific constructs that require careful C# translation: `Optional` parameters with `IsMissing`, `Module` (static class equivalent), `My.` namespace, `WithEvents`/`Handles` clause, `RaiseEvent`, `Shadows`/`Overloads`, string comparison (`Option Compare Text`), `Nothing` vs `null` semantics, `IsNumeric`/`IsDate` intrinsics, `Like` operator, `ReDim Preserve` (already detected but not surfaced in report), late-bound `Object` calls via `CallByName`. An AI agent doing VB→C# translation needs an explicit inventory of these constructs per file.

7. **No multi-tenancy detection** — Many legacy WebForms SaaS applications implement multi-tenancy through patterns like tenant ID columns in database queries, per-tenant connection strings, subdomain-based routing, session-stored tenant context, or custom `HttpModule` tenant resolution. None of these patterns are detected. An AI agent migrating a SaaS app without knowing it's multi-tenant will produce single-tenant code that breaks in production.

8. **No email/notification + background job patterns** — The VB extractor detects legacy COM `CDO.Message` but not modern `System.Net.Mail.SmtpClient`, `MailMessage`, `Attachment`, or `AlternateView`. No extractor detects background processing patterns: `System.Threading.Timer`, `ThreadPool.QueueUserWorkItem`, `BackgroundWorker`, `Task.Run()`, or Windows Service timer loops. An AI agent needs these to plan migration to `IEmailSender`/SendGrid and `IHostedService`/Hangfire/Quartz.NET.

When all eight gaps are closed, an AI agent calling `analyze_full_project_migration` once will have **everything** it needs to write replacement code for any page in a real-world legacy VB.NET ASP.NET WebForms SaaS application — including correct method signatures, third-party control replacements, package dependencies, caching strategies, URL routing, VB→C# translation notes, tenant-awareness, and background job migration.

---

## Architecture Context

### Key Files

| File | Purpose | Current Size |
|------|---------|-------------|
| `crates/engram_server/src/services/full_project_migration_service.rs` | Main service: analysis + markdown rendering | ~2900 lines |
| `crates/engram_server/src/tools.rs` | Tool handler: file discovery + orchestration | ~8800 lines |
| `crates/engram_index/src/webforms.rs` | ASPX/ASCX markup extraction | ~2000 lines |
| `crates/engram_index/src/vb_extractor.rs` | VB.NET code-behind extraction | ~2500 lines |
| `crates/engram_index/src/state_extractor.rs` | Session/ViewState/Cache state extraction | ~700 lines |
| `crates/engram_index/src/js_extractor.rs` | JavaScript/jQuery extraction | ~2500 lines |
| `crates/engram_index/src/solution_parser.rs` | .sln/.csproj/.vbproj parsing | ~800 lines |
| `crates/engram_index/src/control_mapping.rs` | WebForms→Modern control mapping table | ~1100 lines |
| `crates/engram_server/src/services/pattern_detection_service.rs` | Anti-pattern detection | ~400 lines |
| `crates/engram_server/src/services/dossier_service.rs` | Per-file migration dossier | ~600 lines |
| `crates/engram_graph/src/store.rs` | Graph storage + query API | ~2000 lines |

### Current Data Flow

```
tools.rs (tool handler)
  │
  ├── discover_files_recursive() → ProjectFileBundle
  │     ├── markup_files: Vec<FileContent>      (.aspx, .ascx, .master)
  │     ├── js_files: Vec<(String, String)>      (.js)
  │     ├── classic_asp_files: Vec<(String, String)> (.asp)
  │     ├── report_files: Vec<(String, String)>  (.rdl, .rdlc)
  │     ├── global_asax: Option<FileContent>
  │     ├── web_config_content: Option<String>
  │     └── code_files: Vec<(String, String)>    (.cs, .vb)
  │
  └── full_project_migration_service::analyze_full_project(graph, pid, target, bundle, max)
        │
        ├── migration_order_service::suggest_migration_order()
        ├── state_migration_service::analyze_state_migration()
        ├── auth_config_service::analyze_auth_config()
        ├── db_strategy_service::classify_data_access_patterns()
        ├── dossier_service::build_migration_dossier()  (per file)
        ├── analyze_js_dependencies()          ← Phase 32
        ├── analyze_gis_spatial()              ← Phase 32
        ├── extract_webconfig_inventory()      ← Phase 32
        ├── extract_service_endpoints()        ← Phase 32
        ├── extract_global_asax_info()         ← Phase 32
        ├── detect anti-patterns               ← Phase 32
        ├── analyze Classic ASP                ← Phase 32
        ├── analyze reports                    ← Phase 32
        │
        └── render_markdown() → String
```

### Key Struct Definitions (Current State)

**FullProjectMigrationReport** (`full_project_migration_service.rs` ~line 23):
```rust
#[derive(Debug, Clone, Serialize)]
pub struct FullProjectMigrationReport {
    pub project_id: String,
    pub target_stack: String,
    pub generated_at: String,
    pub migration_order: MigrationOrderPlan,
    pub state_migration: StateMigrationReport,
    pub auth_config: AuthConfigMap,
    pub data_access_profiles: Vec<FileDataAccessProfile>,
    pub page_dossiers: Vec<MigrationDossier>,
    pub cross_cutting: CrossCuttingSummary,
    pub js_analysis: JsAnalysisSummary,
    pub gis_analysis: GisAnalysisSummary,
    pub web_config_inventory: WebConfigInventory,
    pub service_endpoints: ServiceEndpointSummary,
    pub global_asax: GlobalAsaxSummary,
    pub anti_patterns: AntiPatternSummary,
    pub classic_asp: ClassicAspSummary,
    pub reports: ReportSummary,
    pub markdown_report: String,
}
```

**CrossCuttingSummary** (`full_project_migration_service.rs` ~line 56):
```rust
#[derive(Debug, Clone, Serialize)]
pub struct CrossCuttingSummary {
    pub total_pages_analyzed: usize,
    pub complexity_distribution: BTreeMap<String, usize>,
    pub shared_sql_tables: Vec<SharedItem>,
    pub shared_state_keys: Vec<SharedItem>,
    pub shared_user_controls: Vec<SharedItem>,
    pub risk_distribution: BTreeMap<String, usize>,
    pub critical_risk_files: Vec<String>,
    pub total_validators: usize,
    pub total_update_panels: usize,
    pub total_lifecycle_events: usize,
    pub files_with_ispostback: usize,
    pub total_js_files: usize,
    pub total_gis_libraries: usize,
    pub total_anti_patterns: usize,
    pub total_service_endpoints: usize,
    pub total_classic_asp_files: usize,
    pub total_reports: usize,
}
```

**ProjectFileBundle** (`full_project_migration_service.rs` ~line 92):
```rust
pub struct ProjectFileBundle {
    pub markup_files: Vec<FileContent>,
    pub js_files: Vec<(String, String)>,
    pub classic_asp_files: Vec<(String, String)>,
    pub report_files: Vec<(String, String)>,
    pub global_asax: Option<FileContent>,
    pub web_config_content: Option<String>,
    pub code_files: Vec<(String, String)>,
}
```

**FileContent** (`full_project_migration_service.rs` ~line 85):
```rust
pub struct FileContent {
    pub file_path: String,
    pub markup_content: String,
    pub codebehind_content: Option<String>,
}
```

**MigrationDossier** (`dossier_service.rs` ~line 62):
```rust
#[derive(Debug, Clone, Serialize)]
pub struct MigrationDossier {
    pub file_path: String,
    pub page_type: String,
    pub target_stack: String,
    pub inherits_class: Option<String>,
    pub base_class: Option<String>,
    pub codebehind_file: Option<String>,
    pub master_page: Option<String>,
    pub user_controls: Vec<DossierControlRef>,
    pub referenced_files: Vec<String>,
    pub referenced_by: Vec<String>,
    pub shared_modules: Vec<String>,
    pub data_sources: Vec<DossierDataSource>,
    pub sql_statements: Vec<DossierSqlInfo>,
    pub connection_strings_used: Vec<String>,
    pub tables_touched: Vec<String>,
    pub lifecycle_summary: LifecycleSummary,
    pub viewstate_summary: ViewStateSummary,
    pub ajax_summary: AjaxSummary,
    pub validation_summary: ValidationSummary,
    pub auth_summary: AuthSummary,
    pub blast_radius_score: u8,
    pub risk_factors: Vec<String>,
    pub scaffold_preview: Option<String>,
    pub migration_steps: Vec<String>,
    pub estimated_complexity: String,
}
```

### Graph API (from `engram_graph/src/store.rs`)

```rust
// Node struct
pub struct Node {
    pub node_id: String,
    pub node_type: String,       // "function", "class", "file", "insight", etc.
    pub name: String,
    pub namespace: String,
    pub language: String,
    pub file_path: RelPath,
    pub start_line: u32,
    pub end_line: u32,
    pub generation: u64,
    pub metadata: Option<JsonValue>,
}

// Edge struct
pub struct Edge {
    pub source_id: String,
    pub target_id: String,
    pub namespace: String,
    pub language: String,
    pub edge_kind: EdgeKind,
    pub weight: u32,
    pub generation: u64,
    pub metadata: Option<JsonValue>,
    pub updated_at_ms: u64,
}

// Key query methods
fn list_edges_by_kind(&self, project_id: &str, kind: EdgeKind, limit: usize) -> Result<Vec<Edge>>
fn query_nodes(&self, project_id: &str, node_type: Option<&str>, name_pattern: Option<&str>, file_path: Option<&str>, limit: usize) -> Result<Vec<Node>>
```

### Solution Parser API (from `engram_index/src/solution_parser.rs`)

```rust
pub struct PackageRef {
    pub name: String,
    pub version: Option<String>,
}

pub struct ProjectFileInfo {
    pub project_path: String,
    pub root_namespace: Option<String>,
    pub assembly_name: Option<String>,
    pub target_framework: Option<String>,
    pub output_type: Option<String>,
    pub project_references: Vec<String>,
    pub package_references: Vec<PackageRef>,
    pub source_files: Vec<String>,
}

pub struct SolutionStructure {
    pub projects: Vec<SolutionProject>,
    pub project_details: BTreeMap<String, ProjectFileInfo>,
    pub dependency_graph: BTreeMap<String, Vec<String>>,
    pub configurations: Vec<String>,
    pub shared_libraries: Vec<String>,
    pub migration_order: Vec<String>,
    pub warnings: Vec<String>,
}

// Public functions:
pub fn parse_project_file(proj_content: &str, proj_path: &str) -> ProjectFileInfo
pub fn parse_solution(sln_content: &str) -> Vec<SolutionProject>
pub fn build_solution_structure(sln_content: &str, project_contents: &BTreeMap<String, String>) -> SolutionStructure
```

### Existing EdgeKind Variants (33 total, from `engram_graph/src/store.rs`)

```
CoOccurrence, TemporalCoupling, Insight, Dependency, AntiPattern, Contains, Imports,
SqlCalls, HasColumn, ForeignKey, QueriesTable, ReadsState, WritesState, DataBinding,
RegistersControl, IncludesFile, UnresolvedStateRead, UnresolvedStateWrite,
ExposesWebService, ExposesHttpHandler, ExposesWcfService, ContainsUi, UiLayoutNeighbor,
ReadsColumn, RegistersModule, RegistersHandler, ManipulatesDom, TriggersPostback, ApiCall,
ParameterBinding, SpatialCall, StateAffinity, InjectsScript
```

### Existing Control Detection (from `webforms.rs`)

The control detection regex at ~line 347:
```rust
r#"(?i)<(?:asp|ajaxToolkit|custom):[A-Za-z]+\b([^>]*runat\s*=\s*"server"[^>]*)/?>"#
```

This matches ONLY three tag prefixes:
- `asp:` — Standard ASP.NET WebForms controls
- `ajaxToolkit:` — AJAX Control Toolkit
- `custom:` — Generic custom controls

The `control_mapping.rs` table has 60 entries, ALL for `System.Web.UI.WebControls` namespace. Zero third-party mappings.

### Existing VB Extractor Capabilities (from `vb_extractor.rs`)

**Detects**: method names with start/end lines, full parameter signatures (via tree-sitter), return types, access modifiers (Public/Private/Protected), async flag, static flag, effects (State_Access, SQL_Access, DOM_Mutation, COM_Interop), On Error Resume Next / GoTo, late binding (Dim As Object), CreateObject COM interop, ReDim Preserve, CallByName, nested With blocks.

**Does NOT detect**: SmtpClient/MailMessage, ThreadPool/BackgroundWorker/Task.Run, HttpRuntime.Cache patterns, URL routing, Optional parameters with IsMissing, Module declarations, My. namespace usage, WithEvents/Handles, RaiseEvent, Shadows/Overloads, Option Compare, Like operator, IsNumeric/IsDate intrinsics.

### Existing State Extractor Capabilities (from `state_extractor.rs`)

**Detects 8 patterns**: `Session["key"]`, `ViewState["key"]`, `Application["key"]`, `Cache["key"]`, `HttpContext.Current.Items["key"]`, `HttpContext.Current.Session["key"]`, `Request.Cookies["key"]`, `Response.Cookies["key"]`.

**Emits**: `reads_state`, `writes_state`, `state_affinity`, `unresolved_state_read`, `unresolved_state_write` edges.

**Does NOT detect**: `HttpRuntime.Cache.Insert()`, `HttpRuntime.Cache.Add()`, `HttpContext.Current.Cache.Get()`, `Cache.Insert()`, `Cache.Add()`, `Response.Cache.SetExpires()`, `Response.Cache.SetCacheability()`, `<%@ OutputCache %>` directives.

### Existing Pattern Detection (from `pattern_detection_service.rs`)

```rust
pub struct DesignAntiPattern {
    pub pattern_name: String,
    pub description: String,
    pub severity: AntiPatternSeverity,  // Minor, Moderate, Severe
    pub affected_nodes: Vec<String>,
    pub evidence: Vec<String>,
    pub modern_target: String,
    pub refactoring_steps: Vec<String>,
}

pub fn detect_design_antipatterns(
    graph: &GraphStore,
    project_id: &str,
    god_threshold: usize,
    spaghetti_threshold: usize,
    soup_threshold: usize,
) -> Result<Vec<DesignAntiPattern>>
```

Detects 5 patterns: God Object, Spaghetti Events, Session Soup, SqlDataSource Coupling, Tight GIS Coupling.

Does NOT detect: background threading anti-patterns, caching anti-patterns, multi-tenancy anti-patterns.

---

## Gap Inventory (8 Gaps, 31 Sub-tasks)

### Gap 1: Code-Behind Method Inventory

**Current behavior**: The per-page dossier (`MigrationDossier`) lists the `inherits_class`, `codebehind_file`, lifecycle event counts, and blast radius — but never lists the actual methods in the code-behind. The VB extractor DOES produce `ExtractedSymbol` entries for every method with name, start/end lines, parameters, return type, access modifier, and effects metadata. This data is stored in the graph as `function` nodes with the code-behind file path. The report never queries it.

**Why it matters**: An AI agent tasked with writing the modern replacement for `AdminPage.aspx` needs to know:
- What methods exist (`Page_Load`, `btnSave_Click`, `gvUsers_RowCommand`, `LoadUserData`, `ExportToExcel`, etc.)
- What parameters they take (`sender As Object, e As EventArgs` vs `sender As Object, e As GridViewCommandEventArgs`)
- What they return (Sub vs Function returning DataTable)
- Whether they're event handlers (lifecycle events, control events, custom methods)
- What side effects they have (SQL access, state access, COM interop, etc.)

Without this, the AI agent must guess or make a separate `analyze_file_coding_style` call for every single page.

**File**: `crates/engram_server/src/services/full_project_migration_service.rs`

#### Sub-task 1.1: Query method nodes from graph

For each page being analyzed (each `FileContent` in the bundle), query the graph for `function` nodes belonging to the code-behind file:

```rust
fn extract_method_inventory(
    graph: &Arc<GraphStore>,
    project_id: &str,
    codebehind_path: &str,
) -> Vec<MethodInfo>
```

Implementation:
```rust
let method_nodes = graph.query_nodes(
    project_id,
    Some("function"),    // node_type
    None,                // name_pattern (all methods)
    Some(codebehind_path), // file_path
    500,                 // limit
)?;
```

For each method node, extract metadata fields:
- `node.name` → method name
- `node.start_line` / `node.end_line` → line range (size indicator)
- `node.metadata["signature"]` → full signature string
- `node.metadata["return_type"]` → return type
- `node.metadata["params"]` → parameter list
- `node.metadata["access_level"]` → Public/Private/Protected
- `node.metadata["effects"]` → comma-separated effect list
- `node.metadata["lifecycle_stage"]` → Init/Load/Event/PreRender etc.

Also check for edges FROM this method node:
- `SqlCalls` edges → method accesses SQL
- `ReadsState`/`WritesState` edges → method accesses session/cache
- `ManipulatesDom` edges → method does DOM manipulation
- `SpatialCall` edges → method calls GIS APIs

#### Sub-task 1.2: Build `MethodInfo` struct

```rust
#[derive(Debug, Clone, Serialize)]
pub struct MethodInfo {
    pub name: String,
    pub signature: String,              // "Protected Sub Page_Load(sender As Object, e As EventArgs)"
    pub return_type: String,            // "Sub" / "Function As DataTable" / "void" / "Task"
    pub access_level: String,           // "Public" / "Protected" / "Private"
    pub line_range: (u32, u32),         // (start, end)
    pub line_count: u32,                // end - start + 1
    pub method_kind: MethodKind,        // Lifecycle, ControlEvent, Helper, DataAccess, WebMethod
    pub effects: Vec<String>,           // ["SQL_Access", "State_Access", "COM_Interop"]
    pub calls_methods: Vec<String>,     // other methods this method calls (from Dependency edges)
    pub called_by: Vec<String>,         // methods that call this method
}

#[derive(Debug, Clone, Serialize)]
pub enum MethodKind {
    Lifecycle,      // Page_Load, Page_Init, Page_PreRender, etc.
    ControlEvent,   // btnSave_Click, gvUsers_RowCommand, etc.
    WebMethod,      // <WebMethod()> decorated methods
    DataAccess,     // Methods with SQL_Access effect
    Helper,         // Private/internal utility methods
    Unknown,
}
```

Classify `MethodKind` by:
- Lifecycle: name matches `Page_Load|Page_Init|Page_PreRender|Page_Unload|OnInit|OnLoad|OnPreRender`
- ControlEvent: name matches `\w+_(Click|Command|RowCommand|SelectedIndexChanged|TextChanged|CheckedChanged|DataBound|RowEditing|RowUpdating|RowDeleting|PageIndexChanging|Sorting|ItemCommand)`
- WebMethod: metadata contains "WebMethod" or graph has `ExposesWebService` edge from this node
- DataAccess: effects contains "SQL_Access"
- Helper: all others

#### Sub-task 1.3: Build `PageMethodInventory` per page

```rust
#[derive(Debug, Clone, Serialize)]
pub struct PageMethodInventory {
    pub file_path: String,
    pub codebehind_path: String,
    pub total_methods: usize,
    pub methods: Vec<MethodInfo>,
    pub lifecycle_methods: usize,
    pub event_handlers: usize,
    pub web_methods: usize,
    pub data_access_methods: usize,
    pub helper_methods: usize,
    pub largest_method: Option<(String, u32)>,  // (name, line_count)
    pub methods_with_sql: usize,
    pub methods_with_state: usize,
}
```

Collect this for every page in the bundle. Store in a `BTreeMap<String, PageMethodInventory>` keyed by file path.

#### Sub-task 1.4: Add to `FullProjectMigrationReport`

Add field:
```rust
pub method_inventories: BTreeMap<String, PageMethodInventory>,
```

#### Sub-task 1.5: Add to `CrossCuttingSummary`

Add fields:
```rust
pub total_methods: usize,
pub total_event_handlers: usize,
pub total_web_methods: usize,
pub largest_file_by_methods: Option<(String, usize)>,  // (path, count)
```

#### Sub-task 1.6: Render method inventory in per-page dossier

In the "Page-by-Page Dossiers" markdown section, for each page, add after the existing dossier fields:

```markdown
**Methods** ({total} total: {lifecycle} lifecycle, {event} event handlers, {helper} helpers)

| Method | Kind | Lines | Effects | Signature |
|--------|------|-------|---------|-----------|
| Page_Load | Lifecycle | 23 | SQL_Access, State_Access | Protected Sub Page_Load(sender As Object, e As EventArgs) |
| btnSave_Click | ControlEvent | 45 | SQL_Access | Protected Sub btnSave_Click(sender As Object, e As EventArgs) |
| LoadUserData | Helper | 18 | SQL_Access | Private Function LoadUserData(userId As Integer) As DataTable |
| gvUsers_RowCommand | ControlEvent | 31 | SQL_Access, State_Access | Protected Sub gvUsers_RowCommand(sender As Object, e As GridViewCommandEventArgs) |
```

#### Sub-task 1.7: Render aggregate method summary

Add a new top-level section after "Data Access Patterns":

```markdown
## Code-Behind Method Inventory

**Total methods**: {total} across {file_count} code-behind files
**Lifecycle handlers**: {count} | **Event handlers**: {count} | **WebMethods**: {count} | **Helpers**: {count}
**Largest code-behind**: AdminPage.aspx.vb ({count} methods, {lines} lines)

### Files by Method Count (top 10)
| File | Methods | Events | SQL Methods | Largest Method |
|------|---------|--------|-------------|----------------|
| AdminPage.aspx.vb | 47 | 12 | 23 | ExportToExcel (145 lines) |
| Dashboard.aspx.vb | 31 | 8 | 15 | LoadChartData (89 lines) |

### Migration Complexity Indicators
- {count} methods > 50 lines → candidates for decomposition
- {count} methods with SQL_Access → need repository extraction
- {count} methods with COM_Interop → need modern library replacement
- {count} WebMethods → must become API endpoints
```

---

### Gap 2: Third-Party Control Detection & Mapping

**Current behavior**: The control regex in `webforms.rs` (~line 347) only matches `asp:`, `ajaxToolkit:`, and `custom:` tag prefixes:

```rust
r#"(?i)<(?:asp|ajaxToolkit|custom):[A-Za-z]+\b([^>]*runat\s*=\s*"server"[^>]*)/?>"#
```

The `control_mapping.rs` table has 60 entries, all for `System.Web.UI.WebControls`. There are zero mappings for Telerik, DevExpress, Infragistics, ComponentArt, or Kendo UI controls.

The `<%@ Register %>` directive parser in `webforms.rs` DOES capture `Assembly` and `Namespace` attributes, but only for resolving user controls to `.ascx` files — it does not use this data to identify vendor controls.

**Why it matters**: In a 15-year-old enterprise WebForms app, third-party control suites are ubiquitous. Telerik RadGrid alone is used in thousands of production apps. An AI agent that encounters `<telerik:RadGrid>` or `<dx:ASPxGridView>` in markup needs to know:
- What it is (a data grid with built-in paging, filtering, sorting, grouping, export)
- What to replace it with (MudBlazor DataGrid, AG Grid, DevExtreme Grid)
- What properties to map (AllowSorting → SortMode, AllowPaging → pagination)
- What events to map (OnNeedDataSource → OnParametersSetAsync, OnItemCommand → RowClick)

**Files to modify**:
- `crates/engram_index/src/webforms.rs` — expand control regex
- `crates/engram_index/src/control_mapping.rs` — add third-party mappings
- `crates/engram_server/src/services/full_project_migration_service.rs` — surface in report

#### Sub-task 2.1: Expand control detection regex in `webforms.rs`

Replace the control regex (~line 347) from:
```rust
r#"(?i)<(?:asp|ajaxToolkit|custom):[A-Za-z]+\b([^>]*runat\s*=\s*"server"[^>]*)/?>"#
```

To:
```rust
r#"(?i)<(?:asp|ajaxToolkit|custom|telerik|rad|dx|ig|igtbl|igmisc|igsch|ComponentArt|kendo|obout|eo|FarPoint|Dart|cwc|ntx):[A-Za-z]+\b([^>]*(?:runat\s*=\s*"server")?[^>]*)/?>"#
```

This adds detection for:
- `telerik:` / `rad:` — Telerik UI for ASP.NET AJAX (RadGrid, RadComboBox, RadEditor, etc.)
- `dx:` — DevExpress ASP.NET controls (ASPxGridView, ASPxTextBox, etc.)
- `ig:` / `igtbl:` / `igmisc:` / `igsch:` — Infragistics WebDataGrid, UltraWebGrid, etc.
- `ComponentArt:` — ComponentArt Web.UI (Grid, TreeView, etc.)
- `kendo:` — Telerik Kendo UI for ASP.NET (newer Telerik suite)
- `obout:` — Obout Suite controls
- `eo:` — EO.WebControls
- `FarPoint:` — FarPoint Spread for ASP.NET
- `Dart:` — Dart PowerWEB controls
- `cwc:` — Custom Web Controls (common generic prefix)
- `ntx:` — NetAdvantage controls (Infragistics alternate prefix)

**Important**: Some third-party controls don't require `runat="server"` in markup (it's implied). Make the `runat="server"` portion optional in the regex for the new prefixes — but keep it required for `asp:` to avoid false positives with HTML5 custom elements.

Also update the HTML control regex to NOT match these prefixes (they are server controls, not HTML controls).

#### Sub-task 2.2: Detect vendor from `<%@ Register %>` directives

In `webforms.rs`, the register directive parser already captures `Assembly` and `Namespace` attributes. Add vendor classification logic:

```rust
fn classify_vendor_from_register(assembly: &str, namespace: &str) -> Option<VendorInfo> {
    // Match by assembly name (case-insensitive)
    let assembly_lower = assembly.to_lowercase();
    if assembly_lower.contains("telerik") { return Some(VendorInfo { vendor: "Telerik", suite: "UI for ASP.NET AJAX" }); }
    if assembly_lower.contains("devexpress") { return Some(VendorInfo { vendor: "DevExpress", suite: "ASP.NET Controls" }); }
    if assembly_lower.contains("infragistics") { return Some(VendorInfo { vendor: "Infragistics", suite: "Ultimate UI" }); }
    if assembly_lower.contains("componentart") { return Some(VendorInfo { vendor: "ComponentArt", suite: "Web.UI" }); }
    if assembly_lower.contains("kendo") { return Some(VendorInfo { vendor: "Telerik", suite: "Kendo UI" }); }
    // etc.
    None
}
```

Store detected vendor info as metadata on the `RegistersControl` edge or as an `insight` node.

#### Sub-task 2.3: Add third-party control mappings to `control_mapping.rs`

Add at minimum 30 new entries to the `CONTROL_MAPPINGS` table for the most common third-party controls:

**Telerik (12 entries)**:
| Legacy Control | Blazor Equivalent | React Equivalent |
|---------------|-------------------|-----------------|
| RadGrid | MudDataGrid / TelerikGrid | AG Grid / React Table |
| RadComboBox | MudAutocomplete / TelerikDropDownList | React Select |
| RadEditor | MudRichTextEditor / TelerikEditor | TinyMCE / CKEditor |
| RadTreeView | MudTreeView / TelerikTreeView | React Arborist |
| RadTabStrip | MudTabs / TelerikTabStrip | React Tabs |
| RadMenu | MudMenu / TelerikMenu | React Menu |
| RadUpload | MudFileUpload / TelerikUpload | React Dropzone |
| RadScheduler | MudScheduler / TelerikScheduler | FullCalendar |
| RadChart | MudChart / TelerikChart | Recharts / Chart.js |
| RadDatePicker | MudDatePicker / TelerikDatePicker | React DatePicker |
| RadWindow | MudDialog / TelerikWindow | React Modal |
| RadPanelBar | MudExpansionPanels / TelerikPanelBar | React Accordion |

**DevExpress (8 entries)**:
| Legacy Control | Blazor Equivalent | React Equivalent |
|---------------|-------------------|-----------------|
| ASPxGridView | DxGrid | DevExtreme DataGrid |
| ASPxTextBox | DxTextBox / MudTextField | Material UI TextField |
| ASPxComboBox | DxComboBox / MudSelect | React Select |
| ASPxDateEdit | DxDateEdit / MudDatePicker | React DatePicker |
| ASPxPopupControl | DxPopup / MudDialog | React Modal |
| ASPxTreeList | DxTreeList | DevExtreme TreeList |
| ASPxRichEdit | DxRichEdit | TinyMCE |
| ASPxPivotGrid | DxPivotGrid | DevExtreme PivotGrid |

**Infragistics (6 entries)**:
| Legacy Control | Blazor Equivalent | React Equivalent |
|---------------|-------------------|-----------------|
| WebDataGrid / UltraWebGrid | IgbGrid | IgrDataGrid |
| WebTab | IgbTabs / MudTabs | React Tabs |
| WebTree | IgbTree / MudTreeView | React Arborist |
| WebDialogWindow | IgbDialog / MudDialog | React Modal |
| WebDatePicker | IgbDatePicker / MudDatePicker | React DatePicker |
| WebChart | IgbCategoryChart | IgrCategoryChart |

**ComponentArt (4 entries)**:
| Legacy Control | Blazor Equivalent | React Equivalent |
|---------------|-------------------|-----------------|
| Grid | MudDataGrid | AG Grid |
| TreeView | MudTreeView | React Arborist |
| Menu | MudMenu | React Menu |
| TabStrip | MudTabs | React Tabs |

Each entry must follow the existing `ControlMapping` struct format with `properties_map`, `event_map`, `data_binding_pattern`, and `notes` fields populated with real mappings.

#### Sub-task 2.4: Build `ThirdPartyControlSummary` in the report

```rust
#[derive(Debug, Clone, Serialize)]
pub struct ThirdPartyControlSummary {
    pub vendors_detected: Vec<VendorSummary>,
    pub total_third_party_controls: usize,
    pub files_with_third_party: Vec<String>,
    pub unmapped_controls: Vec<UnmappedControl>,  // controls with no mapping entry
}

#[derive(Debug, Clone, Serialize)]
pub struct VendorSummary {
    pub vendor: String,           // "Telerik", "DevExpress", etc.
    pub suite: String,            // "UI for ASP.NET AJAX"
    pub control_count: usize,
    pub controls_used: Vec<(String, usize)>,  // (control_name, usage_count)
    pub files: Vec<String>,
    pub modern_replacement_suite: String,  // "Telerik UI for Blazor" / "MudBlazor"
    pub license_note: String,    // "Commercial license required for modern suite"
}

#[derive(Debug, Clone, Serialize)]
pub struct UnmappedControl {
    pub tag_name: String,        // "telerik:RadSpell"
    pub vendor: String,
    pub file_path: String,
    pub note: String,            // "No direct modern equivalent — consider removing or custom implementation"
}
```

To populate this:
1. Scan all `markup_files` for the expanded control regex
2. Group detected controls by vendor prefix
3. Look up each control in the expanded `control_mapping.rs` table
4. Any control NOT in the mapping table → add to `unmapped_controls`

#### Sub-task 2.5: Render third-party control section in markdown

After "Design Anti-Patterns" section:

```markdown
## Third-Party Control Libraries

**Vendors detected**: {count}
**Total third-party controls**: {count} across {file_count} files

### Telerik UI for ASP.NET AJAX
- **Controls used**: RadGrid (12 files), RadComboBox (8 files), RadEditor (3 files), RadTreeView (2 files)
- **Files**: AdminPage.aspx, UserList.aspx, Dashboard.aspx, ...
- **Modern replacement ({target_stack})**: Telerik UI for Blazor or MudBlazor (open source)
- **License**: Commercial license required for Telerik Blazor; MudBlazor is MIT

### DevExpress ASP.NET Controls
- **Controls used**: ASPxGridView (5 files), ASPxDateEdit (3 files)
- **Modern replacement ({target_stack})**: DevExpress Blazor Components or MudBlazor
- **License**: Commercial license required

### Unmapped Controls (no automatic mapping available)
| Control | Vendor | File | Note |
|---------|--------|------|------|
| telerik:RadSpell | Telerik | Editor.aspx | No modern equivalent — remove or use browser spellcheck |

### Migration Impact
- {count} Telerik RadGrid instances → each needs column definitions, templates, and event handlers mapped
- {count} DevExpress ASPxGridView instances → different API surface than Telerik
- {count} controls have no mapping → manual migration required
- **Licensing decision needed**: Continue with vendor suite ($$) or switch to open-source (MudBlazor)?
```

#### Sub-task 2.6: Add per-page third-party controls to dossier

In the per-page dossier markdown, add:

```markdown
**Third-party controls**: RadGrid (AllowSorting, AllowPaging, OnNeedDataSource), RadComboBox (2 instances)
```

---

### Gap 3: Assembly / NuGet Reference Surfacing

**Current behavior**: `solution_parser.rs` already extracts `PackageRef { name, version }` from `<PackageReference>` elements in `.csproj`/`.vbproj` files, and assembly references from `<Reference>` elements. The `parse_project_file()` function returns a `ProjectFileInfo` struct with `package_references: Vec<PackageRef>` and project path. The `build_solution_structure()` function returns a `SolutionStructure` with `project_details: BTreeMap<String, ProjectFileInfo>`.

**None of this data reaches the full project migration report.**

**Why it matters**: An AI agent needs to know:
- What NuGet packages are installed (and what versions) to know what to replace
- What assembly references exist (e.g., `System.Web`, `Microsoft.ReportViewer.WebForms`, `Telerik.Web.UI`)
- What project references exist (multi-project solutions)
- What target framework is in use (`.NET Framework 4.5` vs `4.8`)
- What modern equivalents exist for each package

**Files to modify**:
- `crates/engram_server/src/tools.rs` — discover .csproj/.vbproj files, parse them
- `crates/engram_server/src/services/full_project_migration_service.rs` — surface in report

#### Sub-task 3.1: Discover and parse project files in tool handler

In `tools.rs`, after file discovery, also discover `.csproj`, `.vbproj`, and `.sln` files:

```rust
let proj_files = discover_files_recursive(&project_dir, &[".csproj", ".vbproj"], 50).await;
let sln_files = discover_files_recursive(&project_dir, &[".sln"], 5).await;
```

Read each project file and parse with `solution_parser::parse_project_file()`. Collect all `PackageRef` and assembly references.

Add to `ProjectFileBundle`:
```rust
pub project_references: Vec<ProjectReferenceBundle>,
```

```rust
#[derive(Debug, Clone)]
pub struct ProjectReferenceBundle {
    pub project_path: String,
    pub target_framework: Option<String>,
    pub assembly_name: Option<String>,
    pub root_namespace: Option<String>,
    pub package_references: Vec<PackageRef>,
    pub assembly_references: Vec<String>,
    pub project_dependencies: Vec<String>,
}
```

#### Sub-task 3.2: Build `DependencyInventory` struct

```rust
#[derive(Debug, Clone, Serialize)]
pub struct DependencyInventory {
    pub target_frameworks: Vec<String>,                // [".NET Framework 4.8"]
    pub nuget_packages: Vec<NuGetPackageInfo>,
    pub assembly_references: Vec<AssemblyRefInfo>,
    pub project_references: Vec<ProjectRefInfo>,
    pub total_packages: usize,
    pub total_assemblies: usize,
    pub framework_assemblies: Vec<String>,             // System.Web, System.Data, etc.
    pub third_party_assemblies: Vec<String>,           // Telerik.Web.UI, etc.
    pub packages_with_known_replacement: usize,
    pub packages_without_replacement: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NuGetPackageInfo {
    pub name: String,
    pub version: Option<String>,
    pub modern_replacement: Option<String>,           // "Microsoft.EntityFrameworkCore"
    pub modern_version: Option<String>,               // "8.0"
    pub migration_notes: Option<String>,              // "API surface changes significantly"
    pub category: String,                             // "ORM", "Logging", "Web", "Testing", etc.
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyRefInfo {
    pub assembly_name: String,
    pub is_framework: bool,                           // System.* = true
    pub modern_equivalent: Option<String>,
    pub removal_reason: Option<String>,               // "Removed in .NET Core — use X instead"
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRefInfo {
    pub project_name: String,
    pub project_path: String,
    pub target_framework: Option<String>,
}
```

#### Sub-task 3.3: Build a known-replacement lookup table

Create a function that maps common legacy NuGet packages and assemblies to their modern equivalents:

```rust
fn lookup_modern_replacement(package_or_assembly: &str) -> Option<(&'static str, &'static str, &'static str)>
// Returns: (modern_replacement, modern_version_hint, migration_notes)
```

Populate with at least 40 entries:

| Legacy Package/Assembly | Modern Replacement | Notes |
|------------------------|-------------------|-------|
| `System.Web` | (removed) | Use ASP.NET Core middleware |
| `System.Web.Mvc` | `Microsoft.AspNetCore.Mvc` | Different routing, DI |
| `EntityFramework` | `Microsoft.EntityFrameworkCore` | Different DbContext API |
| `Newtonsoft.Json` | `System.Text.Json` | Or keep Newtonsoft (compatible) |
| `Microsoft.ReportViewer.WebForms` | `Microsoft.Reporting.NETCore` | Limited .NET Core support |
| `Telerik.Web.UI` | `Telerik.UI.for.Blazor` | Commercial, different API |
| `DevExpress.Web` | `DevExpress.Blazor` | Commercial, different API |
| `Infragistics.Web` | `IgniteUI.Blazor` | Commercial |
| `log4net` | `Serilog` or `NLog` | Similar patterns |
| `Unity` (DI) | `Microsoft.Extensions.DependencyInjection` | Built-in DI |
| `Autofac` | `Autofac` | .NET Core compatible |
| `System.Data.SqlClient` | `Microsoft.Data.SqlClient` | Namespace change |
| `Microsoft.Practices.EnterpriseLibrary` | (various) | Replace per-block |
| `NPOI` | `NPOI` | Compatible, or use ClosedXML |
| `EPPlus` | `EPPlus` | License change in v5+ |
| `iTextSharp` | `itext7` or `QuestPDF` | License change |
| `CrystalDecisions.CrystalReports` | (none) | No .NET Core port |
| `Microsoft.AspNet.SignalR` | `Microsoft.AspNetCore.SignalR` | Different hub API |
| `System.Web.Optimization` | (various bundlers) | Use Vite/Webpack |
| `Microsoft.Owin` | (built-in) | ASP.NET Core has native middleware |
| `Antlr` | `Antlr4` | Different API |
| `NHibernate` | `NHibernate` or EF Core | .NET Core compatible |
| `Dapper` | `Dapper` | Already compatible |
| `FluentValidation` | `FluentValidation` | Already compatible |
| `AutoMapper` | `AutoMapper` or `Mapster` | Already compatible |
| `MediatR` | `MediatR` | Already compatible |
| `Hangfire` | `Hangfire` | Already compatible |
| `Quartz.NET` | `Quartz.NET` | Already compatible |
| `StackExchange.Redis` | `StackExchange.Redis` | Already compatible |
| `Microsoft.AspNet.WebApi` | `Microsoft.AspNetCore.Mvc` | Unified in ASP.NET Core |
| `System.Web.Services` | (removed) | Use Minimal API / gRPC |
| `System.ServiceModel` | `CoreWCF` or gRPC | Limited WCF in .NET Core |
| `System.EnterpriseServices` | (removed) | No .NET Core equivalent |
| `System.DirectoryServices` | `System.DirectoryServices` | Partial support |
| `System.Drawing` | `System.Drawing.Common` | Linux needs libgdiplus |
| `AjaxControlToolkit` | (various) | No .NET Core port |
| `Microsoft.Web.Infrastructure` | (removed) | Built into ASP.NET Core |
| `WebGrease` | (removed) | Use modern bundler |
| `Antlr3.Runtime` | (updated) | Only if used directly |

#### Sub-task 3.4: Add to report struct and markdown

Add `pub dependency_inventory: DependencyInventory` to `FullProjectMigrationReport`.

Add to `CrossCuttingSummary`:
```rust
pub total_nuget_packages: usize,
pub target_framework: String,
```

Render after "Executive Summary" (as one of the first sections — framework/packages are foundational):

```markdown
## Project Dependencies

**Target Framework**: .NET Framework 4.8
**NuGet Packages**: {total} ({with_replacement} have modern replacements, {without} need manual evaluation)
**Assembly References**: {total} ({framework} framework, {third_party} third-party)
**Project References**: {count}

### NuGet Packages
| Package | Version | Modern Replacement | Category | Notes |
|---------|---------|-------------------|----------|-------|
| EntityFramework | 6.4.4 | Microsoft.EntityFrameworkCore 8.0 | ORM | Different DbContext API |
| Telerik.Web.UI | 2023.1 | Telerik.UI.for.Blazor | UI Controls | Commercial, different API |
| Newtonsoft.Json | 13.0.3 | System.Text.Json (or keep) | Serialization | Compatible |

### Framework Assemblies Requiring Replacement
| Assembly | Status in .NET Core | Migration Path |
|----------|--------------------|--------------  |
| System.Web | Removed | ASP.NET Core middleware |
| System.Web.Services | Removed | Minimal API / gRPC |
| System.EnterpriseServices | Removed | No equivalent |

### Compatible Packages (no action needed)
Dapper, FluentValidation, AutoMapper, Serilog, StackExchange.Redis, ...

### Migration Impact
- Framework target: .NET Framework 4.8 → .NET 8.0+ (requires `<TargetFramework>net8.0</TargetFramework>`)
- {count} packages need replacement → major effort
- {count} packages are already compatible → just update version
- {count} assemblies removed in .NET Core → must find alternatives
```

---

### Gap 4: OutputCache / Caching Pattern Detection

**Current behavior**: The `state_extractor.rs` detects `Cache["key"]` bracket-access patterns and emits `reads_state`/`writes_state` edges. It does NOT detect:
- `HttpRuntime.Cache.Insert(key, value, deps, expiration, priority)` — the most common programmatic cache pattern
- `HttpRuntime.Cache.Add(key, value, ...)` — same API, returns existing value
- `HttpContext.Current.Cache.Get(key)` / `HttpContext.Current.Cache[key]`
- `Response.Cache.SetExpires()` / `Response.Cache.SetCacheability()` — HTTP response caching
- `<%@ OutputCache Duration="60" VaryByParam="id" %>` — page/control output caching directive

The `webforms.rs` extractor has zero OutputCache directive detection.

**Why it matters**: Caching is critical for performance and often the hardest part to get right in a migration. ASP.NET WebForms caching maps to completely different APIs:
- `HttpRuntime.Cache` → `IMemoryCache` or `IDistributedCache`
- `<%@ OutputCache %>` → `[ResponseCache]` attribute + Response Caching Middleware
- `Response.Cache` → Response headers middleware
- `SqlCacheDependency` → No direct equivalent (use change tracking + pub/sub)

Without a caching inventory, the migrated application will either have no caching (performance regression) or incorrect caching (stale data bugs).

**Files to modify**:
- `crates/engram_index/src/webforms.rs` — detect `<%@ OutputCache %>` directives
- `crates/engram_index/src/state_extractor.rs` — detect programmatic cache API calls
- `crates/engram_server/src/services/full_project_migration_service.rs` — surface in report

#### Sub-task 4.1: Detect `<%@ OutputCache %>` directives in `webforms.rs`

Add a new regex and extraction function in `webforms.rs`:

```rust
// Detect OutputCache directives
// Examples:
//   <%@ OutputCache Duration="60" VaryByParam="id" %>
//   <%@ OutputCache Duration="3600" VaryByParam="none" Location="Server" %>
//   <%@ OutputCache Duration="300" VaryByParam="*" VaryByCustom="browser" %>
//   <%@ OutputCache CacheProfile="ShortCache" %>
static OUTPUT_CACHE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<%@\s*OutputCache\s+([^%]+?)%>"#).unwrap()
});
```

Extract attributes: `Duration`, `VaryByParam`, `VaryByControl`, `VaryByCustom`, `Location` (Any/Server/Client/Downstream/None), `CacheProfile`, `SqlDependency`, `Shared`.

Emit an `insight` node with type `"output_cache"` and metadata containing all parsed attributes.

#### Sub-task 4.2: Detect programmatic cache patterns in `state_extractor.rs`

Add new regex patterns to `state_extractor.rs`:

```rust
// HttpRuntime.Cache.Insert(key, value, ...)
// HttpRuntime.Cache.Add(key, value, ...)
// HttpRuntime.Cache.Get(key)
// HttpRuntime.Cache.Remove(key)
// HttpContext.Current.Cache.Insert(key, value, ...)
// Cache.Insert(key, value, ...) — when Cache is imported
static CACHE_API_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:HttpRuntime\.Cache|HttpContext\.Current\.Cache|(?<!\w)Cache)\.(?:Insert|Add|Get|Remove)\s*\(\s*"([^"]+)""#).unwrap()
});

// Response.Cache.SetExpires(DateTime.Now.AddMinutes(60))
// Response.Cache.SetCacheability(HttpCacheability.Public)
// Response.Cache.SetMaxAge(TimeSpan.FromHours(1))
// Response.Cache.SetValidUntilExpires(true)
static RESPONSE_CACHE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)Response\.Cache\.Set(?:Expires|Cacheability|MaxAge|ValidUntilExpires|NoStore|NoTransforms|SlidingExpiration|Revalidation|ETag|LastModified|VaryByCustom|OmitVaryStar)\s*\("#).unwrap()
});

// SqlCacheDependency — rare but critical
// new SqlCacheDependency("DatabaseName", "TableName")
// new SqlCacheDependency(command)
static SQL_CACHE_DEP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)new\s+SqlCacheDependency\s*\("#).unwrap()
});
```

For each match:
- `Cache.Insert`/`Cache.Add` → emit `writes_state` edge with target `"Cache:{key}"` and metadata `{ "cache_api": "Insert", "pattern": "programmatic" }`
- `Cache.Get` → emit `reads_state` edge with target `"Cache:{key}"` and metadata `{ "cache_api": "Get", "pattern": "programmatic" }`
- `Cache.Remove` → emit `writes_state` edge with metadata `{ "cache_api": "Remove" }`
- `Response.Cache.*` → emit `insight` node with metadata `{ "pattern": "response_cache" }`
- `SqlCacheDependency` → emit `insight` node with metadata `{ "pattern": "sql_cache_dependency" }`

#### Sub-task 4.3: Build `CachingInventory` struct in the report

```rust
#[derive(Debug, Clone, Serialize)]
pub struct CachingInventory {
    pub output_cache_pages: Vec<OutputCacheEntry>,
    pub programmatic_cache_keys: Vec<ProgrammaticCacheEntry>,
    pub response_cache_files: Vec<String>,
    pub sql_cache_dependencies: Vec<SqlCacheDependencyEntry>,
    pub total_cached_pages: usize,
    pub total_cache_keys: usize,
    pub has_response_caching: bool,
    pub has_sql_dependencies: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputCacheEntry {
    pub file_path: String,
    pub duration_seconds: Option<u32>,
    pub vary_by_param: Option<String>,
    pub vary_by_control: Option<String>,
    pub vary_by_custom: Option<String>,
    pub location: Option<String>,
    pub cache_profile: Option<String>,
    pub sql_dependency: Option<String>,
    pub modern_equivalent: String,       // "[ResponseCache(Duration = 60, VaryByQueryKeys = ...)]"
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgrammaticCacheEntry {
    pub cache_key: String,
    pub operation: String,               // "Insert", "Add", "Get", "Remove"
    pub files: Vec<String>,              // files that access this key
    pub has_expiration: bool,
    pub has_dependency: bool,
    pub modern_equivalent: String,       // "IMemoryCache.Set(key, value, options)" or "IDistributedCache"
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlCacheDependencyEntry {
    pub file_path: String,
    pub database_name: Option<String>,
    pub table_name: Option<String>,
    pub modern_note: String,             // "No direct equivalent — use Change Tracking + pub/sub"
}
```

Populate by:
1. Querying graph for `insight` nodes with `output_cache` or `response_cache` pattern metadata
2. Querying `reads_state`/`writes_state` edges where target contains `"Cache:"` prefix
3. Querying insight nodes with `sql_cache_dependency` metadata
4. Fallback: regex-scanning code-behind content directly if graph data is sparse

#### Sub-task 4.4: Add to report and render

Add `pub caching_inventory: CachingInventory` to `FullProjectMigrationReport`.

Add to `CrossCuttingSummary`:
```rust
pub total_cached_pages: usize,
pub total_cache_keys: usize,
```

Render after "State Management":

```markdown
## Caching Strategy

**Output-cached pages**: {count}
**Programmatic cache keys**: {count}
**Response-cached files**: {count}
**SQL cache dependencies**: {count}

### Page/Control Output Caching
| Page | Duration | VaryByParam | Location | Modern Equivalent |
|------|----------|-------------|----------|-------------------|
| ProductList.aspx | 60s | categoryId | Server | [ResponseCache(Duration = 60, VaryByQueryKeys = new[] { "categoryId" })] |
| Dashboard.aspx | 300s | none | Any | [ResponseCache(Duration = 300)] |

### Programmatic Cache Keys
| Key | Operations | Used By | Modern Equivalent |
|-----|-----------|---------|-------------------|
| UserPermissions_{userId} | Insert, Get | Auth.vb, Admin.vb | IMemoryCache with SlidingExpiration |
| LookupData_States | Insert, Get | Common.vb, Address.vb | IDistributedCache (shared across instances) |

### SQL Cache Dependencies
| File | Database | Table | Note |
|------|----------|-------|------|
| ProductCache.vb | AppDb | Products | No direct .NET Core equivalent — use EF Change Tracker + cache invalidation |

### Migration Strategy
- `HttpRuntime.Cache` → `IMemoryCache` (single-server) or `IDistributedCache` (Redis, multi-server)
- `<%@ OutputCache %>` → `[ResponseCache]` attribute + `services.AddResponseCaching()`
- `Response.Cache.*` → `Response.Headers` or `[ResponseCache]` attribute
- `SqlCacheDependency` → Manual invalidation via Change Tracking, SignalR, or message bus
- **WARNING**: {count} cache keys have no explicit expiration → potential memory leaks in legacy code
```

---

### Gap 5: URL Routing / Rewrite Rules

**Current behavior**: The `full_project_migration_service.rs` detects `RouteConfig.RegisterRoutes` and `RouteTable.Routes` in Global.asax code-behind (~line 1068) and categorizes it as a "routing" startup registration. But it misses:
- Classic URL rewriting in web.config: `<system.webServer><rewrite><rules>` and `<httpRuntime><urlMappings>`
- IIS URL Rewrite Module rules
- Third-party URL rewriters (UrlRewriter.NET, ISAPI_Rewrite)
- `HttpContext.RewritePath()` calls in code
- `routes.MapPageRoute()` calls — WebForms-specific routing
- `RouteValueDictionary` usage
- Friendly URL configuration (`FriendlyUrlSettings`)

**Why it matters**: URL structure is one of the first things to break during migration. Every URL in the legacy app must map to a route in the modern app. If the AI agent doesn't know the complete URL scheme, it will produce incorrect routing that breaks bookmarks, SEO, and external integrations.

**Files to modify**:
- `crates/engram_server/src/services/full_project_migration_service.rs` — parse web.config rewrite rules, scan code for routing patterns

#### Sub-task 5.1: Extract URL rewrite rules from web.config

Add to the existing `extract_webconfig_inventory()` function (or create a new helper):

```rust
fn extract_url_routing(
    web_config: &str,
    global_asax_content: &str,
    code_files: &[(String, String)],
) -> UrlRoutingInventory
```

Parse web.config for:
```xml
<!-- IIS URL Rewrite Module -->
<system.webServer>
  <rewrite>
    <rules>
      <rule name="..." stopProcessing="true">
        <match url="^products/(\d+)$" />
        <action type="Rewrite" url="ProductDetail.aspx?id={R:1}" />
      </rule>
    </rules>
  </rewrite>
</system.webServer>

<!-- Old-style URL mappings -->
<urlMappings>
  <add url="~/home" mappedUrl="~/Default.aspx" />
</urlMappings>

<!-- ASP.NET Friendly URLs -->
<appSettings>
  <add key="FriendlyUrlSettings:AutoRedirectMode" value="Permanent" />
</appSettings>
```

Regex patterns:
```rust
// IIS Rewrite rules
static REWRITE_RULE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<rule\s+name="([^"]*)"[^>]*>.*?<match\s+url="([^"]*)"[^/]*/?>.*?<action\s+type="(\w+)"\s+url="([^"]*)"[^/]*/?>.*?</rule>"#).unwrap()
});

// URL mappings
static URL_MAPPING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<add\s+url="([^"]*)"\s+mappedUrl="([^"]*)"\s*/>"#).unwrap()
});
```

#### Sub-task 5.2: Detect routing patterns in code

Scan Global.asax code-behind and all code files for:

```rust
// routes.MapPageRoute("RouteName", "url-pattern", "~/Page.aspx")
static MAP_PAGE_ROUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\.MapPageRoute\s*\(\s*"([^"]*)",\s*"([^"]*)",\s*"([^"]*)""#).unwrap()
});

// HttpContext.RewritePath("/new-path")
// Context.RewritePath("~/page.aspx", ...)
static REWRITE_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:HttpContext\.Current|Context|HttpContext)\.RewritePath\s*\(\s*"([^"]*)""#).unwrap()
});

// Response.Redirect("~/Page.aspx") — not routing but indicates URL structure
// Response.RedirectPermanent("~/Page.aspx")
static REDIRECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)Response\.Redirect(?:Permanent)?\s*\(\s*"([^"]*)""#).unwrap()
});

// Server.Transfer("~/Page.aspx")
static SERVER_TRANSFER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)Server\.Transfer\s*\(\s*"([^"]*)""#).unwrap()
});
```

#### Sub-task 5.3: Build `UrlRoutingInventory` struct

```rust
#[derive(Debug, Clone, Serialize)]
pub struct UrlRoutingInventory {
    pub rewrite_rules: Vec<UrlRewriteRule>,
    pub page_routes: Vec<PageRoute>,
    pub url_mappings: Vec<UrlMapping>,
    pub rewrite_path_calls: Vec<RewritePathCall>,
    pub redirects: Vec<RedirectEntry>,
    pub server_transfers: Vec<ServerTransferEntry>,
    pub has_friendly_urls: bool,
    pub total_url_patterns: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UrlRewriteRule {
    pub rule_name: String,
    pub match_pattern: String,       // regex or glob
    pub action_type: String,         // "Rewrite", "Redirect", "RedirectPermanent"
    pub target_url: String,
    pub modern_equivalent: String,   // "app.MapGet(\"/products/{id}\", ...)"
}

#[derive(Debug, Clone, Serialize)]
pub struct PageRoute {
    pub route_name: String,
    pub url_pattern: String,         // "products/{category}/{id}"
    pub physical_page: String,       // "~/ProductDetail.aspx"
    pub modern_equivalent: String,   // "app.MapGet(\"/products/{category}/{id}\", ...)"
}

#[derive(Debug, Clone, Serialize)]
pub struct UrlMapping {
    pub friendly_url: String,
    pub mapped_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewritePathCall {
    pub file_path: String,
    pub target_path: String,
    pub line_number: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedirectEntry {
    pub file_path: String,
    pub target_url: String,
    pub is_permanent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerTransferEntry {
    pub file_path: String,
    pub target_page: String,
}
```

#### Sub-task 5.4: Add to report and render

Add `pub url_routing: UrlRoutingInventory` to `FullProjectMigrationReport`.

Render after "Configuration (web.config)":

```markdown
## URL Routing & Rewriting

**URL patterns**: {total} ({rewrite_rules} rewrite rules, {page_routes} page routes, {url_mappings} URL mappings)
**RewritePath calls**: {count}
**Redirects**: {count}
**Server.Transfer calls**: {count}
**Friendly URLs**: {enabled/disabled}

### IIS Rewrite Rules
| Rule | Match Pattern | Action | Target | Modern Equivalent |
|------|--------------|--------|--------|-------------------|
| ProductDetail | ^products/(\d+)$ | Rewrite | ProductDetail.aspx?id={R:1} | app.MapGet("/products/{id}", ...) |
| OldCatalog | ^catalog/?$ | Redirect 301 | /products | app.MapGet("/catalog", () => Results.RedirectPermanent("/products")) |

### Page Routes (Global.asax)
| Route Name | URL Pattern | Physical Page | Modern Equivalent |
|-----------|-------------|---------------|-------------------|
| ProductRoute | products/{category}/{id} | ~/ProductDetail.aspx | app.MapGet("/products/{category}/{id}", ...) |

### Code-Based URL Manipulation
| File | Pattern | Target | Type |
|------|---------|--------|------|
| UrlModule.vb | RewritePath | ~/Product.aspx | Rewrite |
| Checkout.aspx.vb | Response.Redirect | ~/Cart.aspx | Redirect |
| Admin.aspx.vb | Server.Transfer | ~/AdminDashboard.aspx | Transfer |

### Migration Strategy
- IIS Rewrite Rules → ASP.NET Core URL Rewriting Middleware (`app.UseRewriter()`)
- `MapPageRoute` → `app.MapGet()` / `app.MapBlazorHub()` / `@page` directives
- `HttpContext.RewritePath` → Middleware pipeline or endpoint routing
- `Server.Transfer` → **No equivalent in ASP.NET Core** — must refactor to redirect or shared component
- `Response.Redirect` → `Results.Redirect()` / `NavigationManager.NavigateTo()`
- **WARNING**: {count} Server.Transfer calls must be refactored — this pattern does not exist in ASP.NET Core
```

---

### Gap 6: VB.NET → C# Translation Flags

**Current behavior**: The VB extractor captures methods, COM interop, error handling (On Error Resume Next), late binding, and ReDim. But the full project report does not flag VB-specific language constructs that require careful C# translation. The report also does not indicate whether the project is VB.NET or C# (it should — the migration strategy differs significantly).

**Why it matters**: VB.NET and C# have semantic differences that cause subtle bugs if translated naively:
- `Nothing` is equivalent to `default(T)`, not always `null`
- `Optional` parameters with `IsMissing()` have no C# equivalent
- `Module` becomes `static class` but has different scoping rules
- `My.Computer`, `My.Application`, `My.Settings` have no C# equivalent
- `WithEvents` / `Handles` clause — C# uses explicit event subscription
- `RaiseEvent` — C# uses direct delegate invocation
- `Shadows` vs `new` — different override semantics
- `Option Compare Text` — case-insensitive string comparison by default
- `Like` operator — VB regex-like pattern matching
- `IsNumeric()` / `IsDate()` — VB intrinsics with no C# equivalent
- `CType()` / `DirectCast()` / `TryCast()` — different casting semantics
- `Dim x As New MyClass()` — VB's combined declaration+initialization
- `With ... End With` blocks — no C# equivalent
- Late-bound `Object` calls — VB allows without `dynamic`, C# requires it

**Files to modify**:
- `crates/engram_server/src/services/full_project_migration_service.rs` — scan code-behind content for VB patterns

#### Sub-task 6.1: Detect VB.NET-specific constructs

Add a function that scans code-behind content for VB-specific patterns:

```rust
fn analyze_vb_translation_flags(
    code_files: &[(String, String)],
    codebehind_contents: &[(String, String)],  // (path, content)
) -> VbTranslationReport
```

Regex patterns to detect:

```rust
// Language detection
static VB_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.vb$").unwrap()
});

// Optional parameters with IsMissing
static OPTIONAL_PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bOptional\s+(?:ByVal|ByRef)?\s*\w+\s+As\s+").unwrap()
});
static IS_MISSING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bIsMissing\s*\(").unwrap()
});

// Module declarations (not Class)
static MODULE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:Public\s+|Friend\s+)?Module\s+(\w+)").unwrap()
});

// My. namespace usage
static MY_NAMESPACE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bMy\.(Computer|Application|Settings|Resources|User|Forms|WebServices)\b").unwrap()
});

// WithEvents / Handles clause
static WITH_EVENTS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bWithEvents\s+").unwrap()
});
static HANDLES_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bHandles\s+\w+\.\w+").unwrap()
});

// RaiseEvent
static RAISE_EVENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bRaiseEvent\s+\w+").unwrap()
});

// Shadows keyword
static SHADOWS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bShadows\s+").unwrap()
});

// Option Compare Text (file-level)
static OPTION_COMPARE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*Option\s+Compare\s+Text").unwrap()
});

// Like operator
static LIKE_OPERATOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bLike\s+""[^""]*[*?#\[\]]+[^""]*""").unwrap()
});

// VB intrinsics with no C# equivalent
static VB_INTRINSICS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:IsNumeric|IsDate|IsNothing|IsDBNull|IsArray|IsError)\s*\(").unwrap()
});

// On Error Resume Next (already detected by VB extractor, but count for report)
static ON_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bOn\s+Error\s+(?:Resume\s+Next|GoTo\s+)").unwrap()
});

// Late binding via Object
static LATE_BINDING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bDim\s+\w+\s+As\s+Object\b").unwrap()
});

// CType / DirectCast / TryCast
static VB_CAST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:CType|DirectCast|TryCast|CStr|CInt|CDbl|CBool|CLng|CDec|CDate|CByte|CShort|CSng|CObj|CChar)\s*\(").unwrap()
});
```

#### Sub-task 6.2: Build `VbTranslationReport` struct

```rust
#[derive(Debug, Clone, Serialize)]
pub struct VbTranslationReport {
    pub is_vb_project: bool,
    pub vb_file_count: usize,
    pub cs_file_count: usize,
    pub mixed_language: bool,          // both VB and C# files present
    pub translation_flags: Vec<VbTranslationFlag>,
    pub total_flags: usize,
    pub flags_by_category: BTreeMap<String, usize>,  // "ErrorHandling" → 12
    pub highest_risk_files: Vec<(String, usize)>,    // files with most flags
}

#[derive(Debug, Clone, Serialize)]
pub struct VbTranslationFlag {
    pub category: String,              // "ErrorHandling", "LateBind", "Intrinsics", etc.
    pub pattern: String,               // "On Error Resume Next"
    pub file_path: String,
    pub count: usize,                  // occurrences in this file
    pub csharp_equivalent: String,     // "try-catch blocks"
    pub risk_level: String,            // "low", "medium", "high"
    pub auto_translatable: bool,       // can a tool do it automatically?
    pub notes: String,                 // additional context
}
```

Categories and their C# equivalents:

| Category | VB Pattern | C# Equivalent | Risk | Auto? |
|----------|-----------|---------------|------|-------|
| ErrorHandling | `On Error Resume Next` | try-catch per statement | High | No |
| ErrorHandling | `On Error GoTo label` | try-catch with specific handling | Medium | Partial |
| OptionalParams | `Optional ByVal x As String = ""` | `string x = ""` | Low | Yes |
| OptionalParams | `IsMissing(x)` | No equivalent — restructure | High | No |
| Modules | `Module ModuleName` | `static class ModuleName` | Low | Yes |
| MyNamespace | `My.Computer.FileSystem` | `System.IO` equivalents | Medium | Partial |
| MyNamespace | `My.Settings.PropertyName` | `IConfiguration` | Medium | No |
| Events | `WithEvents obj` | Explicit `+=` / `-=` subscription | Medium | Partial |
| Events | `Handles btn.Click` | `btn.Click +=` in constructor/Init | Medium | Partial |
| Events | `RaiseEvent MyEvent(args)` | `MyEvent?.Invoke(args)` | Low | Yes |
| Inheritance | `Shadows` | `new` modifier | Low | Yes |
| StringCompare | `Option Compare Text` | `StringComparer.OrdinalIgnoreCase` everywhere | High | No |
| PatternMatch | `Like "A*B"` | `Regex.IsMatch()` | Medium | Partial |
| Intrinsics | `IsNumeric(x)` | `double.TryParse(x, out _)` | Low | Yes |
| Intrinsics | `IsDate(x)` | `DateTime.TryParse(x, out _)` | Low | Yes |
| Intrinsics | `IsNothing(x)` | `x is null` or `x == null` | Low | Yes |
| Casting | `CType(x, String)` | `(string)x` or `x as string` | Low | Yes |
| Casting | `DirectCast(x, String)` | `(string)x` (throws) | Low | Yes |
| Casting | `TryCast(x, String)` | `x as string` (returns null) | Low | Yes |
| LateBind | `Dim x As Object` + method calls | `dynamic x` | High | Partial |
| WithBlock | `With obj ... End With` | No equivalent (use temp var) | Low | Yes |

#### Sub-task 6.3: Add to report and render

Add `pub vb_translation: VbTranslationReport` to `FullProjectMigrationReport`.

Render after "Project Dependencies":

```markdown
## Language & Translation Analysis

**Primary language**: VB.NET ({vb_count} files)
**Secondary language**: C# ({cs_count} files)
**Translation flags**: {total} across {file_count} files

### Translation Risk Summary
| Category | Count | Risk | Auto-Translatable |
|----------|-------|------|-------------------|
| Error Handling (On Error) | 45 | High | No — requires manual try-catch restructuring |
| Late Binding (Object) | 12 | High | Partial — needs dynamic keyword |
| Option Compare Text | 3 files | High | No — must add StringComparer everywhere |
| VB Intrinsics (IsNumeric, etc.) | 28 | Low | Yes — mechanical replacement |
| WithEvents / Handles | 67 | Medium | Partial — need explicit event wiring |
| Module declarations | 8 | Low | Yes — static class |
| My. namespace | 15 | Medium | Partial — different APIs |

### Highest-Risk Files (most translation flags)
| File | Flags | Top Concerns |
|------|-------|-------------|
| Utils/Helpers.vb | 34 | On Error (12), IsNumeric (8), Module (1), Late Binding (5) |
| App_Code/DataLayer.vb | 28 | On Error (15), CType (8), Optional (5) |

### Migration Strategy
- **Step 1**: Run automated VB→C# converter (dotnet-vb2cs or Instant C#) for mechanical translations
- **Step 2**: Manually fix {count} `On Error Resume Next` patterns → proper try-catch
- **Step 3**: Convert {count} `Dim x As Object` late bindings → `dynamic` or typed interfaces
- **Step 4**: Add `StringComparer.OrdinalIgnoreCase` to all string comparisons in {count} `Option Compare Text` files
- **Step 5**: Replace `My.*` namespace calls with .NET standard library equivalents
- **Step 6**: Convert `WithEvents`/`Handles` patterns to explicit event subscription in constructors
```

#### Sub-task 6.4: Per-page VB translation flags in dossier

For each page in the dossier, add:

```markdown
**VB translation flags**: On Error Resume Next (3), Handles clause (5), IsNumeric (2), CType (4)
```

---

### Gap 7: Multi-Tenancy Detection

**Current behavior**: Zero detection of multi-tenancy patterns anywhere in the codebase.

**Why it matters**: A SaaS application that serves multiple tenants from a single deployment has multi-tenancy woven into every layer — database queries filter by tenant ID, connection strings may differ per tenant, session stores tenant context, HTTP modules resolve tenant from subdomain/header, authorization checks include tenant scope. An AI agent that doesn't know the application is multi-tenant will produce single-tenant code that either leaks data between tenants or breaks entirely.

Multi-tenancy patterns in legacy WebForms apps:

1. **Database-level**: Every SQL query includes `WHERE TenantId = @tenantId` or uses tenant-specific schemas
2. **Connection-string-level**: Different tenants get different connection strings (tenant DB isolation)
3. **Session-level**: `Session["TenantId"]` or `Session["TenantContext"]` stored at login
4. **HTTP-level**: Custom `IHttpModule` resolves tenant from subdomain, header, or cookie
5. **Config-level**: `appSettings` keys like `TenantMode`, `MultiTenancy`, `TenantProvider`
6. **Code-level**: Tenant ID passed as parameter to data access methods, or stored in thread-static / `HttpContext.Items`

**Files to modify**:
- `crates/engram_server/src/services/full_project_migration_service.rs` — detect multi-tenancy patterns

#### Sub-task 7.1: Detect multi-tenancy patterns

Add a function:

```rust
fn detect_multi_tenancy(
    web_config: Option<&str>,
    code_files: &[(String, String)],
    codebehind_contents: &[(String, String)],
    global_asax_content: Option<&str>,
) -> MultiTenancyReport
```

Detection patterns:

```rust
// Session-based tenant storage
static TENANT_SESSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)Session\s*[\(\[]\s*"(?:TenantId|Tenant|TenantKey|TenantCode|OrganizationId|OrgId|CompanyId|ClientId|SiteId|AccountId|CustomerId)"#).unwrap()
});

// HttpContext.Items tenant storage
static TENANT_CONTEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:HttpContext\.Current\.Items|Context\.Items)\s*[\(\[]\s*"(?:TenantId|Tenant|TenantContext|CurrentTenant)"#).unwrap()
});

// SQL queries with tenant filtering
static TENANT_SQL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:WHERE|AND)\s+(?:\w+\.)?(?:TenantId|TenantID|Tenant_ID|OrgId|OrganizationId|CompanyId|SiteId|AccountId)\s*=\s*(?:@\w+|'[^']*'|\?)"#).unwrap()
});

// Tenant ID parameters in methods
static TENANT_PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:tenantId|tenant_id|orgId|organizationId|companyId|siteId|accountId)\s+(?:As\s+(?:Integer|String|Guid)|:\s*(?:int|string|Guid))"#).unwrap()
});

// AppSettings for multi-tenancy
static TENANT_CONFIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:TenantMode|MultiTenancy|TenantProvider|TenantResolution|TenantStrategy|IsTenanted|EnableMultiTenancy)"#).unwrap()
});

// Tenant-aware connection string selection
static TENANT_CONN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:GetConnectionString|ConnectionString)\s*[\(\[]\s*(?:tenantId|tenant|orgId)"#).unwrap()
});

// Subdomain-based tenant resolution
static SUBDOMAIN_TENANT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:Request\.Url\.Host|Request\.Headers\["X-Tenant"|Request\.Headers\["Host"\]).*(?:Split|Substring|Replace|tenant|org)"#).unwrap()
});

// IHttpModule with tenant in name
static TENANT_MODULE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)class\s+\w*(?:Tenant|MultiTenant|Org)\w*\s*:\s*I(?:Http)?Module"#).unwrap()
});
```

#### Sub-task 7.2: Build `MultiTenancyReport` struct

```rust
#[derive(Debug, Clone, Serialize)]
pub struct MultiTenancyReport {
    pub is_multi_tenant: bool,
    pub confidence: String,               // "high", "medium", "low", "none"
    pub tenant_id_column_name: Option<String>,  // most common column name
    pub isolation_strategy: Option<String>,       // "shared_db_shared_schema", "shared_db_separate_schema", "separate_db"
    pub detection_evidence: Vec<TenancyEvidence>,
    pub tenant_resolution: Option<TenantResolution>,
    pub tenant_filtered_queries: usize,
    pub files_with_tenant_logic: Vec<String>,
    pub migration_recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TenancyEvidence {
    pub evidence_type: String,          // "session_storage", "sql_filter", "config", "module", "parameter"
    pub file_path: String,
    pub detail: String,
    pub line_hint: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TenantResolution {
    pub mechanism: String,              // "subdomain", "header", "session", "cookie", "querystring"
    pub module_class: Option<String>,   // if implemented as IHttpModule
    pub file_path: String,
}
```

Confidence classification:
- **High**: 3+ different evidence types (e.g., session storage + SQL filtering + config key)
- **Medium**: 2 evidence types
- **Low**: 1 evidence type (could be false positive)
- **None**: 0 evidence

#### Sub-task 7.3: Add to report and render

Add `pub multi_tenancy: MultiTenancyReport` to `FullProjectMigrationReport`.

Render after "Authentication & Authorization" (multi-tenancy is closely related to auth):

```markdown
## Multi-Tenancy Analysis

**Multi-tenant**: Yes (confidence: High)
**Tenant ID column**: `TenantId` (Integer)
**Isolation strategy**: Shared database, shared schema (filtered by TenantId)
**Tenant resolution**: Subdomain-based via `TenantModule.vb` IHttpModule
**Tenant-filtered queries**: {count}
**Files with tenant logic**: {count}

### Detection Evidence
| Type | File | Detail |
|------|------|--------|
| Session Storage | Login.aspx.vb | `Session("TenantId") = user.TenantId` |
| SQL Filter | DataAccess.vb | `WHERE TenantId = @tenantId` (23 queries) |
| Config Key | web.config | `<add key="TenantMode" value="Subdomain" />` |
| HTTP Module | App_Code/TenantModule.vb | `TenantModule : IHttpModule` resolves from Host header |
| Method Parameter | Reports/SalesReport.vb | `Function GetSalesData(tenantId As Integer)` |

### Modern Migration Strategy
1. **Tenant resolution**: Replace `TenantModule` IHttpModule with ASP.NET Core middleware
   ```csharp
   app.UseMiddleware<TenantResolutionMiddleware>();
   ```
2. **Data access**: Use EF Core Global Query Filters for automatic tenant filtering
   ```csharp
   modelBuilder.Entity<Order>().HasQueryFilter(o => o.TenantId == _tenantContext.TenantId);
   ```
3. **DI scope**: Register `ITenantContext` as scoped service (one per request)
4. **Connection strings**: If separate DBs, use `IDbContextFactory<T>` with tenant-specific connections
5. **Session → Claims**: Move `Session["TenantId"]` to JWT claims or `HttpContext.Items`

### Risk Assessment
- **CRITICAL**: {count} SQL queries filter by TenantId — missing ANY filter causes data leak between tenants
- {count} files access tenant context → all must be updated to use DI-injected `ITenantContext`
- Tenant resolution module must be migrated FIRST (Wave 0) — everything depends on it
```

---

### Gap 8: Email / Notification + Background Job Patterns

**Current behavior**: The VB extractor detects legacy COM `CDO.Message` (mapping to `System.Net.Mail.SmtpClient`) but does not detect modern .NET email patterns. No extractor detects background processing patterns. The `pattern_detection_service.rs` detects `System.Threading.Timer` only in the context of Windows Service detection, not as a general background job pattern.

**Why it matters**: Email and background jobs are infrastructure-level concerns that must be migrated to entirely different patterns:
- `SmtpClient` is obsolete in .NET 6+ → migrate to `IEmailSender` abstraction + SendGrid/Mailgun/Azure Communication Services
- `System.Web.Mail` (really old) → same migration path
- `ThreadPool.QueueUserWorkItem` → `IHostedService` or Hangfire/Quartz.NET
- `BackgroundWorker` → `IHostedService` or `BackgroundService`
- `System.Timers.Timer` / `System.Threading.Timer` → `IHostedService` with `PeriodicTimer`
- `Task.Run()` in fire-and-forget → `IHostedService` with channel/queue
- Scheduled tasks (Windows Task Scheduler calling .aspx pages) → Hangfire recurring jobs

Without detecting these, the AI agent will either miss them entirely (leaving the migrated app without email or background processing) or discover them mid-migration and have to restructure.

**Files to modify**:
- `crates/engram_server/src/services/full_project_migration_service.rs` — detect email and background patterns in code

#### Sub-task 8.1: Detect email patterns

```rust
fn detect_email_patterns(
    code_files: &[(String, String)],
    codebehind_contents: &[(String, String)],
) -> EmailPatternReport
```

Regex patterns:

```rust
// System.Net.Mail.SmtpClient
static SMTP_CLIENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:New\s+)?SmtpClient\s*[\(\.]").unwrap()
});

// MailMessage
static MAIL_MESSAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:New\s+)?MailMessage\s*\(").unwrap()
});

// System.Web.Mail (very old)
static WEB_MAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bSystem\.Web\.Mail\b").unwrap()
});

// Attachment
static ATTACHMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:New\s+)?Attachment\s*\(").unwrap()
});

// AlternateView (HTML email)
static ALTERNATE_VIEW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bAlternateView\.CreateAlternateViewFromString\s*\(").unwrap()
});

// SMTP config in web.config
// <system.net><mailSettings><smtp from="..."><network host="..." />
static SMTP_CONFIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<smtp\s[^>]*>.*?</smtp>|<smtp\s[^/]*/>"#).unwrap()
});

// CDO.Message (already in VB extractor, but also check CS files)
static CDO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)CreateObject\s*\(\s*"CDO\.Message"\s*\)"#).unwrap()
});
```

#### Sub-task 8.2: Detect background job patterns

```rust
fn detect_background_job_patterns(
    code_files: &[(String, String)],
    codebehind_contents: &[(String, String)],
    global_asax_content: Option<&str>,
) -> BackgroundJobReport
```

Regex patterns:

```rust
// ThreadPool.QueueUserWorkItem
static THREAD_POOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bThreadPool\.QueueUserWorkItem\s*\(").unwrap()
});

// BackgroundWorker
static BG_WORKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:New\s+)?BackgroundWorker\b").unwrap()
});

// Task.Run (fire-and-forget)
static TASK_RUN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bTask\.Run\s*\(").unwrap()
});

// System.Timers.Timer / System.Threading.Timer
static TIMER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:New\s+)?(?:System\.(?:Timers|Threading)\.)?Timer\s*\(").unwrap()
});

// Thread creation
static THREAD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:New\s+)?Thread\s*\(\s*(?:AddressOf|New\s+ThreadStart)\s").unwrap()
});

// Hangfire (if already using)
static HANGFIRE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bBackgroundJob\.(?:Enqueue|Schedule|ContinueWith)\s*[\(<]").unwrap()
});

// Quartz.NET (if already using)
static QUARTZ_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bIScheduler\b|\bJobBuilder\.Create\b|\bTriggerBuilder\.Create\b").unwrap()
});

// Response page timer trick (calling .aspx on schedule via curl/wget)
// Detectable from Application_Start setting up a timer to hit a page
static SELF_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)WebClient\s*\(\s*\)\.Download(?:String|Data)\s*\(\s*"(?:http|~/).*\.aspx"#).unwrap()
});
```

#### Sub-task 8.3: Build report structs

```rust
#[derive(Debug, Clone, Serialize)]
pub struct EmailPatternReport {
    pub has_email: bool,
    pub email_patterns: Vec<EmailPattern>,
    pub smtp_config: Option<SmtpConfig>,
    pub total_email_files: usize,
    pub uses_html_email: bool,
    pub uses_attachments: bool,
    pub uses_legacy_cdo: bool,
    pub uses_legacy_web_mail: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailPattern {
    pub file_path: String,
    pub pattern_type: String,         // "SmtpClient", "MailMessage", "CDO", "System.Web.Mail"
    pub count: usize,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmtpConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub from_address: Option<String>,
    pub uses_credentials: bool,
    pub uses_ssl: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundJobReport {
    pub has_background_jobs: bool,
    pub patterns: Vec<BackgroundJobPattern>,
    pub total_background_files: usize,
    pub uses_thread_pool: bool,
    pub uses_timers: bool,
    pub uses_task_run: bool,
    pub uses_bg_worker: bool,
    pub uses_hangfire: bool,
    pub uses_quartz: bool,
    pub fire_and_forget_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundJobPattern {
    pub file_path: String,
    pub pattern_type: String,         // "ThreadPool", "Timer", "Task.Run", "BackgroundWorker", "Thread"
    pub count: usize,
    pub modern_equivalent: String,    // "IHostedService", "BackgroundService", "Hangfire"
    pub risk_level: String,           // fire-and-forget Task.Run = High risk
}
```

#### Sub-task 8.4: Add to report and render

Add `pub email_patterns: EmailPatternReport` and `pub background_jobs: BackgroundJobReport` to `FullProjectMigrationReport`.

Add to `CrossCuttingSummary`:
```rust
pub has_email: bool,
pub has_background_jobs: bool,
```

Render after "Caching Strategy":

```markdown
## Email & Notifications

**Email sending**: Yes ({file_count} files)
**SMTP config**: mail.company.com:587 (SSL, credentials)
**HTML email**: Yes ({count} files use AlternateView)
**Attachments**: Yes ({count} files)
**Legacy CDO**: {count} files (COM interop)
**Legacy System.Web.Mail**: {count} files (obsolete)

### Email Usage
| File | Pattern | Count | Modern Equivalent |
|------|---------|-------|-------------------|
| Services/EmailService.vb | SmtpClient + MailMessage | 5 | IEmailSender / SendGrid SDK |
| Notifications/OrderConfirm.vb | SmtpClient + AlternateView | 2 | Razor email templates + SendGrid |
| Legacy/SendMail.asp | CDO.Message | 3 | IEmailSender |

### Migration Strategy
- `SmtpClient` → **Obsolete in .NET 6+** — replace with `IEmailSender` abstraction
- Register `IEmailSender` implementation in DI: SendGrid, Mailgun, or Azure Communication Services
- Move SMTP config from `<system.net><mailSettings>` to `appsettings.json`
- HTML email templates → Razor templates with strongly-typed models
- CDO.Message → IEmailSender (biggest rewrite — COM object to modern API)

## Background Processing

**Background jobs**: Yes ({file_count} files)
**Fire-and-forget**: {count} (HIGH RISK — request may end before task completes)
**Timers**: {count}
**ThreadPool**: {count}

### Background Job Inventory
| File | Pattern | Count | Risk | Modern Equivalent |
|------|---------|-------|------|-------------------|
| Services/ReportGen.vb | ThreadPool.QueueUserWorkItem | 3 | High | BackgroundService + Channel<T> |
| App_Code/CacheWarmer.vb | System.Timers.Timer | 1 | Medium | IHostedService + PeriodicTimer |
| Admin/BulkImport.aspx.vb | Task.Run (fire-and-forget) | 2 | High | Hangfire BackgroundJob.Enqueue |
| Global.asax.vb | Timer in Application_Start | 1 | Medium | IHostedService |

### Migration Strategy
- `ThreadPool.QueueUserWorkItem` → `BackgroundService` with `Channel<T>` for work queue
- `System.Timers.Timer` / `System.Threading.Timer` → `IHostedService` with `PeriodicTimer`
- `Task.Run()` fire-and-forget → **DANGEROUS** — use Hangfire `BackgroundJob.Enqueue()` or `IHostedService`
- `BackgroundWorker` → `BackgroundService` (same pattern, different base class)
- `New Thread(AddressOf Work)` → `BackgroundService` or `Task.Run` with proper lifetime management
- **WARNING**: {count} fire-and-forget patterns will silently fail in ASP.NET Core (request ends → task cancelled)
```

---

## Implementation Order

Execute in this order to minimize rework and maximize incremental value:

| Step | Gap | What | Why This Order |
|------|-----|------|---------------|
| 1 | Gap 3 | Assembly/NuGet references | Foundation — tells AI agent what packages/framework exist. Requires adding `.csproj`/`.vbproj` discovery to `tools.rs`, so do this first alongside any tool handler changes. |
| 2 | Gap 6 | VB.NET→C# translation flags | Self-contained scan of existing `code_files` content. No graph queries needed. Informs all subsequent migration guidance. |
| 3 | Gap 2 | Third-party control detection | Requires `webforms.rs` regex expansion + `control_mapping.rs` table expansion. These are `engram_index` changes that must be done before the report can surface third-party data. |
| 4 | Gap 4 | OutputCache/caching patterns | Requires `webforms.rs` + `state_extractor.rs` changes. Both are `engram_index` changes. |
| 5 | Gap 1 | Code-behind method inventory | Graph-only queries. Depends on nothing new, but is the largest sub-task. |
| 6 | Gap 5 | URL routing/rewrite rules | Parses web.config + code files. Self-contained. |
| 7 | Gap 7 | Multi-tenancy detection | Scans code files + web.config + Global.asax. Self-contained. |
| 8 | Gap 8 | Email + background jobs | Scans code files. Self-contained, smallest risk. |

After all 8 gaps: Update `CrossCuttingSummary`, `render_markdown()`, and per-page dossier rendering with all new data fields.

---

## Files to Modify

| File | Changes | Scope |
|------|---------|-------|
| `crates/engram_index/src/webforms.rs` | Expand control regex to include 15+ vendor prefixes; add OutputCache directive detection | Medium |
| `crates/engram_index/src/control_mapping.rs` | Add 30+ third-party control mappings (Telerik, DevExpress, Infragistics, ComponentArt) | Medium |
| `crates/engram_index/src/state_extractor.rs` | Add HttpRuntime.Cache.Insert/Add/Get/Remove, Response.Cache.*, SqlCacheDependency detection | Medium |
| `crates/engram_server/src/tools.rs` | Add `.csproj`/`.vbproj` discovery; parse with solution_parser; add `project_references` to `ProjectFileBundle` | Small |
| `crates/engram_server/src/services/full_project_migration_service.rs` | **Major**: Add 8 new struct groups (~30 structs total), 8 new analysis functions, 8 new markdown sections, expand `FullProjectMigrationReport` with 8 new fields, expand `CrossCuttingSummary` with ~10 new aggregation fields, expand per-page dossier rendering with 6 new per-page lines, add package replacement lookup table (40+ entries) | Large |
| `crates/engram_server/src/models/requests.rs` | No changes needed | — |

**New files**: None. All changes are additions to existing files.

---

## Verification Criteria

### Must-pass checks

1. `cargo check --all-targets` — compiles clean (zero errors, zero new warnings)
2. `cargo fmt --all` — formatted
3. All existing tests pass (especially `full_project_migration_service::tests::*`, `webforms_mutation_test`, `control_mapping::tests::*`, `state_extractor` tests)
4. Any new `webforms.rs` regex patterns must not break existing control detection tests
5. Any new `state_extractor.rs` patterns must not break existing state detection tests

### New test requirements

Each gap should have at least 3 tests:

**Gap 1 (Method inventory)**:
- Test that method nodes from graph produce correct `MethodInfo` structs
- Test `MethodKind` classification (lifecycle vs event handler vs helper)
- Test empty code-behind (no methods) produces empty inventory gracefully

**Gap 2 (Third-party controls)**:
- Test `webforms.rs` regex detects `<telerik:RadGrid>`, `<dx:ASPxGridView>`, `<ig:WebDataGrid>`
- Test `control_mapping.rs` lookup returns correct mappings for `RadGrid`, `ASPxGridView`
- Test unmapped controls (e.g., `<telerik:RadSpell>`) appear in `unmapped_controls`
- Test existing `asp:` controls still work (regression)

**Gap 3 (Dependencies)**:
- Test `.csproj` parsing returns correct `PackageRef` entries
- Test package replacement lookup returns correct modern equivalents
- Test unknown packages return `None` for replacement

**Gap 4 (Caching)**:
- Test `<%@ OutputCache Duration="60" VaryByParam="id" %>` detection
- Test `HttpRuntime.Cache.Insert("key", value)` detection
- Test `Response.Cache.SetExpires()` detection
- Test existing `Cache["key"]` bracket access still works (regression)

**Gap 5 (URL routing)**:
- Test IIS rewrite rule extraction from web.config XML
- Test `MapPageRoute` detection in Global.asax
- Test `Server.Transfer` detection

**Gap 6 (VB translation)**:
- Test VB-specific construct detection: `On Error Resume Next`, `Module`, `WithEvents`, `Like`, `IsNumeric`
- Test C# files are correctly excluded from VB analysis
- Test mixed VB/C# project correctly reports both languages

**Gap 7 (Multi-tenancy)**:
- Test `Session("TenantId")` detection
- Test SQL `WHERE TenantId = @tenantId` detection
- Test confidence classification (high/medium/low/none)

**Gap 8 (Email + background)**:
- Test `SmtpClient`/`MailMessage` detection
- Test `ThreadPool.QueueUserWorkItem` detection
- Test `Task.Run` fire-and-forget detection
- Test legacy CDO detection

### Functional verification

Run `analyze_full_project_migration` on a real indexed WebForms project and verify the markdown output contains ALL of these sections (in addition to the existing Phase 32 sections):

- [ ] Project Dependencies — with NuGet packages table, framework assemblies, compatible packages
- [ ] Language & Translation Analysis — with VB flag summary, highest-risk files, migration strategy
- [ ] Multi-Tenancy Analysis — with evidence table, resolution mechanism, migration strategy
- [ ] Caching Strategy — with output cache table, programmatic cache keys, SQL dependencies
- [ ] URL Routing & Rewriting — with rewrite rules, page routes, code-based URL manipulation
- [ ] Email & Notifications — with email usage table, SMTP config, migration strategy
- [ ] Background Processing — with job inventory, fire-and-forget warnings, migration strategy
- [ ] Code-Behind Method Inventory — with aggregate summary and per-file method tables
- [ ] Third-Party Control Libraries — with vendor summary, unmapped controls, licensing notes
- [ ] Updated per-page dossiers — each page now includes: method inventory table, third-party controls, VB flags, caching directives

### Complete Section Order (after Phase 33)

The full markdown report should have these sections in this order:

```
## Executive Summary
## Project Dependencies                    ← NEW (Gap 3)
## Language & Translation Analysis          ← NEW (Gap 6)
## Authentication & Authorization
## Multi-Tenancy Analysis                   ← NEW (Gap 7)
## Application Lifecycle (Global.asax)
## Configuration (web.config)
## URL Routing & Rewriting                  ← NEW (Gap 5)
## State Management (Project-Wide)
## Caching Strategy                         ← NEW (Gap 4)
## Data Access Patterns
## Code-Behind Method Inventory             ← NEW (Gap 1)
## Service Endpoints
## JavaScript & Client-Side Dependencies
## GIS / Spatial Analysis
## Third-Party Control Libraries            ← NEW (Gap 2)
## Design Anti-Patterns
## Email & Notifications                    ← NEW (Gap 8)
## Background Processing                    ← NEW (Gap 8)
## Classic ASP Files
## Reports (SSRS / Crystal)
## Migration Wave Plan
## Cross-Cutting Concerns
## Page-by-Page Dossiers                    ← ENHANCED (all gaps add per-page data)
## Risk Assessment
```

### Quality Bar

An AI agent reading this report should be able to answer ALL of these questions without making any additional tool calls:

**From Phase 32 (must still work)**:
1. "What JavaScript files affect Page X, and what server endpoints do they call?"
2. "Which GIS library is used and what's the modern equivalent for Blazor?"
3. "What appSettings keys exist and which files use them?"
4. "Are there any God Object pages that should be split before migration?"
5. "What ASMX web services exist and what should replace them?"
6. "What does Application_Start do and how does it map to Program.cs?"
7. "Are there any Classic ASP files still in the project?"
8. "What Crystal Reports exist and can they be auto-migrated?"
9. "What is the complete dependency chain from Page X through JS through AJAX to the backend service?"
10. "In what order should I migrate files, and what blocks what?"

**New from Phase 33**:
11. "What methods does AdminPage.aspx.vb contain, and what are their signatures?" (Gap 1)
12. "Which methods in this code-behind access SQL vs session vs COM objects?" (Gap 1)
13. "Is Telerik RadGrid used? What's the Blazor equivalent and what properties need mapping?" (Gap 2)
14. "What DevExpress controls exist and do they have MudBlazor alternatives?" (Gap 2)
15. "What NuGet packages are installed and which need replacement for .NET 8?" (Gap 3)
16. "Is EntityFramework 6 used? What's the EF Core migration path?" (Gap 3)
17. "Which pages use OutputCache and what are their caching parameters?" (Gap 4)
18. "What programmatic cache keys exist and what's the IMemoryCache equivalent?" (Gap 4)
19. "What URL rewrite rules exist and how do they map to ASP.NET Core routing?" (Gap 5)
20. "Are there any Server.Transfer calls that need complete refactoring?" (Gap 5)
21. "Is this a VB.NET project? How many On Error Resume Next patterns need fixing?" (Gap 6)
22. "Does the code use WithEvents/Handles? How many need explicit event subscription?" (Gap 6)
23. "Is this application multi-tenant? How is tenant context resolved?" (Gap 7)
24. "How many SQL queries include tenant filtering? What's the EF Core Global Query Filter strategy?" (Gap 7)
25. "How does the app send email? What should replace SmtpClient?" (Gap 8)
26. "Are there fire-and-forget Task.Run calls that will break in ASP.NET Core?" (Gap 8)
27. "What background timers exist and how should they become IHostedService?" (Gap 8)

If ANY of these questions cannot be answered from the report, the task is not complete.

---

## What This Is NOT

- **NOT new extractors for most gaps** — Gaps 1, 5, 6, 7, 8 scan existing content with regex in the service layer. Only Gaps 2 and 4 modify `engram_index` extractors.
- **NOT new graph edge kinds** — All 33 existing EdgeKind variants are sufficient. Gap 4 uses existing `reads_state`/`writes_state` edges with enhanced metadata.
- **NOT new tools** — `analyze_full_project_migration` already exists. This task enriches its output.
- **NOT refactoring** — All changes are additive. No existing structs, functions, or tests are modified (except minor expansions to `FullProjectMigrationReport`, `CrossCuttingSummary`, and `ProjectFileBundle`).

This is about **closing the last 20% of blind spots** so that an AI agent has complete knowledge of every aspect of a legacy application — methods, controls, packages, caching, routing, language quirks, tenancy, email, and background jobs — from a single tool call.

---

## Rules

1. Only production code — no stubs, no `todo!()`, no simplified implementations
2. Never simplified code/functions — every regex must handle real-world edge cases (case-insensitive, multiline, VB and C# syntax variations)
3. If multiple solutions exist, always pick the more enterprise/future-proof solution
4. All struct fields must have real values, not placeholder defaults
5. All markdown sections must render complete tables with real column data
6. Per-page dossier enhancements must appear for EVERY page, not just a summary
7. Fallback to content scanning when graph data is sparse (not all projects are fully indexed)
8. Resilient to missing data — if a `.csproj` file can't be found, skip dependency inventory gracefully (warn, don't crash)
9. No new dependencies — use only `regex`, `serde`, `serde_json`, and existing crate dependencies
