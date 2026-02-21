# Phase 30: End-to-End Migration Engine

## Overview

Engram-MCP v2 has world-class **legacy code comprehension** (29 phases of extraction, graph modeling, safety gates, and migration planning). But there are 8 structural gaps between "understands the legacy code" and "autonomously migrates it." This phase closes them all.

**Goal**: After Phase 30, an agent using Engram can take a real ASP.NET WebForms/VB.NET application with ADO.NET, GIS, and session-heavy state management and produce a working modern replacement — not just a plan.

---

## Gap 1: Target Code Generation Pipeline

### Problem
Engram produces migration plans with adapter templates and contract test templates, but these are **comment-only strings** — not compilable code. The agent still writes every line of target code from scratch with no scaffold assistance.

### What Exists
- `migration_service.rs`: `generate_contract_test_template()` returns `// Contract test: {name}` comment blocks
- `migration_service.rs`: 5 adapter types (Facade, Translator, Proxy, StateBridge, AuthBridge) as comment strings
- `pattern_detection_service.rs`: `modern_target` field on anti-patterns (e.g., "MediatR IRequestHandler")
- `blast_radius_service.rs`: Guidance items with `modern_pattern` suggestions (e.g., "Repository + Dapper/EF Core")

### Deliverables

#### 1a. Control Mapping Catalog (`engram_index/src/control_mapping.rs`)

A deterministic lookup table that maps every detected WebForms control to its modern equivalent across multiple target stacks.

```rust
pub struct ControlMapping {
    pub legacy_control: &'static str,     // e.g., "asp:GridView"
    pub legacy_namespace: &'static str,   // e.g., "System.Web.UI.WebControls"
    pub blazor_equivalent: &'static str,  // e.g., "QuickGrid<T>"
    pub react_equivalent: &'static str,   // e.g., "AG Grid / TanStack Table"
    pub angular_equivalent: &'static str, // e.g., "mat-table"
    pub properties_map: &'static [(&'static str, &'static str)], // legacy prop → modern prop
    pub event_map: &'static [(&'static str, &'static str)],      // legacy event → modern event
    pub data_binding_pattern: &'static str, // How to wire data in modern stack
    pub notes: &'static str,
}
```

**Minimum catalog entries (35+ controls)**:
| Legacy Control | Category |
|---|---|
| `asp:GridView` | Data display |
| `asp:DetailsView` | Data display |
| `asp:FormView` | Data display |
| `asp:ListView` | Data display |
| `asp:Repeater` | Data display |
| `asp:DataList` | Data display |
| `asp:TextBox` | Input |
| `asp:DropDownList` | Input |
| `asp:CheckBox` / `asp:CheckBoxList` | Input |
| `asp:RadioButton` / `asp:RadioButtonList` | Input |
| `asp:Calendar` | Input |
| `asp:FileUpload` | Input |
| `asp:Button` | Action |
| `asp:LinkButton` | Action |
| `asp:ImageButton` | Action |
| `asp:HyperLink` | Navigation |
| `asp:Menu` | Navigation |
| `asp:TreeView` | Navigation |
| `asp:SiteMapPath` | Navigation |
| `asp:Panel` | Layout |
| `asp:PlaceHolder` | Layout |
| `asp:MultiView` / `asp:View` | Layout |
| `asp:Wizard` | Layout |
| `asp:UpdatePanel` | AJAX |
| `asp:ScriptManager` | AJAX |
| `asp:Timer` | AJAX |
| `asp:UpdateProgress` | AJAX |
| `asp:SqlDataSource` | Data access |
| `asp:ObjectDataSource` | Data access |
| `asp:LinqDataSource` | Data access |
| `asp:EntityDataSource` | Data access |
| `asp:Label` | Display |
| `asp:Literal` | Display |
| `asp:Image` | Display |
| `asp:ValidationSummary` | Validation |
| `asp:RequiredFieldValidator` | Validation |
| `asp:CompareValidator` | Validation |
| `asp:RangeValidator` | Validation |
| `asp:RegularExpressionValidator` | Validation |
| `asp:CustomValidator` | Validation |

For each control, include:
- **Properties map**: e.g., `GridView.DataKeyNames` → `@key` in Blazor, `rowKey` in AG Grid
- **Event map**: e.g., `GridView.RowCommand` → Blazor `@onclick` per-row, React `onRowClick` handler
- **Data binding**: e.g., `GridView.DataSourceID="SqlDataSource1"` → Blazor `Items="@data"` with injected service
- **Validation controls**: Map to HTML5 validation, FluentValidation, or Zod schemas depending on target

#### 1b. Scaffold Generator Tool (`generate_migration_scaffold`)

New MCP tool that takes:
- A file path (already indexed)
- A target stack (`blazor`, `react`, `angular`)
- An optional migration_class override

And produces a **compilable skeleton** of the target file by:

1. Reading the file's extracted graph data (controls, events, data bindings, state access, SQL calls)
2. Looking up each control in the mapping catalog
3. Generating a skeleton with:
   - Correct imports/using statements for the target framework
   - Component structure with mapped controls
   - Event handler stubs with `// TODO: migrate from {legacy_method}` comments referencing the original
   - Data access interface stubs (repository pattern) replacing inline SQL
   - State management hooks replacing Session/ViewState access
   - Validation rules mapped from ASP.NET validators

**Request struct**:
```rust
pub struct GenerateMigrationScaffoldRequest {
    pub project_id: String,
    pub file_path: String,
    pub target_stack: String,        // "blazor", "react", "angular"
    #[serde(default)]
    pub include_test_scaffold: bool, // Also generate test file
    #[serde(default)]
    pub output_format: String,       // "full" (default) or "diff" (shows what maps to what)
}
```

**Output**: The generated scaffold code as a string, plus a mapping report showing what each section corresponds to in the legacy code.

#### 1c. Data Access Layer Generator

For files with `SqlCalls`/`QueriesTable` edges, generate:
- A repository interface (`IOrderRepository`) with methods derived from the SQL operations found
- A Dapper/EF Core implementation skeleton
- A DTO class derived from the `ReadsColumn` edges (column names → properties)

Example: If extraction found `SELECT OrderId, CustomerName, Total FROM Orders WHERE CustomerId = @id`, generate:
```csharp
public interface IOrderRepository {
    Task<Order> GetByCustomerIdAsync(int customerId);
}
public class Order {
    public int OrderId { get; set; }
    public string CustomerName { get; set; }
    public decimal Total { get; set; }
}
```

---

## Gap 2: VB.NET Semantic Deep Extraction

### Problem
VB.NET has a dedicated extractor (`vb_extractor.rs`) that handles symbols, SQL, calls, `Handles` clauses, ADO.NET column access, and lifecycle methods. But several VB.NET-specific patterns that are common in real legacy apps are not extracted.

### What Exists
- Tree-sitter VB grammar with `queries/vb.scm`
- FQN symbol extraction (Namespace.Class.Method)
- `Handles` clause detection (Me.Event, MyBase.Event)
- ADO.NET column access (row indexer, Item member, ordinal)
- VB constant resolution in state extractor
- VB continuation line handling

### Deliverables

#### 2a. `On Error` Pattern Detection

VB.NET legacy code heavily uses unstructured error handling:
```vbnet
On Error Resume Next
' ... risky code ...
If Err.Number <> 0 Then
    ' handle
End If
On Error GoTo 0
```

Add extraction of:
- `On Error Resume Next` regions (mark as `error_suppression_region`)
- `On Error GoTo <label>` handlers
- `Err.Number` / `Err.Description` / `Err.Clear` usage
- Emit a new **anti-pattern edge** (`AntiPattern` kind) from the containing method to an `insight` node: `"unstructured_error_handling"` with metadata:
  - `pattern`: "on_error_resume_next" | "on_error_goto"
  - `line_range`: start-end lines of the suppression region
  - `err_object_accesses`: count of `Err.*` references
  - `modern_equivalent`: "try/catch with specific exception types"

#### 2b. `With` Block Detection

```vbnet
With order
    .CustomerName = txtName.Text
    .Total = CDec(txtTotal.Text)
    .Save()
End With
```

Detect `With` blocks and resolve the `.Property` references to the `With` target variable. This matters because:
- Property assignments inside `With` blocks are currently invisible to state/binding extraction
- The `With` target may be a database entity, UI control, or service object

Emit edges from the `With` target to each accessed property, classified as:
- `ReadsState` if reading
- `WritesState` if writing
- `DataBinding` if the target is a control

#### 2c. Late Binding Detection

```vbnet
Dim obj As Object = CreateObject("Excel.Application")
obj.Workbooks.Open("file.xlsx")
```

Detect `CreateObject()` and `GetObject()` calls as COM interop patterns. Emit:
- `insight` node: `"com_interop_usage"` with `prog_id` metadata (e.g., "Excel.Application")
- `AntiPattern` edge: late binding prevents static analysis and is a migration blocker
- `modern_equivalent`: "Use the specific NuGet package (e.g., EPPlus, ClosedXML for Excel)"

Also detect:
- `Dim x As Object` followed by member access (late-bound calls)
- `Microsoft.VisualBasic.Interaction.CallByName()` usage

#### 2d. `My.` Namespace Detection

VB.NET's `My` namespace provides quick access to application resources:
```vbnet
My.Settings.ConnectionString
My.Computer.FileSystem.ReadAllText("file.txt")
My.Application.Log.WriteEntry("message")
My.User.Name
My.Resources.SomeImage
```

Detect `My.*` usage and emit:
- `ReadsState` edge for `My.Settings.*` (maps to `ConfigurationManager.AppSettings`)
- `insight` node for `My.Computer.*` (maps to `System.IO` / `System.Environment`)
- `insight` node for `My.Application.*` (maps to logging framework)
- `insight` node for `My.User.*` (maps to `ClaimsPrincipal`)
- `insight` node for `My.Resources.*` (maps to embedded resource management)

#### 2e. `ReDim Preserve` and Dynamic Array Detection

```vbnet
ReDim Preserve arr(UBound(arr) + 1)
```

Detect `ReDim` / `ReDim Preserve` as an anti-pattern:
- Emit `AntiPattern` edge: "dynamic_array_resize"
- `modern_equivalent`: "Use List(Of T) or ImmutableArray<T>"
- Severity: Minor (performance concern, not correctness)

---

## Gap 3: Database Migration Strategy Advisor

### Problem
The graph knows `SqlCalls`, `QueriesTable`, `HasColumn`, `ForeignKey`, `ReadsColumn` edges — it maps *what* touches the database. But it doesn't suggest *how* to transform the data access layer.

### What Exists
- `ddl_extractor.rs`: CREATE TABLE / column / foreign key extraction
- `vb_extractor.rs`: SqlCommand, CommandText, ExecuteReader/NonQuery/Scalar, ADO.NET column access
- `blast_radius_service.rs`: `sql_concat_score` dimension
- `pattern_detection_service.rs`: SqlDataSource coupling detection
- `ReadsColumn` edges with column names

### Deliverables

#### 3a. Data Access Pattern Classifier

New function in `pattern_detection_service.rs` that classifies each file's data access pattern:

| Pattern | Detection Rule | Migration Target |
|---------|---------------|-----------------|
| `inline_sql` | Raw SQL strings in code (SqlCommand with literal) | Repository + parameterized queries |
| `stored_procedure` | `CommandType.StoredProcedure` or `EXEC/EXECUTE` | Keep SP, wrap in repository |
| `dataset_adapter` | `SqlDataAdapter` + `DataSet`/`DataTable` usage | EF Core or Dapper with POCOs |
| `data_reader_manual` | `ExecuteReader` + manual column indexing | Dapper `Query<T>()` |
| `sql_datasource_declarative` | `<asp:SqlDataSource>` in markup | Repository + DI injection |
| `entity_framework_v1` | `ObjectContext` or `DbContext` (old EF) | Upgrade to EF Core 8+ |
| `linq_to_sql` | `DataContext` usage | Replace with EF Core |
| `typed_dataset` | `.xsd` files or `TableAdapter` | Dapper + POCOs |

Emit classification as metadata on the file node and as an `insight` node with the pattern name.

#### 3b. Repository Interface Generator

New function that takes a file path and produces a repository interface by analyzing its graph edges:

1. Collect all `QueriesTable` edges → table names
2. Collect all `SqlCalls` edges → SQL operation types (SELECT/INSERT/UPDATE/DELETE)
3. Collect all `ReadsColumn` edges → column names per table
4. Group by table to produce methods:
   - SELECT with WHERE → `GetBy{Column}Async()`
   - SELECT without WHERE → `GetAllAsync()`
   - INSERT → `CreateAsync(entity)`
   - UPDATE → `UpdateAsync(entity)`
   - DELETE → `DeleteBy{Column}Async()`
5. Generate DTO from column names (with inferred C# types where possible from column naming conventions: `*Id` → int, `*Name` → string, `*Date` → DateTime, `*Amount`/`*Total`/`*Price` → decimal, `Is*`/`Has*` → bool)

Output: Repository interface + DTO class + Dapper implementation skeleton as a structured result.

#### 3c. SQL Injection Risk Scorer

Enhance the existing `sql_concat_score` in blast_radius with a dedicated tool or sub-report:

For each file with `SqlCalls` edges, analyze whether:
- SQL is parameterized (`@param` or `?` placeholders) → Safe
- SQL uses string concatenation (`"SELECT * FROM X WHERE id=" + userId`) → **Critical risk**
- SQL uses string interpolation (`$"SELECT ... {id}"`) → **Critical risk**
- SQL uses stored procedures → Generally safe (depends on SP)

The `vb_extractor.rs` already detects some of this (concat SQL). Extend it to:
- Classify each SQL statement as `parameterized`, `concatenated`, `interpolated`, or `stored_proc`
- Add a `sql_injection_risk` field to the `SqlCalls` edge metadata
- Surface in blast radius and safety evaluation

---

## Gap 4: Runtime Instrumentation Pipeline

### Problem
The `RuntimeEvidenceBatch` schema exists, `validate_batch()` works, `ReconciliationResult` is defined, and ADP vNext has a reconciliation-aware gate. But there's no way to **capture** runtime behavior from a running legacy app.

### What Exists
- `engram_core/src/runtime_evidence.rs`: `RuntimeEvent`, `RuntimeEvidenceBatch`, `ReconciliationResult`, `validate_batch()`
- `engram_server/src/tools.rs`: `get_instrumentation_pack` and `ingest_instrumentation_logs` tools
- ADP vNext: `ReconciliationScores` in gate evaluation

### Deliverables

#### 4a. Instrumentation Code Generator Tool (`generate_instrumentation_code`)

New MCP tool that generates injectable instrumentation code for the legacy app. Given a project_id and target files, it produces:

**For ASP.NET WebForms (C#)**:
```csharp
// Auto-generated by Engram-MCP — add to Global.asax.cs
public class EngramInstrumentation : IHttpModule {
    public void Init(HttpApplication app) {
        app.BeginRequest += OnBeginRequest;
        app.EndRequest += OnEndRequest;
        app.Error += OnError;
    }
    private void OnBeginRequest(object sender, EventArgs e) {
        var ctx = HttpContext.Current;
        EngramLogger.LogEvent(new RuntimeEvent {
            event_type = "route",
            source_path = ctx.Request.Path,
            target = ctx.Request.HttpMethod,
            timestamp = DateTime.UtcNow
        });
    }
    // ... session access tracking, SQL execution tracking ...
}
```

**For ASP.NET WebForms (VB.NET)**:
Same as above but in VB.NET syntax.

**What it instruments**:
1. **HTTP request/response** (route events): path, method, status code, timing
2. **Session access**: key reads/writes with timestamps (wraps `HttpSessionState`)
3. **SQL execution**: command text, execution time, row count (wraps `DbCommand.Execute*`)
4. **Control interactions**: postback source (`__EVENTTARGET`), event argument
5. **Error events**: unhandled exceptions with stack trace

**Output format**: The generated code as a string, plus installation instructions (which files to modify, which web.config entries to add).

#### 4b. Runtime Evidence Reconciliation Tool (`reconcile_runtime_evidence`)

New MCP tool that takes:
- `project_id` (with indexed static analysis)
- An ingested `RuntimeEvidenceBatch`

And produces a `ReconciliationResult` by comparing:
- Static paths (from graph edges: which files call which, which files access which state) vs. runtime paths (which files were actually hit, which state was actually accessed)
- Static SQL (extracted SQL strings) vs. runtime SQL (actually executed queries)
- Static control wiring (event handler edges) vs. runtime control interactions (actual postbacks)

For each static path, classify as:
- **Confirmed**: Runtime evidence shows this path is exercised
- **Contradicted**: Runtime evidence shows this path is NOT exercised (dead code candidate)
- **Inconclusive**: No runtime data for this path

Wire the output into the ADP reconciliation gate so that `ReconciliationScores` are populated from real data.

---

## Gap 5: State Management Migration Advisor

### Problem
The graph knows every `ReadsState`/`WritesState`/`StateAffinity` edge. The state extractor captures Session, ViewState, Application, Cache, and Cookie access with literal key resolution. But there's no tool that says "here's how to migrate your state management."

### What Exists
- `state_extractor.rs`: Full state access detection (8 stores, C# + VB.NET)
- `StateAffinity` edges: Co-accessed state keys clustered by method
- `state_extractor.rs`: ViewState schema hints, session clustering, endpoint suggestions
- `trace_state_usage` tool: Traces all access points for a given state key
- Anti-pattern: "Session Soup" detection

### Deliverables

#### 5a. State Migration Strategy Tool (`suggest_state_migration`)

New MCP tool that analyzes all state access in a project and produces a per-key migration recommendation:

**Input**: `project_id`

**Analysis per state key**:

| State Store | Analysis | Migration Targets |
|---|---|---|
| `Session["Key"]` | Count readers/writers, co-access patterns, data type inference from usage | **Option A**: JWT claim (if auth-related, read-heavy) |
| | | **Option B**: Redis/distributed cache (if shared across requests) |
| | | **Option C**: Client-side state (if page-scoped, small data) |
| | | **Option D**: Database (if persistent, large data) |
| `ViewState["Key"]` | Scope analysis (single page? cross-page?), size estimation, read/write ratio | **Option A**: Component state (React useState, Blazor @bind) |
| | | **Option B**: URL query parameter (if navigation-scoped) |
| | | **Option C**: Hidden field (if form submission scope) |
| `Application["Key"]` | Write frequency, read count, concurrency implications | **Option A**: Singleton service (DI) |
| | | **Option B**: IMemoryCache (if cache-like usage) |
| | | **Option C**: Static configuration (if write-once) |
| `Cache["Key"]` | Expiration patterns, invalidation triggers, size | **Option A**: IDistributedCache (Redis) |
| | | **Option B**: IMemoryCache (if single-server) |
| `Request.Cookies` / `Response.Cookies` | Auth cookies vs. preference cookies vs. tracking | **Option A**: JWT in HttpOnly cookie (auth) |
| | | **Option B**: localStorage (preferences, client-only) |

**Output per key**:
```json
{
  "state_key": "Session:UserId",
  "store_type": "Session",
  "readers": ["Login.aspx.cs:45", "Default.aspx.cs:12", "Orders.aspx.cs:30"],
  "writers": ["Login.aspx.cs:78"],
  "access_pattern": "write_once_read_many",
  "data_type_inference": "int (based on comparison with integer)",
  "affinity_group": ["Session:UserName", "Session:UserRole"],
  "recommended_target": "JWT claim",
  "reasoning": "Auth-related, written at login, read across many pages, small data",
  "migration_code_hint": "services.AddAuthentication().AddJwtBearer(); // UserId as ClaimTypes.NameIdentifier"
}
```

#### 5b. ViewState Elimination Report

Specifically for ViewState — the hardest state to migrate — produce a per-page report:

For each page with ViewState access:
1. List all ViewState keys with read/write locations
2. Classify each key's **lifecycle**: single-postback (can eliminate), cross-postback (needs alternative), cross-page (needs server state)
3. Estimate ViewState payload size contribution (heuristic from key count × average value size)
4. Suggest elimination strategy:
   - **Eliminate**: Key is only used in one event handler → move to local variable
   - **To component state**: Key tracks UI toggle/visibility → React useState / Blazor field
   - **To hidden field**: Key carries form data between postbacks → `<input type="hidden">`
   - **To server session**: Key carries complex state → server-side session with Redis
5. Flag keys where ViewState is used as a **crutch for missing URL state** (common pattern: storing filter/sort state that should be query parameters)

---

## Gap 6: Characterization Test Generator

### Problem
Before migrating a legacy app with little-to-no test coverage, you need to capture its current behavior as tests. Engram has deep extraction data (event handlers, data flows, state transitions, SQL queries) but doesn't generate tests from it.

### What Exists
- `migration_service.rs`: `generate_contract_test_template()` (comment-only)
- `webforms_mutation_test.rs`: 12 mutation scenarios (tests extractor correctness, not app behavior)
- `ADP calibration corpus`: 7+ scenarios for decision validation
- Migration slicer: Can extract event handlers, data methods, SQL queries per file

### Deliverables

#### 6a. Characterization Test Generator Tool (`generate_characterization_tests`)

New MCP tool that takes a `project_id` and `file_path` and generates characterization tests based on extraction data:

**What it generates (per event handler)**:

For a `Button_Click` handler that:
- Reads `Session["UserId"]`
- Executes `SELECT * FROM Orders WHERE CustomerId = @id`
- Sets `Label1.Text = result.Count.ToString()`

Generate:
```csharp
[TestFixture]
public class OrderPage_CharacterizationTests {
    [Test]
    public void Button_Click_Should_Query_Orders_By_Session_UserId() {
        // Arrange
        // Session["UserId"] is read → mock session with test value
        var session = new MockHttpSession();
        session["UserId"] = 42;

        // Act
        // Executes SQL: SELECT * FROM Orders WHERE CustomerId = @id
        // TODO: Capture actual result from legacy app run

        // Assert
        // Sets Label1.Text → verify output matches captured behavior
        // Assert.That(page.Label1.Text, Is.EqualTo("CAPTURED_VALUE"));
    }
}
```

**Categories of generated tests**:

1. **Event handler tests**: One test per event handler, with:
   - Session/state mocking from `ReadsState` edges
   - SQL call expectations from `SqlCalls` edges
   - UI mutation assertions from control property assignments
   - Postback event source setup from `TriggersPostback` edges

2. **Data flow tests**: For each SQL query found:
   - Input parameters from `ParameterBinding` edges
   - Expected columns from `ReadsColumn` edges
   - Connection string reference from `connection_string` nodes

3. **State transition tests**: For each state key:
   - Write locations → test that the write happens
   - Read locations → test that reads get expected value
   - Affinity groups → test co-access patterns

4. **Navigation tests**: For each `Response.Redirect` / `Server.Transfer`:
   - Source page + condition
   - Target page
   - Query string parameters passed

**Output format**: Test class as a string, plus a coverage map showing which extraction edges each test covers.

#### 6b. API Contract Test Generator

For files exposed as web services (`ExposesWebService`, `ExposesHttpHandler`, `ExposesWcfService` edges):

Generate HTTP-level contract tests:
```csharp
[Test]
public async Task OrderService_GetOrders_Returns_Expected_Schema() {
    // Arrange — from extraction: ASMX endpoint /OrderService.asmx
    var client = new HttpClient();

    // Act
    var response = await client.PostAsync(
        "/OrderService.asmx/GetOrders",
        new StringContent(soapEnvelope, Encoding.UTF8, "text/xml")
    );

    // Assert
    Assert.That(response.StatusCode, Is.EqualTo(HttpStatusCode.OK));
    // Verify SOAP response contains expected elements
    // (derived from service method return type)
}
```

---

## Gap 7: GIS Migration Deep Extraction

### Problem
The current GIS extraction (in `js_extractor.rs`) detects Google Maps, Leaflet, OpenLayers, and OpenStreetMap usage. It extracts API keys (masked), zoom levels, center coordinates, and emits `SpatialCall` edges. But real GIS migrations need deeper extraction.

### What Exists
- `js_extractor.rs`: 4 library detection (Google Maps, Leaflet, OpenLayers, OSM)
- GIS config extraction: API keys, zoom, center coords
- `SpatialCall` edges with library metadata
- `modern_equivalent` field per GIS call (react-leaflet, google-maps-react)
- Anti-pattern: "Tight GIS Coupling" detection
- Blast radius: `gis_coupling_score` dimension

### Deliverables

#### 7a. GIS Layer Inventory

Extend `js_extractor.rs` to detect and catalog:

**Map layers**:
- Tile layers: `L.tileLayer('https://{s}.tile.openstreetmap.org/...')` → extract tile URL template
- WMS layers: `L.tileLayer.wms('url', {layers: '...'})` → extract WMS endpoint + layer names
- Vector layers: `L.geoJSON(data)` → detect GeoJSON usage
- Marker layers: count markers, detect clustering (`L.markerClusterGroup`)

**Coordinate systems**:
- `ol.proj.fromLonLat()` → EPSG:4326 to EPSG:3857 conversion
- `L.CRS.*` usage → detect custom CRS
- Google Maps uses WGS84 (EPSG:4326) implicitly

**Geocoding services**:
- `google.maps.Geocoder()` → Google Geocoding API
- Custom geocoding endpoints (detect `geocode` in AJAX URLs)

**Drawing/editing tools**:
- `L.Control.Draw` (Leaflet Draw)
- `google.maps.drawing.DrawingManager`
- `ol.interaction.Draw`

Emit a `gis_inventory` insight node per file with structured metadata:
```json
{
  "library": "leaflet",
  "version_hint": "1.7+",
  "tile_sources": ["openstreetmap"],
  "layers": ["tile", "marker", "geojson"],
  "has_drawing_tools": true,
  "has_geocoding": false,
  "has_clustering": true,
  "coordinate_system": "EPSG:4326",
  "api_keys_detected": 0,
  "modern_target": {
    "react": "react-leaflet + @react-leaflet/core",
    "blazor": "BlazorLeaflet",
    "angular": "ngx-leaflet"
  }
}
```

#### 7b. Esri/ArcGIS Detection

Many enterprise legacy apps use Esri's ArcGIS JavaScript API. Add detection for:
- `esri/Map`, `esri/views/MapView`, `esri/layers/*` (AMD module loading)
- `new Map()`, `new MapView()`, `new FeatureLayer()` (ES module style)
- ArcGIS REST API endpoint detection in AJAX calls (`/arcgis/rest/services/`)
- `dojo.require('esri.map')` (Dojo-style legacy ArcGIS API)

This is common in government, utility, and real estate applications — exactly the kinds of apps that need WebForms migration.

---

## Gap 8: Multi-Technology Legacy Support

### Problem
Real legacy .NET shops don't just have WebForms. They have classic ASP, COM components, Crystal Reports, SSRS, Windows Services, and more. Currently, Engram only extracts WebForms/VB.NET/C# patterns.

### What Exists
- File type detection: .aspx, .ascx, .master, .asmx, .ashx, .svc, .asax, .cs, .vb, .config, .sql
- No classic ASP (.asp) support
- No Crystal Reports (.rpt) support
- No SSRS (.rdl, .rdlc) support
- No Windows Service detection
- COM interop detection only via Gap 2c (late binding)

### Deliverables

#### 8a. Classic ASP Extractor (`engram_index/src/extractors/asp_classic_extractor.rs`)

Detect and extract from `.asp` files:

**Server-side VBScript blocks**:
- `<% ... %>` and `<script runat="server">` blocks
- `Response.Write`, `Response.Redirect`, `Server.Transfer`
- `Request.QueryString("key")`, `Request.Form("key")`
- `Session("key")`, `Application("key")` (same as WebForms but VBScript syntax)
- `Server.CreateObject("ADODB.Connection")` → COM database access
- `Set conn = Server.CreateObject("ADODB.Connection")` + `conn.Open connectionString`
- `Set rs = conn.Execute("SQL")` → SQL extraction from classic ADO
- `#include file="..."` and `#include virtual="..."` → `IncludesFile` edges

**Emit**:
- `file` nodes for .asp files
- `function` nodes for `Sub`/`Function` blocks
- `SqlCalls` edges for `conn.Execute()` and `Command.Execute`
- `ReadsState`/`WritesState` for Session/Application
- `IncludesFile` edges for `#include` directives
- `insight` node: `"classic_asp_file"` to flag for migration priority (these are the oldest, highest-risk files)

#### 8b. Report Definition Extractor

Detect `.rdl` (SSRS) and `.rdlc` (client-side SSRS) files:

Parse the XML to extract:
- Data sources and connection strings
- SQL queries embedded in `<CommandText>` elements
- Parameters (`<ReportParameter>`)
- Dataset fields (column names)
- Subreport references

Emit:
- `file` node for the report
- `SqlCalls` edges for embedded queries
- `QueriesTable` edges for referenced tables
- `Dependency` edges to subreports
- `connection_string` node references

For Crystal Reports (`.rpt`): These are binary files and can't be parsed directly. Instead, detect:
- References to Crystal Reports in code: `CrystalDecisions.CrystalReports.Engine`
- `ReportDocument.Load()` calls with .rpt file paths
- `ReportDocument.SetDataSource()` calls
- `CrystalReportViewer` controls in .aspx files
- Emit `Dependency` edges from code to referenced .rpt files
- Emit `insight` node: `"crystal_reports_usage"` with migration guidance → "Migrate to SSRS, DevExpress Reports, or generate PDF via code"

#### 8c. Windows Service / Background Job Detection

Detect:
- Classes inheriting `ServiceBase` (Windows Service)
- `[WebMethod]` attributes in `.asmx.cs`/`.asmx.vb` (already handled)
- `Timer` usage in services (`System.Timers.Timer`, `System.Threading.Timer`)
- `Quartz.NET` job scheduling (`IJob` implementation, `JobBuilder.Create`)
- `Hangfire` usage (`BackgroundJob.Enqueue`, `RecurringJob.AddOrUpdate`)

Emit:
- `insight` node: `"windows_service"` / `"scheduled_job"` / `"background_task"`
- `modern_equivalent`: "Use ASP.NET Core BackgroundService / Hangfire / Azure Functions"

---

## Implementation Priority Order

| Priority | Gap | Effort | Impact | Rationale |
|----------|-----|--------|--------|-----------|
| **P0** | Gap 1 (Code Generation) | Large | Critical | Directly enables autonomous migration output |
| **P0** | Gap 3 (Database Strategy) | Medium | Critical | Data layer is the hardest part of most migrations |
| **P1** | Gap 2 (VB.NET Deep) | Medium | High | Many real apps are VB.NET; missing patterns cause silent extraction gaps |
| **P1** | Gap 5 (State Migration) | Medium | High | State management is where WebForms migrations actually fail |
| **P1** | Gap 6 (Test Generation) | Medium | High | No tests = no confidence in migration correctness |
| **P2** | Gap 4 (Runtime Pipeline) | Medium | Medium | Enables ADP reconciliation but requires running legacy app |
| **P2** | Gap 7 (GIS Deep) | Small | Medium | Important for GIS-heavy apps but not universal |
| **P2** | Gap 8 (Multi-Tech) | Medium | Medium | Expands addressable legacy scope beyond WebForms |

---

## New Tools Summary

| Tool Name | Gap | Description |
|-----------|-----|-------------|
| `generate_migration_scaffold` | 1 | Produce compilable target-stack skeleton from extraction data |
| `generate_instrumentation_code` | 4 | Produce injectable runtime instrumentation for legacy app |
| `reconcile_runtime_evidence` | 4 | Compare static analysis vs runtime behavior |
| `suggest_state_migration` | 5 | Per-key state migration strategy with code hints |
| `generate_characterization_tests` | 6 | Produce test skeletons from extraction data |

## New Modules Summary

| Module | Gap | Location |
|--------|-----|----------|
| `control_mapping.rs` | 1 | `engram_index/src/control_mapping.rs` |
| `scaffold_generator.rs` | 1 | `engram_server/src/services/scaffold_service.rs` |
| `asp_classic_extractor.rs` | 8 | `engram_index/src/extractors/asp_classic_extractor.rs` |
| `report_extractor.rs` | 8 | `engram_index/src/extractors/report_extractor.rs` |

## New Edge Kinds Needed

None — all new extractions use existing edge kinds (AntiPattern, Insight, SqlCalls, ReadsState, WritesState, IncludesFile, Dependency, QueriesTable).

## New Node Types Needed

| Node Type | Gap | Description |
|-----------|-----|-------------|
| `report` | 8 | SSRS/Crystal report file |
| `background_service` | 8 | Windows Service or scheduled job |

## Estimated New Config Fields

| Field | Default | Gap |
|-------|---------|-----|
| `scaffold_default_target_stack` | `"blazor"` | 1 |
| `scaffold_include_tests` | `true` | 1 |
| `enable_classic_asp_extraction` | `false` | 8 |
| `enable_report_extraction` | `false` | 8 |
| `characterization_test_framework` | `"nunit"` | 6 |

---

## Acceptance Criteria

1. **Gap 1**: `generate_migration_scaffold` for a WebForms page with GridView + SqlDataSource produces a compilable Blazor component with correct imports, component structure, and data service stubs
2. **Gap 2**: VB.NET file with `On Error Resume Next`, `With` blocks, `My.Settings`, and `CreateObject` emits correct anti-pattern/insight nodes
3. **Gap 3**: File with 5 different SQL queries produces correct repository interface with typed methods and DTO
4. **Gap 4**: `generate_instrumentation_code` produces a working HttpModule that logs Session access and SQL execution
5. **Gap 5**: Project with 20 Session keys produces per-key recommendations with reasoning and code hints
6. **Gap 6**: Event handler with Session read + SQL + UI write generates a test with correct arrange/act/assert sections
7. **Gap 7**: File using Leaflet with tile layer + GeoJSON + marker clustering produces complete GIS inventory
8. **Gap 8**: `.asp` file with `Server.CreateObject("ADODB.Connection")` + `conn.Execute("SQL")` produces SqlCalls edges

---

## Test Count Target

Each gap should add 10-20 unit tests (extraction correctness, edge emission, scaffold output). Target: **100+ new tests** across all gaps, bringing the project total to 350+.
