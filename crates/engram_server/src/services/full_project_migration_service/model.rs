//! Data model for the full-project migration report.
//!
//! This submodule hosts every `pub struct` / `pub enum` that makes up
//! [`FullProjectMigrationReport`] and its building-block types. It was
//! extracted from the former monolithic `full_project_migration_service.rs`
//! purely as a structural split — no behavioural change. Every analyzer,
//! renderer, and LLM-enhancement module in this directory imports these
//! types via `use super::model::*;` (and the parent module re-exports
//! them so external callers keep using the same paths).
//!
//! The top-level entry point [`FullProjectMigrationReport`] is populated
//! by `analyze_full_project` in the parent module. Each sub-struct
//! corresponds to one analyzer function (`build_*`, `extract_*`,
//! `detect_*`). No struct here owns any mutable state — everything is
//! plain data plus `Serialize` for JSON output.

use std::collections::BTreeMap;

use engram_index::solution_parser::PackageRef;
use serde::Serialize;

use super::super::auth_config_service::AuthConfigMap;
use super::super::db_strategy_service::FileDataAccessProfile;
use super::super::dossier_service::MigrationDossier;
use super::super::migration_order_service::MigrationOrderPlan;
use super::super::state_migration_service::StateMigrationReport;

// ── Public types ──────────────────────────────────────────────────────────────

/// Complete migration analysis for an entire project.
#[derive(Debug, Clone, Serialize)]
pub struct FullProjectMigrationReport {
    pub project_id: String,
    pub target_stack: String,
    pub generated_at: String,

    // ── Project-wide analyses ─────────────────────────────────────────────
    pub migration_order: MigrationOrderPlan,
    pub state_migration: StateMigrationReport,
    pub auth_config: AuthConfigMap,
    pub data_access_profiles: Vec<FileDataAccessProfile>,

    // ── Per-file dossiers ─────────────────────────────────────────────────
    pub page_dossiers: Vec<MigrationDossier>,

    // ── Cross-cutting aggregation ─────────────────────────────────────────
    pub cross_cutting: CrossCuttingSummary,

    // ── Phase 32: full-spectrum analyses ───────────────────────────────────
    pub js_analysis: JsAnalysisSummary,
    pub gis_analysis: GisAnalysisSummary,
    pub web_config_inventory: WebConfigInventory,
    pub service_endpoints: ServiceEndpointSummary,
    pub global_asax: GlobalAsaxSummary,
    pub anti_patterns: AntiPatternSummary,
    pub classic_asp: ClassicAspSummary,
    pub reports: ReportSummary,

    // ── Phase 33: 80%→100% analyses ───────────────────────────────────────
    pub method_inventories: BTreeMap<String, PageMethodInventory>,
    pub third_party_controls: ThirdPartyControlSummary,
    pub dependency_inventory: DependencyInventory,
    pub caching_inventory: CachingInventory,
    pub url_routing: UrlRoutingInventory,
    pub vb_translation: VbTranslationReport,
    pub multi_tenancy: MultiTenancyReport,
    pub email_patterns: EmailPatternReport,
    pub background_jobs: BackgroundJobReport,

    // ── Phase 34: deep structural analyses ────────────────────────────────
    pub sp_catalog: StoredProcedureCatalog,
    pub inheritance_chains: InheritanceChainReport,
    pub config_transforms: ConfigTransformReport,
    pub master_page_regions: MasterPageRegionMap,
    pub resource_inventory: ResourceInventory,

    // ── Phase 35: last-mile accuracy ─────────────────────────────────────
    pub vb_translation_traps: engram_index::vb_translation_traps::VbTranslationTrapReport,
    pub jquery_inventory: engram_index::jquery_inventory::JQueryInventory,
    pub cross_layer_traces: CrossLayerTraceSummary,

    // ── Phase 36: business logic comprehension ───────────────────────────
    pub business_logic: super::super::business_logic_service::ProjectBusinessLogicReport,

    // ── Phase 37: intelligence amplification ─────────────────────────────
    pub database_intelligence: super::super::database_intelligence_service::DatabaseIntelligence,
    pub session_workflows: super::super::session_workflow_service::SessionWorkflowReport,

    // ── The single markdown report ────────────────────────────────────────
    pub markdown_report: String,

    // ── MIG1/D2: report completeness surface ─────────────────────────────
    /// Contexts of every graph query that failed and returned empty defaults
    /// rather than live data.  Empty when `report_is_complete` is `true`.
    pub degraded_sections: Vec<String>,
    /// `true` iff every graph query succeeded and no section was substituted
    /// with an empty default.  Consumers should warn operators when `false`.
    pub report_is_complete: bool,
}

/// Aggregated cross-cutting concerns derived from per-file dossiers.
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
    // Phase 32 aggregation
    pub total_script_files: usize,
    /// Backward-compatible alias for external consumers still reading `total_js_files`.
    #[serde(rename = "total_js_files")]
    pub legacy_total_js_files: usize,
    pub total_gis_libraries: usize,
    pub total_anti_patterns: usize,
    pub total_service_endpoints: usize,
    pub total_classic_asp_files: usize,
    pub total_reports: usize,
    // Phase 33 aggregation
    pub total_methods: usize,
    pub total_event_handlers: usize,
    pub total_web_methods: usize,
    pub largest_file_by_methods: Option<(String, usize)>,
    pub total_nuget_packages: usize,
    pub target_framework: String,
    pub total_cached_pages: usize,
    pub total_cache_keys: usize,
    pub has_email: bool,
    pub has_background_jobs: bool,
    // Phase 34 aggregation
    pub total_stored_procedures: usize,
    pub total_sp_called_from_code: usize,
    pub deepest_inheritance_chain: usize,
    pub total_base_classes: usize,
    pub total_config_environments: usize,
    pub total_resource_files: usize,
    pub total_resource_languages: usize,
    pub total_master_page_regions: usize,
    pub total_legacy_packages: usize,
    pub option_strict_on_files: usize,
    pub option_strict_off_files: usize,
    pub dynamic_dispatch_methods: usize,
    pub dynamic_dispatch_risk_tier: String,
}

/// An item (table, state key, control) shared across multiple files.
#[derive(Debug, Clone, Serialize)]
pub struct SharedItem {
    pub name: String,
    pub used_by: Vec<String>,
}

/// Pre-read file content for a single markup file + optional code-behind.
pub struct FileContent {
    pub file_path: String,
    pub markup_content: String,
    pub codebehind_content: Option<String>,
}

/// All pre-read file categories for a project, built by the tool handler.
pub struct ProjectFileBundle {
    pub markup_files: Vec<FileContent>,
    /// Script source files discovered for client-side analysis (.js, .ts, .tsx, .jsx).
    pub script_files: Vec<(String, String)>,
    pub classic_asp_files: Vec<(String, String)>,
    pub report_files: Vec<(String, String)>,
    pub global_asax: Option<FileContent>,
    pub web_config_content: Option<String>,
    pub code_files: Vec<(String, String)>,
    /// Phase 33: parsed .csproj/.vbproj project references.
    pub project_references: Vec<ProjectReferenceBundle>,
    /// Phase 34: .sql files for stored procedure catalog.
    pub sql_files: Vec<(String, String)>,
    /// Phase 34: packages.config files (legacy NuGet format).
    pub packages_config_files: Vec<(String, String)>,
    /// Phase 34: web.*.config transform files (web.Debug.config, etc.).
    pub config_transform_files: Vec<(String, String)>,
    /// Phase 34: .resx resource files.
    pub resx_files: Vec<(String, String)>,
    /// Phase 34: .master files for region mapping.
    pub master_files: Vec<(String, String)>,
}

/// Parsed project file metadata (.csproj/.vbproj).
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

// ── Phase 32: JavaScript / jQuery Analysis ────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct JsAnalysisSummary {
    pub total_script_files: usize,
    /// Backward-compatible alias for external consumers still reading `total_js_files`.
    #[serde(rename = "total_js_files")]
    pub legacy_total_js_files: usize,
    pub script_files_with_server_deps: usize,
    /// Backward-compatible alias for external consumers still reading `js_files_with_server_deps`.
    #[serde(rename = "js_files_with_server_deps")]
    pub legacy_js_files_with_server_deps: usize,
    pub dom_manipulations: Vec<JsDomRef>,
    pub postback_triggers: Vec<JsPostbackRef>,
    pub ajax_calls: Vec<JsAjaxCall>,
    pub page_js_dependencies: BTreeMap<String, Vec<String>>,
    pub inline_script_files: Vec<String>,
    pub jquery_version_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsDomRef {
    pub js_file: String,
    pub target_control: String,
    pub selector_type: String,
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
    pub transport: String,
    pub target_method: Option<String>,
    pub target_type: String,
}

// ── Phase 32: GIS / Spatial Analysis ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct GisAnalysisSummary {
    pub has_gis: bool,
    pub libraries_detected: Vec<GisLibrarySummary>,
    pub total_spatial_calls: usize,
    pub files_with_gis: Vec<String>,
    pub migration_complexity: String,
    pub modern_targets: GisModernTargets,
}

#[derive(Debug, Clone, Serialize)]
pub struct GisLibrarySummary {
    pub library: String,
    pub files: Vec<String>,
    pub class_count: usize,
    pub features: Vec<String>,
    pub has_3d: bool,
    pub has_drawing: bool,
    pub has_geocoding: bool,
    pub has_clustering: bool,
    pub has_wms: bool,
    pub api_keys_detected: usize,
    pub api_style: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GisModernTargets {
    pub react: Vec<String>,
    pub blazor: Vec<String>,
    pub angular: Vec<String>,
}

// ── Phase 32: web.config Inventory ────────────────────────────────────────────

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
    pub value_preview: String,
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionStringEntry {
    pub name: String,
    pub provider: String,
    pub has_integrated_security: bool,
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HandlerRegistration {
    pub verb: String,
    pub path: String,
    pub handler_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModuleRegistration {
    pub name: String,
    pub module_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CustomErrorConfig {
    pub mode: String,
    pub default_redirect: Option<String>,
    pub status_redirects: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompilationConfig {
    pub debug: bool,
    pub target_framework: Option<String>,
    pub assemblies: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStateConfig {
    pub mode: String,
    pub timeout_minutes: Option<u32>,
    pub cookieless: Option<String>,
    pub custom_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PagesConfig {
    pub theme: Option<String>,
    pub master_page_file: Option<String>,
    pub namespaces: Vec<String>,
    pub controls: Vec<String>,
}

// ── Phase 32: Service Endpoint Inventory ──────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ServiceEndpointSummary {
    pub web_services: Vec<ServiceEndpoint>,
    pub http_handlers: Vec<ServiceEndpoint>,
    pub wcf_services: Vec<ServiceEndpoint>,
    pub http_modules: Vec<ServiceEndpoint>,
    pub route_handlers: Vec<ServiceEndpoint>,
    pub total_endpoints: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceEndpoint {
    pub file_path: String,
    pub service_name: String,
    pub methods: Vec<String>,
    pub modern_equivalent: String,
    pub called_by: Vec<String>,
}

// ── Phase 32: Global.asax / Application Lifecycle ─────────────────────────────

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
    pub key_actions: Vec<String>,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartupRegistration {
    pub registration_type: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModernMapping {
    pub legacy: String,
    pub modern: String,
}

// ── Phase 32: Anti-Pattern Summary ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct AntiPatternSummary {
    pub total_anti_patterns: usize,
    pub by_type: BTreeMap<String, usize>,
    pub critical_items: Vec<AntiPatternItem>,
    pub migration_impact: Vec<String>,
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

// ── Phase 32: Classic ASP Summary ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ClassicAspSummary {
    pub total_asp_files: usize,
    pub com_objects: Vec<ComObjectRef>,
    pub ado_connections: usize,
    pub sql_statements: usize,
    pub includes: Vec<IncludeRef>,
    pub state_accesses: usize,
    pub migration_effort_hours: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComObjectRef {
    pub file_path: String,
    pub prog_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IncludeRef {
    pub source_file: String,
    pub included_file: String,
}

// ── Phase 32: Report Summary ──────────────────────────────────────────────────

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
    pub migration_target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrystalReportInfo {
    pub file_path: String,
    pub report_file: String,
    pub is_binary: bool,
    pub modern_equivalent: String,
}

// ── Phase 33: Code-Behind Method Inventory (Gap 1) ────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MethodInfo {
    pub name: String,
    pub signature: String,
    pub return_type: String,
    pub access_level: String,
    pub line_range: (u32, u32),
    pub line_count: u32,
    pub method_kind: MethodKind,
    pub effects: Vec<String>,
    pub calls_methods: Vec<String>,
    pub called_by: Vec<String>,
    /// Method body preview: full body for ≤30 lines, truncated otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_preview: Option<String>,
    /// Heuristic complexity: branches + loops + error handlers + SQL + Session.
    pub complexity_score: u32,
    /// VB Handles clause bindings: e.g. ["btnSave.Click", "MyBase.Load"].
    /// Empty for C# methods (which use += event wiring or aspx runat="server" attributes).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub handles_clause: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub enum MethodKind {
    Lifecycle,
    ControlEvent,
    WebMethod,
    DataAccess,
    Helper,
    Unknown,
}

impl std::fmt::Display for MethodKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lifecycle => write!(f, "Lifecycle"),
            Self::ControlEvent => write!(f, "ControlEvent"),
            Self::WebMethod => write!(f, "WebMethod"),
            Self::DataAccess => write!(f, "DataAccess"),
            Self::Helper => write!(f, "Helper"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

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
    pub largest_method: Option<(String, u32)>,
    pub methods_with_sql: usize,
    pub methods_with_state: usize,
}

// ── Phase 33: Third-Party Control Detection (Gap 2) ───────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ThirdPartyControlSummary {
    pub vendors_detected: Vec<VendorSummary>,
    pub total_third_party_controls: usize,
    pub files_with_third_party: Vec<String>,
    pub unmapped_controls: Vec<UnmappedControl>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VendorSummary {
    pub vendor: String,
    pub suite: String,
    pub control_count: usize,
    pub controls_used: Vec<(String, usize)>,
    pub files: Vec<String>,
    pub modern_replacement_suite: String,
    pub license_note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnmappedControl {
    pub tag_name: String,
    pub vendor: String,
    pub file_path: String,
    pub note: String,
}

// ── Phase 33: Dependency Inventory (Gap 3) ────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct DependencyInventory {
    pub target_frameworks: Vec<String>,
    pub nuget_packages: Vec<NuGetPackageInfo>,
    pub assembly_references: Vec<AssemblyRefInfo>,
    pub project_references: Vec<ProjectRefInfo>,
    pub total_packages: usize,
    pub total_assemblies: usize,
    pub framework_assemblies: Vec<String>,
    pub third_party_assemblies: Vec<String>,
    pub packages_with_known_replacement: usize,
    pub packages_without_replacement: usize,
    /// Packages from legacy packages.config files (pre-SDK-style projects).
    pub legacy_packages: Vec<LegacyPackageRef>,
    /// Assembly binding redirects from web.config.
    pub binding_redirects: Vec<BindingRedirect>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NuGetPackageInfo {
    pub name: String,
    pub version: Option<String>,
    pub modern_replacement: Option<String>,
    pub modern_version: Option<String>,
    pub migration_notes: Option<String>,
    pub category: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyRefInfo {
    pub assembly_name: String,
    pub is_framework: bool,
    pub modern_equivalent: Option<String>,
    pub removal_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRefInfo {
    pub project_name: String,
    pub project_path: String,
    pub target_framework: Option<String>,
}

// ── Phase 33: Caching Inventory (Gap 4) ───────────────────────────────────────

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
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgrammaticCacheEntry {
    pub cache_key: String,
    pub operation: String,
    pub files: Vec<String>,
    pub has_expiration: bool,
    pub has_dependency: bool,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlCacheDependencyEntry {
    pub file_path: String,
    pub database_name: Option<String>,
    pub table_name: Option<String>,
    pub modern_note: String,
}

// ── Phase 33: URL Routing Inventory (Gap 5) ───────────────────────────────────

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
    pub match_pattern: String,
    pub action_type: String,
    pub target_url: String,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageRoute {
    pub route_name: String,
    pub url_pattern: String,
    pub physical_page: String,
    pub modern_equivalent: String,
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

// ── Phase 33: VB.NET → C# Translation Flags (Gap 6) ──────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct VbTranslationReport {
    pub is_vb_project: bool,
    pub vb_file_count: usize,
    pub cs_file_count: usize,
    pub mixed_language: bool,
    pub translation_flags: Vec<VbTranslationFlag>,
    pub total_flags: usize,
    pub flags_by_category: BTreeMap<String, usize>,
    pub highest_risk_files: Vec<(String, usize)>,
    pub dynamic_dispatch: DynamicDispatchSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct DynamicDispatchSummary {
    pub option_strict_on_files: usize,
    pub option_strict_off_files: usize,
    pub methods_with_dynamic_dispatch: usize,
    pub late_binding_call_count: usize,
    pub object_var_count: usize,
    pub callbyname_count: usize,
    pub dynamic_dispatch_risk_tier: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VbTranslationFlag {
    pub category: String,
    pub pattern: String,
    pub file_path: String,
    pub count: usize,
    pub csharp_equivalent: String,
    pub risk_level: String,
    pub auto_translatable: bool,
    pub notes: String,
}

// ── Phase 33: Multi-Tenancy Detection (Gap 7) ─────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MultiTenancyReport {
    pub is_multi_tenant: bool,
    pub confidence: String,
    pub tenant_id_column_name: Option<String>,
    pub isolation_strategy: Option<String>,
    pub detection_evidence: Vec<TenancyEvidence>,
    pub tenant_resolution: Option<TenantResolution>,
    pub tenant_filtered_queries: usize,
    pub files_with_tenant_logic: Vec<String>,
    pub migration_recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TenancyEvidence {
    pub evidence_type: String,
    pub file_path: String,
    pub detail: String,
    pub line_hint: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TenantResolution {
    pub mechanism: String,
    pub module_class: Option<String>,
    pub file_path: String,
}

// ── Phase 33: Email / Notification Patterns (Gap 8) ───────────────────────────

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
    pub pattern_type: String,
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

// ── Phase 33: Background Job Patterns (Gap 8) ─────────────────────────────────

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
    pub pattern_type: String,
    pub count: usize,
    pub modern_equivalent: String,
    pub risk_level: String,
}

// ── Phase 34: Stored Procedure Catalog (Ticket 1) ─────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct StoredProcedureCatalog {
    pub procedures: Vec<StoredProcedureInfo>,
    pub total_procedures: usize,
    pub procedures_with_params: usize,
    pub procedures_called_from_code: usize,
    pub uncalled_procedures: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredProcedureInfo {
    pub name: String,
    pub parameters: Vec<SpParameterInfo>,
    pub tables_read: Vec<String>,
    pub tables_written: Vec<String>,
    pub called_from: Vec<String>,
    pub line_count: usize,
    pub has_dynamic_sql: bool,
    pub has_cursor: bool,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpParameterInfo {
    pub name: String,
    pub sql_type: String,
    pub direction: String,
    pub default_value: Option<String>,
    pub csharp_type: String,
}

// ── Phase 34: Base Class Inheritance Chain (Ticket 2) ─────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct InheritanceChainReport {
    pub chains: Vec<InheritanceChain>,
    pub base_classes: Vec<BaseClassInfo>,
    pub shared_lifecycle_methods: Vec<SharedLifecycleMethod>,
    pub inherited_effects: Vec<InheritedEffect>,
    pub deepest_chain_depth: usize,
}

/// An effect inherited from a base class method.
#[derive(Debug, Clone, Serialize)]
pub struct InheritedEffect {
    pub class: String,
    pub inherited_from: String,
    pub method: String,
    pub effects: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InheritanceChain {
    pub page_file: String,
    pub chain: Vec<String>,
    pub inherited_lifecycle_methods: Vec<(String, String)>,
    pub inherited_state_writes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BaseClassInfo {
    pub class_name: String,
    pub file_path: String,
    pub derived_count: usize,
    pub lifecycle_methods: Vec<String>,
    pub state_keys_initialized: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SharedLifecycleMethod {
    pub method_name: String,
    pub defining_class: String,
    pub overridden_in: Vec<String>,
    pub calls_base: bool,
}

// ── Phase 35: Cross-Layer AJAX→Handler→Data Tracing ──────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CrossLayerTraceSummary {
    pub chains: Vec<DataFlowChain>,
    pub total_chains: usize,
    pub unresolved_urls: Vec<String>,
    pub handlers_without_ajax_callers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataFlowChain {
    pub feature_name: String,
    pub trigger_file: String,
    pub steps: Vec<DataFlowStep>,
    pub tables_touched: Vec<String>,
    pub risk_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataFlowStep {
    pub layer: String,
    pub file_path: String,
    pub action: String,
    pub params: Vec<String>,
}

// ── Phase 35: VB Translation Traps (from engram_index) ───────────────────────
// Re-exported from engram_index::vb_translation_traps

// ── Phase 35: jQuery Inventory (from engram_index) ───────────────────────────
// Re-exported from engram_index::jquery_inventory

// ── Phase 34: Binding Redirects (Ticket 3) ────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LegacyPackageRef {
    pub package_id: String,
    pub version: String,
    pub target_framework: String,
    pub is_dev_dependency: bool,
    pub modern_replacement: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BindingRedirect {
    pub assembly_name: String,
    pub old_version_range: String,
    pub new_version: String,
    pub public_key_token: Option<String>,
    pub has_known_replacement: bool,
}

// ── Phase 34: Config Transforms (Ticket 6a) ──────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ConfigTransformReport {
    pub environments: Vec<ConfigEnvironment>,
    pub total_transforms: usize,
    pub connection_string_overrides: Vec<(String, String)>,
    pub debug_flag_overrides: Vec<(String, bool)>,
    pub app_setting_overrides: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigEnvironment {
    pub name: String,
    pub file_path: String,
    pub transforms: Vec<ConfigTransform>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigTransform {
    pub xpath_hint: String,
    pub operation: String,
    pub key: Option<String>,
    pub value_preview: Option<String>,
}

// ── Phase 34: Master Page Region Mapping (Ticket 6b) ──────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MasterPageRegionMap {
    pub master_pages: Vec<MasterPageInfo>,
    pub regions: Vec<RegionMapping>,
    pub orphan_regions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MasterPageInfo {
    pub file_path: String,
    pub placeholders: Vec<String>,
    pub nested_master: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegionMapping {
    pub region_name: String,
    pub defined_in: String,
    pub filled_by: Vec<String>,
    pub has_default_content: bool,
    pub modern_equivalent: String,
}

// ── Phase 34: Resource File Inventory (Ticket 6c) ─────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ResourceInventory {
    pub resource_files: Vec<ResourceFileInfo>,
    pub total_keys: usize,
    pub languages_detected: Vec<String>,
    pub has_global_resources: bool,
    pub has_local_resources: bool,
    pub embedded_resource_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceFileInfo {
    pub file_path: String,
    pub key_count: usize,
    pub language: Option<String>,
    pub resource_type: String,
}
