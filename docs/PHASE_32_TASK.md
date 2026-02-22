# Phase 32: `analyze_full_project_migration` — From 60% to 100%

## Problem Statement

We built 20K+ lines of extraction, graph, and service code across Phases 28–31. The extractors work. The graph stores rich data. The individual services return useful results. But the **one tool that matters** — `analyze_full_project_migration` — only surfaces ~60% of what an AI agent needs to migrate a real VB.NET ASP.NET WebForms application with jQuery, GIS/Google Maps, and typical enterprise patterns.

The graph already contains ManipulatesDom, TriggersPostback, ApiCall, SpatialCall, InjectsScript, AntiPattern edges, plus insight nodes for GIS inventories, Classic ASP findings, and Crystal/SSRS reports. **None of this data reaches the final report.**

This task fixes every gap. When complete, an AI agent calling `analyze_full_project_migration` once will receive everything it needs to understand and plan the migration of an entire legacy project — no follow-up tool calls required.

---

## Gap Inventory (8 Gaps, 22 Sub-tasks)

### Gap 1: Recursive File Discovery (tool handler)

**Current behavior**: The fallback path in `tools.rs` uses `tokio::fs::read_dir()` which only reads the top-level directory. Real projects have files in `Pages/`, `Controls/`, `Admin/`, `App_Code/`, `Scripts/`, etc.

**Also missing**: The handler only discovers `.aspx`/`.ascx`/`.master` files. It does not discover `.js` files, `.asp` files, `.rdl`/`.rdlc` files, or `Global.asax`.

**File**: `crates/engram_server/src/tools.rs` (handler starting at ~line 8613)

#### Sub-task 1.1: Recursive directory walker

Replace the non-recursive `read_dir` fallback with a recursive async walker. Use `tokio::fs::read_dir` in a `VecDeque`-based BFS:

```rust
async fn discover_files_recursive(
    dir: &Path,
    extensions: &[&str],  // [".aspx", ".ascx", ".master", ".js", ".asp", ".rdl", ".rdlc"]
    max_files: usize,
) -> Vec<PathBuf>
```

Skip `bin/`, `obj/`, `node_modules/`, `.git/`, `packages/` directories. Return relative paths. Cap at `max_files` total.

#### Sub-task 1.2: Discover JS files

After discovering markup files, also discover all `.js` files in the project. Store them separately — they're needed for Gap 2.

From graph: `file_nodes.iter().filter(|n| n.name.to_lowercase().ends_with(".js"))`.
Fallback: recursive walker with `.js` extension.

Pre-read all JS file content asynchronously alongside markup files.

#### Sub-task 1.3: Discover Classic ASP files

Discover `.asp` files the same way. These are needed for Gap 8.

#### Sub-task 1.4: Discover report files

Discover `.rdl`, `.rdlc` files. These are needed for Gap 8.

#### Sub-task 1.5: Discover Global.asax

Explicitly check for `Global.asax` (and `Global.asax.cs` / `Global.asax.vb`) in the project root. Pre-read its content. This is needed for Gap 6.

#### Sub-task 1.6: Pass new file categories to the service

Extend `FileContent` or add new parameters to `analyze_full_project()`:

```rust
pub struct ProjectFileBundle {
    pub markup_files: Vec<FileContent>,        // .aspx, .ascx, .master
    pub js_files: Vec<(String, String)>,        // (rel_path, content)
    pub classic_asp_files: Vec<(String, String)>,// (rel_path, content)
    pub report_files: Vec<(String, String)>,    // (rel_path, content)
    pub global_asax: Option<FileContent>,       // Global.asax + codebehind
    pub web_config_content: Option<String>,
    pub code_files: Vec<(String, String)>,      // all .cs/.vb for auth scanning
}
```

Update the `analyze_full_project()` signature to accept `ProjectFileBundle` instead of the current scatter of parameters.

---

### Gap 2: JavaScript / jQuery Analysis

**Current behavior**: The report contains zero information about JavaScript. The graph has `ManipulatesDom`, `TriggersPostback`, `ApiCall` edges (from `js_extractor.rs`) but the report never queries them.

**Why it matters**: A typical WebForms app has JavaScript that:
- Manipulates server controls via jQuery selectors (`$("[id$='gvResults']")`)
- Triggers postbacks via `__doPostBack()`
- Makes AJAX calls to `.asmx`/`.ashx`/`.aspx` WebMethods
- Uses `PageMethods.MethodName()` for script service calls
- All of these create hidden dependencies that must be migrated

**File**: `crates/engram_server/src/services/full_project_migration_service.rs`

#### Sub-task 2.1: Query JS-related edges from graph

In `analyze_full_project()`, after the project-wide analyses, query these edge kinds:

```rust
// All ManipulatesDom edges for this project
let dom_edges = graph.list_edges_by_kind(project_id, "ManipulatesDom", 10_000)?;
// All TriggersPostback edges
let postback_edges = graph.list_edges_by_kind(project_id, "TriggersPostback", 10_000)?;
// All ApiCall edges
let api_call_edges = graph.list_edges_by_kind(project_id, "ApiCall", 10_000)?;
```

Group these by source file (the JS file) and by target (the server control / endpoint).

#### Sub-task 2.2: Build `JsAnalysisSummary` struct

```rust
#[derive(Debug, Clone, Serialize)]
pub struct JsAnalysisSummary {
    pub total_js_files: usize,
    pub js_files_with_server_deps: usize,  // JS files that reference ASP.NET controls

    // DOM manipulation inventory
    pub dom_manipulations: Vec<JsDomRef>,

    // Postback triggers
    pub postback_triggers: Vec<JsPostbackRef>,

    // AJAX calls to server endpoints
    pub ajax_calls: Vec<JsAjaxCall>,

    // Per-page JS dependency map: which JS files affect which ASPX pages
    pub page_js_dependencies: BTreeMap<String, Vec<String>>,  // aspx → [js files]

    // Migration risk items
    pub inline_script_files: Vec<String>,  // files with <script> blocks (not separate .js)
    pub jquery_version_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsDomRef {
    pub js_file: String,
    pub target_control: String,
    pub selector_type: String,  // "jquery_ends_with", "asp_client_id", "getelementbyid"
}

#[derive(Debug, Clone, Serialize)]
pub struct JsPostbackRef {
    pub js_file: String,
    pub target_control: String,
    pub unique_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsAjaxCall {
    pub js_file: String,
    pub target_url: String,
    pub transport: String,        // "jquery_ajax", "fetch", "xhr", "page_methods"
    pub target_method: Option<String>,
    pub target_type: String,      // "web_service", "http_handler", "wcf_service", "page"
}
```

#### Sub-task 2.3: Build the JS dependency map

For each ASPX page, determine which JS files reference its controls:
1. From `ManipulatesDom` edges: source=JS file, target=control → find which ASPX page owns that control via `Contains` edges
2. From `TriggersPostback` edges: source=JS file, target=control → same mapping
3. From `<script src="...">` references in markup content (regex scan)

Result: `page_js_dependencies: BTreeMap<String, Vec<String>>` — for each ASPX page, the list of JS files that must be migrated alongside it.

#### Sub-task 2.4: Add JS section to `FullProjectMigrationReport`

Add `pub js_analysis: JsAnalysisSummary` to `FullProjectMigrationReport`.

#### Sub-task 2.5: Render JS section in markdown

After the "Data Access Patterns" section, add:

```markdown
## JavaScript & Client-Side Dependencies

**JS files**: {total} ({with_server_deps} with server-side dependencies)
**DOM manipulations**: {count} (jQuery: {jquery_count}, getElementById: {getbyid_count}, ASP ClientID: {clientid_count})
**Postback triggers**: {count} __doPostBack calls from JS
**AJAX calls**: {count} ({by_transport breakdown})

### AJAX Endpoint Inventory
| JS File | Target URL | Transport | Method | Target Type |
|---------|-----------|-----------|--------|-------------|
| scripts/map.js | Services/MapData.asmx/GetPolygons | jquery_ajax | GetPolygons | web_service |
| ... | ... | ... | ... | ... |

### Page ↔ JS Dependencies
| Page | JS Files | DOM Refs | Postbacks | AJAX Calls |
|------|----------|----------|-----------|------------|
| Default.aspx | scripts/main.js, scripts/map.js | 5 | 2 | 3 |
| ... | ... | ... | ... | ... |

### Migration Impact
- {count} JS files manipulate server control IDs → must update to modern component selectors
- {count} `__doPostBack` calls → must replace with component event handlers / SignalR
- {count} AJAX calls to .asmx → must migrate to Web API / Minimal API endpoints
- {count} PageMethods calls → must migrate to Blazor JS interop / API calls
```

#### Sub-task 2.6: Per-page JS summary in dossier rendering

In the page-by-page dossier section of the markdown, for each page, add a JS dependencies line:

```markdown
**JS dependencies**: scripts/main.js (3 DOM refs, 1 postback), scripts/map.js (2 AJAX calls)
```

This uses the `page_js_dependencies` map to show which JS files affect each page.

---

### Gap 3: GIS / Spatial Analysis

**Current behavior**: The graph contains `SpatialCall` edges and `insight` nodes with rich GIS metadata (library, class count, has_places_api, has_directions, migration_complexity, modern_target_react/blazor/angular). The blast_radius_service computes a `gis_coupling_score`. None of this appears in the full project report.

**Why it matters**: GIS is often the hardest part of a WebForms migration. Google Maps API v2→v3 changes, Esri ArcGIS AMD→ES modules, Leaflet plugin ecosystems — the AI agent needs a complete inventory to plan the migration.

**File**: `crates/engram_server/src/services/full_project_migration_service.rs`

#### Sub-task 3.1: Query GIS data from graph

```rust
// All SpatialCall edges
let spatial_edges = graph.list_edges_by_kind(project_id, "SpatialCall", 10_000)?;

// All insight nodes that are GIS inventories
let gis_insights: Vec<_> = graph
    .query_nodes(project_id, Some("insight"), Some("gis_inventory"), None, 1_000)?
    .into_iter()
    .chain(
        graph.query_nodes(project_id, Some("insight"), Some("google_maps_inventory"), None, 1_000)?
    )
    .collect();
```

#### Sub-task 3.2: Build `GisAnalysisSummary` struct

```rust
#[derive(Debug, Clone, Serialize)]
pub struct GisAnalysisSummary {
    pub has_gis: bool,
    pub libraries_detected: Vec<GisLibrarySummary>,
    pub total_spatial_calls: usize,
    pub files_with_gis: Vec<String>,
    pub migration_complexity: String,  // "low", "medium", "high"
    pub modern_targets: GisModernTargets,
}

#[derive(Debug, Clone, Serialize)]
pub struct GisLibrarySummary {
    pub library: String,           // "google_maps", "leaflet", "openlayers", "esri_arcgis"
    pub files: Vec<String>,
    pub class_count: usize,
    pub features: Vec<String>,     // ["Places API", "StreetView", "Directions", ...]
    pub has_3d: bool,
    pub has_drawing: bool,
    pub has_geocoding: bool,
    pub has_clustering: bool,
    pub has_wms: bool,
    pub api_keys_detected: usize,
    pub api_style: Option<String>,  // for Esri: "AMD", "ES modules", "REST"
}

#[derive(Debug, Clone, Serialize)]
pub struct GisModernTargets {
    pub react: Vec<String>,     // ["@react-google-maps/api", "react-leaflet"]
    pub blazor: Vec<String>,    // ["BlazorGoogleMaps", "BlazorLeaflet"]
    pub angular: Vec<String>,   // ["@angular/google-maps", "ngx-leaflet"]
}
```

#### Sub-task 3.3: Populate from graph insight metadata

Parse the insight node metadata (stored as edge weight or node properties) to populate `GisLibrarySummary`. The JS extractor stores metadata like `library`, `class_count`, `has_places_api`, `has_streetview`, `migration_complexity`, `modern_target_react`, etc.

#### Sub-task 3.4: Render GIS section in markdown

After the JS section:

```markdown
## GIS / Spatial Analysis

**Libraries**: Google Maps (3 files), Esri ArcGIS (2 files)
**Total spatial calls**: 47
**Migration complexity**: High

### Google Maps
- **Files**: scripts/map.js, pages/MapView.aspx, controls/MapControl.ascx
- **Features**: Places API, Directions, Heatmap, KML layers, Drawing tools
- **API keys detected**: 2
- **Modern target ({target_stack})**: BlazorGoogleMaps NuGet package

### Esri ArcGIS
- **Files**: scripts/gis-viewer.js, scripts/spatial-query.js
- **API style**: AMD modules (legacy Dojo)
- **Features**: FeatureLayer, MapView, Geoprocessing, Editing, Portal
- **3D support**: Yes (SceneView)
- **Modern target ({target_stack})**: ArcGIS REST JS (@esri/arcgis-rest-request)

### Migration Considerations
- Google Maps JS API v3 → wrapper component needed (direct DOM → component binding)
- Esri AMD → ES module migration required (Dojo→modern bundler)
- {count} WMS layer endpoints must be preserved
- {count} API keys must be migrated to server-side configuration
```

#### Sub-task 3.5: GIS in per-page dossiers

For each page that has GIS (check `SpatialCall` edges where source matches the page or its JS dependencies), add:

```markdown
**GIS**: Google Maps (Places API, Directions) via scripts/map.js — complexity: High
```

---

### Gap 4: web.config Full Inventory

**Current behavior**: `auth_config_service` reads web.config but only extracts `<authentication>`, `<authorization>`, `<membership>`, `<roleManager>`. It ignores `<appSettings>`, `<connectionStrings>`, `<httpHandlers>`, `<httpModules>`, `<compilation>`, `<customErrors>`, etc.

**Why it matters**: Every `ConfigurationManager.AppSettings["key"]` in code-behind needs to become `IConfiguration["key"]` in modern .NET. Every `<connectionStrings>` entry needs to move to `appsettings.json`. Custom error pages, HTTP modules, compilation settings — all need migration.

**File**: `crates/engram_server/src/services/full_project_migration_service.rs`

#### Sub-task 4.1: Extract web.config sections

Add a function that parses web.config for non-auth sections:

```rust
fn extract_webconfig_inventory(web_config: &str) -> WebConfigInventory
```

Use regex to extract:
- `<appSettings>` → `Vec<AppSettingEntry>` (key, value)
- `<connectionStrings>` → `Vec<ConnectionStringEntry>` (name, provider, has_integrated_security)
- `<httpHandlers>` / `<system.webServer><handlers>` → `Vec<HandlerEntry>` (verb, path, type)
- `<httpModules>` / `<system.webServer><modules>` → `Vec<ModuleEntry>` (name, type)
- `<customErrors>` → `CustomErrorConfig` (mode, defaultRedirect, per-code redirects)
- `<compilation>` → `CompilationConfig` (debug, targetFramework, assemblies list)
- `<pages>` → `PagesConfig` (theme, masterPage, namespaces, controls)
- `<sessionState>` → `SessionStateConfig` (mode: InProc/StateServer/SQLServer/Custom, timeout, cookieless)

```rust
#[derive(Debug, Clone, Serialize)]
pub struct WebConfigInventory {
    pub app_settings: Vec<AppSettingEntry>,
    pub connection_strings: Vec<ConnectionStringEntry>,
    pub http_handlers: Vec<HandlerRegistration>,
    pub http_modules: Vec<ModuleRegistration>,
    pub custom_errors: Option<CustomErrorConfig>,
    pub compilation: Option<CompilationConfig>,
    pub session_state: Option<SessionStateConfig>,
    pub pages_config: Option<PagesConfig>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppSettingEntry {
    pub key: String,
    pub value_preview: String,  // first 30 chars, mask if looks like password/key
    pub used_by: Vec<String>,   // files that reference this key (from graph or grep)
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionStringEntry {
    pub name: String,
    pub provider: String,
    pub has_integrated_security: bool,
    pub used_by: Vec<String>,  // files that reference this connection string name
}
```

#### Sub-task 4.2: Cross-reference appSettings with code

For each appSettings key, scan `code_files` for `ConfigurationManager.AppSettings["keyname"]` or `WebConfigurationManager.AppSettings["keyname"]` to populate `used_by`.

For each connectionString name, scan for `ConfigurationManager.ConnectionStrings["name"]` to populate `used_by`.

#### Sub-task 4.3: Add to report struct and markdown

Add `pub web_config_inventory: WebConfigInventory` to `FullProjectMigrationReport`.

Render in markdown:

```markdown
## Configuration (web.config)

### Connection Strings
| Name | Provider | Integrated Auth | Used By |
|------|----------|-----------------|---------|
| DefaultConnection | System.Data.SqlClient | Yes | DataAccess.cs, Reports.cs |

### App Settings ({count} keys)
| Key | Preview | Used By |
|-----|---------|---------|
| GoogleMapsApiKey | AIzaSy... | scripts/map.js, MapPage.aspx.cs |
| SmtpServer | mail.company... | EmailService.cs |

### Session State
**Mode**: SQLServer | **Timeout**: 30min
→ Migration: Replace with distributed cache (Redis/IDistributedCache)

### HTTP Handlers ({count})
| Path | Type |
|------|------|
| *.asmx | System.Web.Services.Protocols.WebServiceHandlerFactory |
→ Migration: Replace with Minimal API / Controller endpoints

### HTTP Modules ({count})
| Name | Type |
|------|------|
| ErrorLog | Company.Modules.ErrorLogModule |
→ Migration: Replace with ASP.NET Core middleware

### Custom Errors
**Mode**: RemoteOnly | Default: ~/Error.aspx
→ Migration: Replace with UseExceptionHandler + UseStatusCodePagesWithReExecute
```

---

### Gap 5: Service Endpoint Inventory

**Current behavior**: Phase 13 extracts `ExposesWebService` (ASMX), `ExposesHttpHandler` (ASHX), `ExposesWcfService` (SVC) edges. These are in the graph but the full project report never queries them.

**Why it matters**: Every `.asmx` endpoint needs to become a Web API controller or Minimal API. Every `.ashx` handler needs middleware or an endpoint. Every WCF service needs gRPC or REST replacement. The AI agent needs to know exactly what endpoints exist and what they do.

**File**: `crates/engram_server/src/services/full_project_migration_service.rs`

#### Sub-task 5.1: Query service endpoints from graph

```rust
let web_service_edges = graph.list_edges_by_kind(project_id, "ExposesWebService", 1_000)?;
let http_handler_edges = graph.list_edges_by_kind(project_id, "ExposesHttpHandler", 1_000)?;
let wcf_service_edges = graph.list_edges_by_kind(project_id, "ExposesWcfService", 1_000)?;

// Also get registered modules and route handlers
let module_edges = graph.list_edges_by_kind(project_id, "RegistersModule", 1_000)?;
let handler_edges = graph.list_edges_by_kind(project_id, "RegistersHandler", 1_000)?;
```

#### Sub-task 5.2: Build `ServiceEndpointSummary` struct

```rust
#[derive(Debug, Clone, Serialize)]
pub struct ServiceEndpointSummary {
    pub web_services: Vec<ServiceEndpoint>,   // .asmx
    pub http_handlers: Vec<ServiceEndpoint>,  // .ashx
    pub wcf_services: Vec<ServiceEndpoint>,   // .svc
    pub http_modules: Vec<ServiceEndpoint>,   // IHttpModule implementations
    pub route_handlers: Vec<ServiceEndpoint>, // registered routes
    pub total_endpoints: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceEndpoint {
    pub file_path: String,
    pub service_name: String,     // class name or service name
    pub methods: Vec<String>,     // WebMethod names (if available from graph)
    pub modern_equivalent: String, // "Minimal API endpoint" / "gRPC service" / "Middleware"
    pub called_by: Vec<String>,   // JS files or pages that call this endpoint (from ApiCall edges)
}
```

#### Sub-task 5.3: Cross-reference with AJAX calls

For each web service, check if any `ApiCall` edge targets it. This tells us which JS files / pages depend on each endpoint — critical for migration ordering.

#### Sub-task 5.4: Render in markdown

```markdown
## Service Endpoints

**Web Services (ASMX)**: {count}
| File | Service | Methods | Called By |
|------|---------|---------|-----------|
| Services/MapData.asmx | MapDataService | GetPolygons, SaveMarker | scripts/map.js |

**HTTP Handlers (ASHX)**: {count}
| File | Handler | Modern Equivalent |
|------|---------|-------------------|
| Handlers/Download.ashx | FileDownloadHandler | Minimal API with FileResult |

**WCF Services (SVC)**: {count}
| File | Service | Modern Equivalent |
|------|---------|-------------------|
| Services/DataSync.svc | DataSyncService | gRPC service or Web API |

**HTTP Modules**: {count}
| Module | Type | Modern Equivalent |
|--------|------|-------------------|
| ErrorLog | ErrorLogModule | ASP.NET Core Middleware |

### Migration Impact
- {asmx_count} ASMX services → Web API / Minimal API controllers
- {ashx_count} ASHX handlers → Middleware or endpoint routes
- {wcf_count} WCF services → gRPC or REST API
- {module_count} HTTP modules → ASP.NET Core middleware pipeline
```

---

### Gap 6: Global.asax / Application Lifecycle

**Current behavior**: The WebForms extractor recognizes `Global.asax` as an `application` node type and links it to its code-behind. But the full project report has no section for application-level lifecycle events.

**Why it matters**: `Application_Start` often configures routing, DI registration, bundle config, area registration. `Session_Start` / `Session_End` manage session initialization. `Application_Error` handles global error logging. All of this maps to `Program.cs` / `Startup.cs` in modern .NET.

**File**: `crates/engram_server/src/services/full_project_migration_service.rs`

#### Sub-task 6.1: Parse Global.asax code-behind

Add a function to extract application lifecycle events:

```rust
fn extract_global_asax_info(
    markup_content: &str,
    codebehind_content: &str,
) -> GlobalAsaxSummary
```

Regex-extract these method signatures:
- `Application_Start` / `Application_OnStart`
- `Application_End`
- `Application_Error` / `Application_OnError`
- `Session_Start` / `Session_OnStart`
- `Session_End` / `Session_OnEnd`
- `Application_BeginRequest` / `Application_EndRequest`
- `Application_AuthenticateRequest`
- `Application_PostAuthenticateRequest`

For each found method, extract the method body and scan for:
- `RouteConfig.RegisterRoutes` / `RouteTable.Routes` → routing setup
- `BundleConfig.RegisterBundles` / `BundleTable.Bundles` → bundling
- `AreaRegistration.RegisterAllAreas` → MVC areas
- `GlobalConfiguration.Configure` → Web API config
- `Container.Register` / `kernel.Bind` / `builder.Register` → DI registration
- `Application["key"] =` → application state initialization
- `Session["key"] =` → session initialization

```rust
#[derive(Debug, Clone, Serialize)]
pub struct GlobalAsaxSummary {
    pub has_global_asax: bool,
    pub codebehind_class: Option<String>,
    pub lifecycle_events: Vec<GlobalLifecycleEvent>,
    pub startup_registrations: Vec<StartupRegistration>,
    pub modern_mapping: Vec<ModernMapping>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GlobalLifecycleEvent {
    pub event_name: String,
    pub line_count: usize,
    pub key_actions: Vec<String>,  // summarized actions found in method body
    pub modern_equivalent: String, // "Program.cs builder.Services..." / "app.UseMiddleware..."
}

#[derive(Debug, Clone, Serialize)]
pub struct StartupRegistration {
    pub registration_type: String,  // "routing", "bundling", "areas", "webapi", "di"
    pub detail: String,
}
```

#### Sub-task 6.2: Add to report and render

Add `pub global_asax: GlobalAsaxSummary` to `FullProjectMigrationReport`.

Render after the Auth section:

```markdown
## Application Lifecycle (Global.asax)

**Class**: `MyApp.Global` (Global.asax.cs)

### Lifecycle Events
| Event | Lines | Key Actions | Modern Equivalent |
|-------|-------|-------------|-------------------|
| Application_Start | 45 | RouteConfig, BundleConfig, DI registration | Program.cs builder setup |
| Application_Error | 12 | Error logging, redirect to error page | app.UseExceptionHandler() |
| Session_Start | 8 | Initialize cart, set culture | Middleware + ISession |
| Session_End | 3 | Cleanup temp files | No direct equivalent (use IHostedService) |

### Startup Registrations (→ Program.cs)
- **Routing**: RouteConfig.RegisterRoutes → app.MapControllerRoute / app.MapBlazorHub
- **Bundling**: BundleConfig.RegisterBundles → Vite/Webpack or ASP.NET Core bundling
- **DI**: Unity container → builder.Services (built-in DI)

### Migration Notes
- Application_Start content → Program.cs service registration + middleware pipeline
- Session_Start → ISession middleware or custom middleware
- Application_Error → UseExceptionHandler + ProblemDetails
```

---

### Gap 7: Anti-Pattern / Design Pattern Detection

**Current behavior**: `pattern_detection_service.rs` detects God Objects, Spaghetti Events, Session Soup, SqlDataSource Coupling, Tight GIS Coupling, and Windows Services. None of this appears in the full project report.

**Why it matters**: Anti-patterns directly affect migration strategy. A God Object page might need to be split before migration. Session Soup indicates state coupling that blocks parallel migration. The AI agent needs to know about these to plan correctly.

**File**: `crates/engram_server/src/services/full_project_migration_service.rs`

#### Sub-task 7.1: Call pattern detection service

```rust
use super::pattern_detection_service;

let anti_patterns = pattern_detection_service::detect_design_antipatterns(
    graph,
    project_id,
    15,  // god_threshold (Contains edges)
    5,   // spaghetti_threshold (cross-file Dependency in-edges)
    4,   // soup_threshold (Session keys accessed from N files)
)
.unwrap_or_else(|e| {
    tracing::warn!("anti-pattern detection failed: {e}");
    vec![]
});
```

#### Sub-task 7.2: Build `AntiPatternSummary`

```rust
#[derive(Debug, Clone, Serialize)]
pub struct AntiPatternSummary {
    pub total_anti_patterns: usize,
    pub by_type: BTreeMap<String, usize>,  // "God Object" → 3
    pub critical_items: Vec<AntiPatternItem>,
    pub migration_impact: Vec<String>,  // human-readable impact statements
}

#[derive(Debug, Clone, Serialize)]
pub struct AntiPatternItem {
    pub pattern_type: String,
    pub file_path: String,
    pub node_name: String,
    pub severity: String,
    pub detail: String,
    pub recommendation: String,
}
```

#### Sub-task 7.3: Add to report and render

```markdown
## Design Anti-Patterns

**Total detected**: {count}

| Type | Count | Impact |
|------|-------|--------|
| God Object | 3 | Must split before migration — too many responsibilities |
| Session Soup | 2 | Blocks parallel migration — shared mutable state |
| Spaghetti Events | 5 | Cross-file event chains — map dependencies carefully |
| SqlDataSource Coupling | 4 | Inline SQL + data binding — extract to repository |
| Tight GIS Coupling | 1 | GIS tightly bound to data — extract map service |

### Critical Items
- **God Object**: `AdminPage.aspx.cs` (47 methods, 12 event handlers) → Split into AdminUsers, AdminSettings, AdminReports components
- **Session Soup**: Session["UserPreferences"] accessed from 8 files → Extract to IUserPreferencesService
- ...

### Migration Impact
- 3 God Object pages should be split BEFORE migration (Wave 0 refactoring)
- Session Soup keys must be consolidated before parallel wave execution
- Spaghetti Events indicate hidden coupling — verify with characterization tests
```

#### Sub-task 7.4: Anti-patterns in per-page dossier

For each page, check if it appears in any anti-pattern finding. If so, add:

```markdown
**Anti-patterns**: God Object (47 methods), Spaghetti Events (called from 6 files)
```

---

### Gap 8: Classic ASP / SSRS / Crystal Reports

**Current behavior**: `asp_classic_extractor.rs` extracts COM objects, ADO connections, SQL, state access, includes, and functions from `.asp` files. `report_extractor.rs` extracts SSRS datasets/fields and Crystal Reports usage. Neither appears in the full project report.

**Why it matters**: Many enterprise WebForms projects have a mix of Classic ASP pages (not yet migrated from the ASP→ASP.NET transition), SSRS reports embedded in pages, and Crystal Reports with binary `.rpt` files. These are migration blockers that the AI agent must know about.

**File**: `crates/engram_server/src/services/full_project_migration_service.rs`

#### Sub-task 8.1: Analyze Classic ASP files

For each `.asp` file in `classic_asp_files`, call the extractor (or query the graph if already indexed):

```rust
// If already indexed, query the graph for insight nodes about Classic ASP
let asp_insights = graph.query_nodes(
    project_id, Some("insight"), None, None, 1_000
)?
.into_iter()
.filter(|n| n.name.contains("classic_asp") || n.metadata.get("migration_complexity").is_some())
.collect::<Vec<_>>();

// Query edges from .asp files
let asp_includes = graph.list_edges_by_kind(project_id, "IncludesFile", 5_000)?;
```

Build:
```rust
#[derive(Debug, Clone, Serialize)]
pub struct ClassicAspSummary {
    pub total_asp_files: usize,
    pub com_objects: Vec<ComObjectRef>,    // Server.CreateObject("ADODB.Connection")
    pub ado_connections: usize,
    pub sql_statements: usize,
    pub includes: Vec<IncludeRef>,
    pub state_accesses: usize,
    pub migration_effort_hours: f64,  // from insight metadata
}
```

#### Sub-task 8.2: Analyze report files

Query graph for report-related insights and edges:

```rust
let report_insights = graph.query_nodes(
    project_id, Some("insight"), None, None, 1_000
)?
.into_iter()
.filter(|n| n.name.contains("report") || n.name.contains("crystal") || n.name.contains("ssrs"))
.collect::<Vec<_>>();
```

Build:
```rust
#[derive(Debug, Clone, Serialize)]
pub struct ReportSummary {
    pub ssrs_reports: Vec<ReportInfo>,
    pub crystal_reports: Vec<CrystalReportInfo>,
    pub total_reports: usize,
    pub has_binary_rpt_files: bool,
    pub shared_data_sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportInfo {
    pub file_path: String,
    pub datasets: Vec<String>,
    pub parameters: usize,
    pub subreports: Vec<String>,
    pub migration_target: String,  // "Power BI" / "SSRS on modern" / "Telerik Reporting"
}
```

#### Sub-task 8.3: Render in markdown

```markdown
## Classic ASP Files

**Files**: {count} | **Estimated effort**: {hours}h

| File | COM Objects | SQL Statements | Includes | State Access |
|------|-------------|----------------|----------|--------------|
| login.asp | ADODB.Connection, ADODB.Recordset | 3 | header.inc, footer.inc | 2 Session reads |

### Migration Path
- Classic ASP → ASP.NET Core Razor Pages or Blazor
- COM objects (ADODB) → Entity Framework Core / Dapper
- Server-side includes → Partial views / Razor components
- `Response.Write` → Razor template syntax

## Reports (SSRS / Crystal)

**SSRS reports**: {count} | **Crystal Reports**: {count}

### SSRS Reports
| File | Datasets | Parameters | Subreports | Target |
|------|----------|------------|------------|--------|
| Sales.rdl | SalesData (3 tables) | 4 | SalesDetail.rdl | Power BI |

### Crystal Reports
| File | Report (.rpt) | Binary | Modern Equivalent |
|------|--------------|--------|-------------------|
| InvoiceViewer.aspx | Invoice.rpt | Yes | Power BI / SSRS |

**Warning**: {count} binary .rpt files cannot be automatically migrated — manual recreation required
```

---

## Implementation Order

Execute in this order to minimize rework:

| Step | What | Why First |
|------|------|-----------|
| 1 | Gap 1 (recursive file discovery + new file bundle) | Everything else depends on having the right files |
| 2 | Gap 4 (web.config inventory) | Small, self-contained, immediate value |
| 3 | Gap 6 (Global.asax) | Small, self-contained, uses pre-read content |
| 4 | Gap 5 (service endpoints) | Graph-only queries, no new file reading |
| 5 | Gap 7 (anti-patterns) | Graph-only, calls existing service |
| 6 | Gap 2 (JavaScript/jQuery) | Needs JS files from Gap 1, graph edges |
| 7 | Gap 3 (GIS/spatial) | Needs JS analysis from Gap 2, graph edges |
| 8 | Gap 8 (Classic ASP / reports) | Needs ASP/report files from Gap 1, graph edges |

---

## Files to Modify

| File | Changes |
|------|---------|
| `crates/engram_server/src/services/full_project_migration_service.rs` | Major: add 7 new struct groups, 5 new analysis functions, 7 new markdown sections, expand `FullProjectMigrationReport` with 7 new fields, expand `CrossCuttingSummary` with JS/GIS/anti-pattern aggregation |
| `crates/engram_server/src/tools.rs` | Major: replace file discovery with recursive walker, add JS/ASP/report file discovery, build `ProjectFileBundle`, discover Global.asax |
| `crates/engram_server/src/models/requests.rs` | Minor: no changes needed (request struct is fine) |

**No new files needed.** All changes are additions to existing files. No new services — we're surfacing data that existing services already extract.

---

## Verification Criteria

### Must-pass checks

1. `cargo check --all-targets` — compiles clean
2. `cargo fmt --all` — formatted
3. Existing tests still pass (especially `full_project_migration_service::tests::*`)

### Functional verification

Run `analyze_full_project_migration` on a real indexed WebForms project and verify the markdown output contains ALL of these sections:

- [ ] Executive Summary (with JS file count, GIS library count, anti-pattern count)
- [ ] Authentication & Authorization
- [ ] Application Lifecycle (Global.asax) — with lifecycle events table and startup registrations
- [ ] Configuration (web.config) — with connection strings, appSettings, handlers, modules, session state
- [ ] State Management (Project-Wide)
- [ ] Data Access Patterns
- [ ] Service Endpoints — with ASMX, ASHX, WCF, HTTP Modules inventory
- [ ] JavaScript & Client-Side Dependencies — with AJAX endpoint table, page↔JS dependency map
- [ ] GIS / Spatial Analysis — with per-library feature inventory and modern targets
- [ ] Design Anti-Patterns — with critical items and migration impact
- [ ] Migration Wave Plan
- [ ] Cross-Cutting Concerns
- [ ] Classic ASP Files (if any .asp files exist)
- [ ] Reports (if any .rdl/.rdlc files exist)
- [ ] Page-by-Page Dossiers — each page now includes JS deps, GIS, anti-patterns
- [ ] Risk Assessment

### Quality bar

An AI agent reading this report should be able to answer ALL of these questions without making any additional tool calls:

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

If any of these questions cannot be answered from the report, the task is not complete.

---

## What This Is NOT

- NOT new extractors — all extraction already works (Phases 13, 16, 24, 25, 30)
- NOT new graph storage — all edges already exist
- NOT new services — existing services already return this data
- NOT new tools — `analyze_full_project_migration` already exists

This is **exclusively** about querying existing graph data and rendering it into the final report. The infrastructure is built. This task connects the wires.
