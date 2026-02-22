//! Full project migration analysis — the "one call, everything" service.
//!
//! Orchestrates every migration sub-service to produce a single comprehensive
//! report covering every file in the project.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_graph::{EdgeKind, GraphStore};
use serde::Serialize;

use super::auth_config_service::AuthConfigMap;
use super::db_strategy_service::{self, FileDataAccessProfile};
use super::dossier_service::{self, MigrationDossier};
use super::migration_order_service::{self, MigrationOrderPlan};
use super::pattern_detection_service;
use super::state_migration_service::{self, StateMigrationReport};

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

    // ── The single markdown report ────────────────────────────────────────
    pub markdown_report: String,
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
    pub total_js_files: usize,
    pub total_gis_libraries: usize,
    pub total_anti_patterns: usize,
    pub total_service_endpoints: usize,
    pub total_classic_asp_files: usize,
    pub total_reports: usize,
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
    pub js_files: Vec<(String, String)>,
    pub classic_asp_files: Vec<(String, String)>,
    pub report_files: Vec<(String, String)>,
    pub global_asax: Option<FileContent>,
    pub web_config_content: Option<String>,
    pub code_files: Vec<(String, String)>,
}

// ── Phase 32: JavaScript / jQuery Analysis ────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct JsAnalysisSummary {
    pub total_js_files: usize,
    pub js_files_with_server_deps: usize,
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

// ── Main entry point ──────────────────────────────────────────────────────────

/// Analyze an entire project for migration.
///
/// All file content must be pre-read (async) and passed in via [`ProjectFileBundle`].
/// Every sub-service call inside is synchronous and safe for `spawn_blocking`.
pub fn analyze_full_project(
    graph: &Arc<GraphStore>,
    project_id: &str,
    target_stack: &str,
    bundle: &ProjectFileBundle,
    max_files: usize,
) -> anyhow::Result<FullProjectMigrationReport> {
    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let days = secs / 86400;
        let time_secs = secs % 86400;
        let h = time_secs / 3600;
        let m = (time_secs % 3600) / 60;
        let s = time_secs % 60;
        let (y, mo, d) = epoch_days_to_date(days);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
    };

    let web_config_content = bundle.web_config_content.as_deref();
    let code_refs: Vec<(&str, &str)> = bundle
        .code_files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();

    // ── 1. Project-wide analyses (graph-only, no file I/O) ────────────────

    let migration_order = migration_order_service::suggest_migration_order(graph, project_id)
        .unwrap_or_else(|e| {
            tracing::warn!("migration_order failed: {e}");
            MigrationOrderPlan {
                project_id: project_id.to_string(),
                total_files: 0,
                waves: vec![],
                circular_dependencies: vec![],
                bottleneck_files: vec![],
                summary: format!("(migration order unavailable: {e})"),
            }
        });

    let state_migration = state_migration_service::analyze_state_migration(graph, project_id)
        .unwrap_or_else(|e| {
            tracing::warn!("state_migration failed: {e}");
            StateMigrationReport {
                project_id: project_id.to_string(),
                recommendations: vec![],
                viewstate_report: None,
                summary: state_migration_service::StateMigrationSummary {
                    total_state_keys: 0,
                    by_store: BTreeMap::new(),
                    by_target: BTreeMap::new(),
                    high_risk_keys: vec![],
                },
            }
        });

    let auth_config = super::auth_config_service::analyze_auth_config(
        graph,
        project_id,
        web_config_content,
        &code_refs,
    )
    .unwrap_or_else(|e| {
        tracing::warn!("auth_config failed: {e}");
        AuthConfigMap {
            project_id: project_id.to_string(),
            file_scope: None,
            auth_mode: "unknown".to_string(),
            forms_auth: None,
            windows_auth: None,
            location_rules: vec![],
            membership_config: None,
            role_provider: None,
            code_auth_checks: vec![],
            session_auth_patterns: vec![],
            recommendations: vec![],
            migration_complexity: "Unknown".to_string(),
        }
    });

    let data_access_profiles =
        db_strategy_service::classify_data_access_patterns(graph, project_id).unwrap_or_else(|e| {
            tracing::warn!("data_access classification failed: {e}");
            vec![]
        });

    // ── 2. Per-file dossiers ──────────────────────────────────────────────

    let file_contents = &bundle.markup_files;
    let capped = if file_contents.len() > max_files {
        &file_contents[..max_files]
    } else {
        file_contents
    };

    let mut page_dossiers: Vec<MigrationDossier> = Vec::with_capacity(capped.len());

    for fc in capped {
        match dossier_service::build_migration_dossier(
            graph,
            project_id,
            &fc.file_path,
            &fc.markup_content,
            fc.codebehind_content.as_deref().unwrap_or(""),
            web_config_content,
            target_stack,
        ) {
            Ok(dossier) => page_dossiers.push(dossier),
            Err(e) => {
                tracing::warn!(file = %fc.file_path, "dossier failed: {e}");
            }
        }
    }

    // ── 3. Phase 32 analyses ─────────────────────────────────────────────

    let web_config_inv = web_config_content
        .map(|wc| extract_webconfig_inventory(wc, &code_refs))
        .unwrap_or_else(|| WebConfigInventory {
            app_settings: vec![],
            connection_strings: vec![],
            http_handlers: vec![],
            http_modules: vec![],
            custom_errors: None,
            compilation: None,
            session_state: None,
            pages_config: None,
        });

    let global_asax = bundle
        .global_asax
        .as_ref()
        .map(|ga| {
            extract_global_asax_info(
                &ga.markup_content,
                ga.codebehind_content.as_deref().unwrap_or(""),
            )
        })
        .unwrap_or_else(|| GlobalAsaxSummary {
            has_global_asax: false,
            codebehind_class: None,
            lifecycle_events: vec![],
            startup_registrations: vec![],
            modern_mapping: vec![],
        });

    let service_endpoints = build_service_endpoint_summary(graph, project_id);

    let anti_patterns = build_anti_pattern_summary(graph, project_id);

    let js_analysis = build_js_analysis(graph, project_id, &bundle.markup_files, &bundle.js_files);

    let gis_analysis = build_gis_analysis(graph, project_id, target_stack);

    let classic_asp = build_classic_asp_summary(graph, project_id, &bundle.classic_asp_files);

    let reports = build_report_summary(graph, project_id, &bundle.report_files);

    // ── 4. Cross-cutting aggregation ──────────────────────────────────────

    let cross_cutting = build_cross_cutting_summary(
        &page_dossiers,
        &state_migration,
        &js_analysis,
        &gis_analysis,
        &anti_patterns,
        &service_endpoints,
        &classic_asp,
        &reports,
    );

    // ── 5. Build the wave lookup (file_path → wave number) ────────────────

    let mut wave_lookup: BTreeMap<String, u32> = BTreeMap::new();
    for wave in &migration_order.waves {
        for wf in &wave.files {
            wave_lookup.insert(wf.path.clone(), wave.wave_number);
        }
    }

    // ── 6. Render markdown ────────────────────────────────────────────────

    let markdown_report = render_markdown(
        project_id,
        target_stack,
        &now,
        &migration_order,
        &state_migration,
        &auth_config,
        &data_access_profiles,
        &page_dossiers,
        &cross_cutting,
        &wave_lookup,
        &js_analysis,
        &gis_analysis,
        &web_config_inv,
        &service_endpoints,
        &global_asax,
        &anti_patterns,
        &classic_asp,
        &reports,
    );

    Ok(FullProjectMigrationReport {
        project_id: project_id.to_string(),
        target_stack: target_stack.to_string(),
        generated_at: now,
        migration_order,
        state_migration,
        auth_config,
        data_access_profiles,
        page_dossiers,
        cross_cutting,
        js_analysis,
        gis_analysis,
        web_config_inventory: web_config_inv,
        service_endpoints,
        global_asax,
        anti_patterns,
        classic_asp,
        reports,
        markdown_report,
    })
}

// ── Cross-cutting aggregation ─────────────────────────────────────────────────

fn build_cross_cutting_summary(
    dossiers: &[MigrationDossier],
    state_report: &StateMigrationReport,
    js: &JsAnalysisSummary,
    gis: &GisAnalysisSummary,
    ap: &AntiPatternSummary,
    se: &ServiceEndpointSummary,
    asp: &ClassicAspSummary,
    rpt: &ReportSummary,
) -> CrossCuttingSummary {
    let mut complexity_distribution: BTreeMap<String, usize> = BTreeMap::new();
    let mut risk_distribution: BTreeMap<String, usize> = BTreeMap::new();
    let mut sql_table_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut control_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut critical_risk_files = Vec::new();
    let mut total_validators = 0usize;
    let mut total_update_panels = 0usize;
    let mut total_lifecycle_events = 0usize;
    let mut files_with_ispostback = 0usize;

    for d in dossiers {
        // Complexity distribution
        *complexity_distribution
            .entry(d.estimated_complexity.clone())
            .or_insert(0) += 1;

        // Risk distribution
        let risk_band = match d.blast_radius_score {
            0..=3 => "Low",
            4..=6 => "Medium",
            7..=8 => "High",
            _ => "Critical",
        };
        *risk_distribution.entry(risk_band.to_string()).or_insert(0) += 1;

        if d.blast_radius_score >= 9 {
            critical_risk_files.push(d.file_path.clone());
        }

        // Shared SQL tables
        for table in &d.tables_touched {
            sql_table_map
                .entry(table.clone())
                .or_default()
                .push(d.file_path.clone());
        }

        // Shared user controls
        for uc in &d.user_controls {
            control_map
                .entry(uc.control_path.clone())
                .or_default()
                .push(d.file_path.clone());
        }

        // Validators
        total_validators +=
            d.validation_summary.validator_count + d.validation_summary.custom_validator_count;

        // UpdatePanels
        total_update_panels += d.ajax_summary.update_panel_count;

        // Lifecycle events
        total_lifecycle_events +=
            d.lifecycle_summary.lifecycle_event_count + d.lifecycle_summary.control_event_count;

        if d.lifecycle_summary.has_ispostback_logic {
            files_with_ispostback += 1;
        }
    }

    // Shared state keys from project-wide state_migration report
    let mut state_key_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for rec in &state_report.recommendations {
        let mut all_files: Vec<String> = rec.readers.clone();
        all_files.extend(rec.writers.iter().cloned());
        all_files.sort();
        all_files.dedup();
        if !all_files.is_empty() {
            state_key_map.insert(rec.state_key.clone(), all_files);
        }
    }

    // Filter to only items shared by 2+ files
    let shared_sql_tables = sql_table_map
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|(name, mut used_by)| {
            used_by.sort();
            used_by.dedup();
            SharedItem { name, used_by }
        })
        .collect();

    let shared_state_keys = state_key_map
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|(name, used_by)| SharedItem { name, used_by })
        .collect();

    let shared_user_controls = control_map
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|(name, mut used_by)| {
            used_by.sort();
            used_by.dedup();
            SharedItem { name, used_by }
        })
        .collect();

    CrossCuttingSummary {
        total_pages_analyzed: dossiers.len(),
        complexity_distribution,
        shared_sql_tables,
        shared_state_keys,
        shared_user_controls,
        risk_distribution,
        critical_risk_files,
        total_validators,
        total_update_panels,
        total_lifecycle_events,
        files_with_ispostback,
        total_js_files: js.total_js_files,
        total_gis_libraries: gis.libraries_detected.len(),
        total_anti_patterns: ap.total_anti_patterns,
        total_service_endpoints: se.total_endpoints,
        total_classic_asp_files: asp.total_asp_files,
        total_reports: rpt.total_reports,
    }
}

// ── Phase 32: Analysis functions ──────────────────────────────────────────────

/// Extract web.config inventory: appSettings, connectionStrings, handlers,
/// modules, customErrors, compilation, sessionState, pages.
fn extract_webconfig_inventory(
    web_config: &str,
    code_files: &[(&str, &str)],
) -> WebConfigInventory {
    use regex::Regex;

    // ── appSettings ──
    let add_re = Regex::new(r#"<add\s+key\s*=\s*"([^"]+)"\s+value\s*=\s*"([^"]*)""#).unwrap();
    let appsettings_section = extract_xml_section(web_config, "appSettings");
    let mut app_settings: Vec<AppSettingEntry> = Vec::new();
    for cap in add_re.captures_iter(&appsettings_section) {
        let key = cap[1].to_string();
        let raw_value = &cap[2];
        let value_preview = mask_sensitive_value(&key, raw_value);
        let used_by = find_config_references(&key, "AppSettings", code_files);
        app_settings.push(AppSettingEntry {
            key,
            value_preview,
            used_by,
        });
    }

    // ── connectionStrings ──
    let conn_re = Regex::new(
        r#"<add\s+name\s*=\s*"([^"]+)"[^>]*connectionString\s*=\s*"([^"]*)"[^>]*(?:providerName\s*=\s*"([^"]*)")?"#
    ).unwrap();
    let conn_section = extract_xml_section(web_config, "connectionStrings");
    let mut connection_strings: Vec<ConnectionStringEntry> = Vec::new();
    for cap in conn_re.captures_iter(&conn_section) {
        let name = cap[1].to_string();
        let cs_value = &cap[2];
        let provider = cap
            .get(3)
            .map_or_else(|| infer_provider(cs_value), |m| m.as_str().to_string());
        let has_integrated_security = cs_value.to_lowercase().contains("integrated security=true")
            || cs_value.to_lowercase().contains("trusted_connection=true");
        let used_by = find_config_references(&name, "ConnectionStrings", code_files);
        connection_strings.push(ConnectionStringEntry {
            name,
            provider,
            has_integrated_security,
            used_by,
        });
    }

    // ── httpHandlers / system.webServer handlers ──
    let handler_re = Regex::new(
        r#"<add\s+(?:[^>]*?)verb\s*=\s*"([^"]*)"[^>]*path\s*=\s*"([^"]*)"[^>]*type\s*=\s*"([^"]*)""#
    ).unwrap();
    let handler_section = extract_xml_section(web_config, "httpHandlers")
        + &extract_xml_section(web_config, "handlers");
    let http_handlers: Vec<HandlerRegistration> = handler_re
        .captures_iter(&handler_section)
        .map(|cap| HandlerRegistration {
            verb: cap[1].to_string(),
            path: cap[2].to_string(),
            handler_type: cap[3].to_string(),
        })
        .collect();

    // ── httpModules / system.webServer modules ──
    let module_re = Regex::new(r#"<add\s+name\s*=\s*"([^"]+)"[^>]*type\s*=\s*"([^"]*)""#).unwrap();
    let module_section = extract_xml_section(web_config, "httpModules")
        + &extract_xml_section(web_config, "modules");
    let http_modules: Vec<ModuleRegistration> = module_re
        .captures_iter(&module_section)
        .map(|cap| ModuleRegistration {
            name: cap[1].to_string(),
            module_type: cap[2].to_string(),
        })
        .collect();

    // ── customErrors ──
    let custom_errors = {
        let ce_re = Regex::new(
            r#"<customErrors\s+mode\s*=\s*"([^"]+)"(?:[^>]*defaultRedirect\s*=\s*"([^"]*)")?"#,
        )
        .unwrap();
        let error_re =
            Regex::new(r#"<error\s+statusCode\s*=\s*"([^"]+)"[^>]*redirect\s*=\s*"([^"]*)""#)
                .unwrap();
        let ce_section = extract_xml_section(web_config, "customErrors");
        ce_re.captures(&ce_section).map(|cap| {
            let redirects: Vec<(String, String)> = error_re
                .captures_iter(&ce_section)
                .map(|ec| (ec[1].to_string(), ec[2].to_string()))
                .collect();
            CustomErrorConfig {
                mode: cap[1].to_string(),
                default_redirect: cap.get(2).map(|m| m.as_str().to_string()),
                status_redirects: redirects,
            }
        })
    };

    // ── compilation ──
    let compilation = {
        let comp_re = Regex::new(r#"<compilation\s+([^>]*?)/?>"#).unwrap();
        comp_re.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            let debug = attrs.contains(r#"debug="true""#);
            let tf_re = Regex::new(r#"targetFramework\s*=\s*"([^"]+)""#).unwrap();
            let target_framework = tf_re.captures(attrs).map(|c| c[1].to_string());
            let asm_re = Regex::new(r#"<add\s+assembly\s*=\s*"([^"]+)""#).unwrap();
            let comp_section = extract_xml_section(web_config, "compilation");
            let assemblies: Vec<String> = asm_re
                .captures_iter(&comp_section)
                .map(|c| c[1].to_string())
                .collect();
            CompilationConfig {
                debug,
                target_framework,
                assemblies,
            }
        })
    };

    // ── sessionState ──
    let session_state = {
        let ss_re = Regex::new(r#"<sessionState\s+([^>]*?)/?>"#).unwrap();
        ss_re.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            let mode_re = Regex::new(r#"mode\s*=\s*"([^"]+)""#).unwrap();
            let timeout_re = Regex::new(r#"timeout\s*=\s*"(\d+)""#).unwrap();
            let cookieless_re = Regex::new(r#"cookieless\s*=\s*"([^"]+)""#).unwrap();
            let provider_re = Regex::new(r#"customProvider\s*=\s*"([^"]+)""#).unwrap();
            SessionStateConfig {
                mode: mode_re
                    .captures(attrs)
                    .map_or("InProc".into(), |c| c[1].to_string()),
                timeout_minutes: timeout_re.captures(attrs).and_then(|c| c[1].parse().ok()),
                cookieless: cookieless_re.captures(attrs).map(|c| c[1].to_string()),
                custom_provider: provider_re.captures(attrs).map(|c| c[1].to_string()),
            }
        })
    };

    // ── pages ──
    let pages_config = {
        let pages_re = Regex::new(r#"<pages\s+([^>]*?)/?>"#).unwrap();
        pages_re.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            let theme_re = Regex::new(r#"theme\s*=\s*"([^"]+)""#).unwrap();
            let mp_re = Regex::new(r#"masterPageFile\s*=\s*"([^"]+)""#).unwrap();
            let ns_re = Regex::new(r#"<add\s+namespace\s*=\s*"([^"]+)""#).unwrap();
            let ctrl_re =
                Regex::new(r#"<add\s+tagPrefix\s*=\s*"([^"]+)"[^>]*namespace\s*=\s*"([^"]+)""#)
                    .unwrap();
            let pages_section = extract_xml_section(web_config, "pages");
            PagesConfig {
                theme: theme_re.captures(attrs).map(|c| c[1].to_string()),
                master_page_file: mp_re.captures(attrs).map(|c| c[1].to_string()),
                namespaces: ns_re
                    .captures_iter(&pages_section)
                    .map(|c| c[1].to_string())
                    .collect(),
                controls: ctrl_re
                    .captures_iter(&pages_section)
                    .map(|c| format!("{}:{}", &c[1], &c[2]))
                    .collect(),
            }
        })
    };

    WebConfigInventory {
        app_settings,
        connection_strings,
        http_handlers,
        http_modules,
        custom_errors,
        compilation,
        session_state,
        pages_config,
    }
}

/// Helper: extract a named XML section (tag body, non-greedy).
fn extract_xml_section(xml: &str, tag: &str) -> String {
    let pattern = format!(r"(?si)<{tag}[^>]*>(.*?)</{tag}>");
    regex::Regex::new(&pattern)
        .ok()
        .and_then(|re| re.captures(xml))
        .map(|c| c[1].to_string())
        .unwrap_or_default()
}

/// Mask potentially sensitive config values (API keys, passwords, etc.)
fn mask_sensitive_value(key: &str, value: &str) -> String {
    let k = key.to_lowercase();
    let sensitive = k.contains("key")
        || k.contains("secret")
        || k.contains("password")
        || k.contains("token")
        || k.contains("apikey")
        || k.contains("connectionstring");
    if sensitive && value.len() > 6 {
        format!("{}...", &value[..6])
    } else if value.len() > 30 {
        format!("{}...", &value[..30])
    } else {
        value.to_string()
    }
}

/// Infer ADO.NET provider from connection string content.
fn infer_provider(cs: &str) -> String {
    let lower = cs.to_lowercase();
    if lower.contains("sqloledb") || lower.contains("data source=") {
        "System.Data.SqlClient".into()
    } else if lower.contains("mysql") {
        "MySql.Data.MySqlClient".into()
    } else if lower.contains("npgsql") {
        "Npgsql".into()
    } else if lower.contains("oracle") {
        "System.Data.OracleClient".into()
    } else {
        "System.Data.SqlClient".into()
    }
}

/// Find code files that reference a config key via ConfigurationManager.
fn find_config_references(key: &str, section: &str, code_files: &[(&str, &str)]) -> Vec<String> {
    let patterns = [
        format!(r#"ConfigurationManager.{section}["{key}"]"#),
        format!(r#"WebConfigurationManager.{section}["{key}"]"#),
        format!(r#"{section}["{key}"]"#),
    ];
    let mut found = Vec::new();
    for &(path, content) in code_files {
        if patterns.iter().any(|p| content.contains(p.as_str())) {
            found.push(path.to_string());
        }
    }
    found
}

// ── Global.asax analysis ──────────────────────────────────────────────────────

fn extract_global_asax_info(markup_content: &str, codebehind_content: &str) -> GlobalAsaxSummary {
    use regex::Regex;

    let combined = if codebehind_content.is_empty() {
        markup_content.to_string()
    } else {
        codebehind_content.to_string()
    };

    if combined.trim().is_empty() {
        return GlobalAsaxSummary {
            has_global_asax: false,
            codebehind_class: None,
            lifecycle_events: vec![],
            startup_registrations: vec![],
            modern_mapping: vec![],
        };
    }

    // Extract class name
    let class_re = Regex::new(r#"(?i)(?:Class|Inherits\s*=\s*["'])(\S+?)(?:["']|\s)"#).unwrap();
    let codebehind_class = class_re.captures(&combined).map(|c| c[1].to_string());

    // Event methods to look for
    let event_names = [
        ("Application_Start", "Program.cs builder setup + app.Run()"),
        (
            "Application_OnStart",
            "Program.cs builder setup + app.Run()",
        ),
        (
            "Application_End",
            "IHostApplicationLifetime.ApplicationStopping",
        ),
        (
            "Application_Error",
            "app.UseExceptionHandler() + ProblemDetails",
        ),
        (
            "Application_OnError",
            "app.UseExceptionHandler() + ProblemDetails",
        ),
        ("Session_Start", "Middleware + ISession configuration"),
        ("Session_OnStart", "Middleware + ISession configuration"),
        ("Session_End", "IHostedService background task"),
        ("Session_OnEnd", "IHostedService background task"),
        ("Application_BeginRequest", "app.Use() middleware"),
        (
            "Application_EndRequest",
            "app.Use() middleware (response phase)",
        ),
        ("Application_AuthenticateRequest", "app.UseAuthentication()"),
        (
            "Application_PostAuthenticateRequest",
            "Custom auth middleware after UseAuthentication",
        ),
        ("Application_AuthorizeRequest", "app.UseAuthorization()"),
        (
            "Application_AcquireRequestState",
            "ISession / IDistributedCache middleware",
        ),
    ];

    let mut lifecycle_events = Vec::new();
    for (event_name, modern_equiv) in &event_names {
        let pattern = format!(
            r"(?si)(Sub|void|Handles)\s+{}\b(.*?)(?:End\s+Sub|(?=\b(Sub|void|Protected|Private|Public)\b)|\z)",
            regex::escape(event_name)
        );
        if let Ok(re) = Regex::new(&pattern) {
            if let Some(cap) = re.captures(&combined) {
                let body = cap.get(2).map_or("", |m| m.as_str());
                let line_count = body.lines().count();
                let key_actions = extract_key_actions(body);
                lifecycle_events.push(GlobalLifecycleEvent {
                    event_name: event_name.to_string(),
                    line_count,
                    key_actions,
                    modern_equivalent: modern_equiv.to_string(),
                });
            }
        }
    }

    // Detect startup registrations
    let mut startup_registrations = Vec::new();
    let reg_patterns: &[(&str, &str, &str)] = &[
        (
            r"RouteConfig\.RegisterRoutes|RouteTable\.Routes",
            "routing",
            "app.MapControllerRoute / app.MapBlazorHub",
        ),
        (
            r"BundleConfig\.RegisterBundles|BundleTable\.Bundles",
            "bundling",
            "Vite/Webpack or ASP.NET Core bundling",
        ),
        (
            r"AreaRegistration\.RegisterAllAreas",
            "areas",
            "app.MapAreaControllerRoute",
        ),
        (
            r"GlobalConfiguration\.Configure|WebApiConfig\.Register",
            "webapi",
            "app.MapControllers / Minimal API",
        ),
        (
            r"Container\.Register|kernel\.Bind|builder\.Register|UnityConfig|Ninject|Autofac",
            "di",
            "builder.Services (built-in DI)",
        ),
        (
            r"GlobalFilters\.Filters\.Add",
            "filters",
            "app.AddControllersWithViews + filter options",
        ),
        (
            r"log4net|NLog|Serilog",
            "logging",
            "builder.Logging / Serilog integration",
        ),
    ];
    for &(pattern, reg_type, detail) in reg_patterns {
        if let Ok(re) = Regex::new(pattern) {
            if re.is_match(&combined) {
                startup_registrations.push(StartupRegistration {
                    registration_type: reg_type.to_string(),
                    detail: detail.to_string(),
                });
            }
        }
    }

    // Build modern mapping
    let mut modern_mapping = Vec::new();
    if !lifecycle_events.is_empty() {
        modern_mapping.push(ModernMapping {
            legacy: "Application_Start".into(),
            modern: "Program.cs service registration + middleware pipeline".into(),
        });
        modern_mapping.push(ModernMapping {
            legacy: "Session_Start / Session_End".into(),
            modern: "ISession middleware or custom middleware".into(),
        });
        modern_mapping.push(ModernMapping {
            legacy: "Application_Error".into(),
            modern: "UseExceptionHandler + ProblemDetails".into(),
        });
        modern_mapping.push(ModernMapping {
            legacy: "Application_BeginRequest / EndRequest".into(),
            modern: "Custom middleware pipeline".into(),
        });
    }

    GlobalAsaxSummary {
        has_global_asax: true,
        codebehind_class,
        lifecycle_events,
        startup_registrations,
        modern_mapping,
    }
}

/// Extract key actions from a method body (routing, bundling, DI, state init, etc.)
fn extract_key_actions(body: &str) -> Vec<String> {
    let mut actions = Vec::new();
    let checks: &[(&str, &str)] = &[
        ("RouteConfig", "RouteConfig registration"),
        ("BundleConfig", "BundleConfig registration"),
        ("AreaRegistration", "Area registration"),
        ("GlobalConfiguration", "Web API configuration"),
        ("Container.Register", "DI container registration"),
        ("kernel.Bind", "DI (Ninject) binding"),
        ("builder.Register", "DI (Autofac) registration"),
        ("Application(", "Application state initialization"),
        ("Session(", "Session state initialization"),
        ("Response.Redirect", "Redirect on error/event"),
        ("Server.Transfer", "Server.Transfer"),
        ("log4net", "Logging (log4net)"),
        ("NLog", "Logging (NLog)"),
        ("Serilog", "Logging (Serilog)"),
        ("Exception", "Exception handling"),
        ("HttpContext.Current", "HttpContext usage"),
    ];
    for &(pattern, action) in checks {
        if body.contains(pattern) {
            actions.push(action.to_string());
        }
    }
    actions
}

// ── Service endpoint summary ──────────────────────────────────────────────────

fn build_service_endpoint_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> ServiceEndpointSummary {
    let ws = graph
        .list_edges_by_kind(project_id, EdgeKind::ExposesWebService, 1_000)
        .unwrap_or_default();
    let hh = graph
        .list_edges_by_kind(project_id, EdgeKind::ExposesHttpHandler, 1_000)
        .unwrap_or_default();
    let wcf = graph
        .list_edges_by_kind(project_id, EdgeKind::ExposesWcfService, 1_000)
        .unwrap_or_default();
    let mods = graph
        .list_edges_by_kind(project_id, EdgeKind::RegistersModule, 1_000)
        .unwrap_or_default();
    let routes = graph
        .list_edges_by_kind(project_id, EdgeKind::RegistersHandler, 1_000)
        .unwrap_or_default();

    // Get ApiCall edges to cross-reference callers
    let api_calls = graph
        .list_edges_by_kind(project_id, EdgeKind::ApiCall, 10_000)
        .unwrap_or_default();

    let build_endpoints = |edges: &[engram_graph::Edge], modern: &str| -> Vec<ServiceEndpoint> {
        let mut map: BTreeMap<String, ServiceEndpoint> = BTreeMap::new();
        for e in edges {
            let file_path = extract_file_from_node_id(&e.source_id);
            let entry = map
                .entry(file_path.clone())
                .or_insert_with(|| ServiceEndpoint {
                    file_path: file_path.clone(),
                    service_name: e.target_id.clone(),
                    methods: vec![],
                    modern_equivalent: modern.to_string(),
                    called_by: vec![],
                });
            // Extract method name from metadata if available
            if let Some(ref meta) = e.metadata {
                if let Some(method) = meta.get("method_name").and_then(|v| v.as_str()) {
                    if !entry.methods.contains(&method.to_string()) {
                        entry.methods.push(method.to_string());
                    }
                }
            }
        }
        // Cross-reference with ApiCall edges
        for ep in map.values_mut() {
            for ac in &api_calls {
                let target_file = extract_file_from_node_id(&ac.target_id);
                if target_file == ep.file_path || ac.target_id.contains(&ep.service_name) {
                    let caller = extract_file_from_node_id(&ac.source_id);
                    if !ep.called_by.contains(&caller) {
                        ep.called_by.push(caller);
                    }
                }
            }
        }
        map.into_values().collect()
    };

    let web_services = build_endpoints(&ws, "Minimal API / Web API controller");
    let http_handlers = build_endpoints(&hh, "Minimal API endpoint / Middleware");
    let wcf_services = build_endpoints(&wcf, "gRPC service or Web API controller");
    let http_modules: Vec<ServiceEndpoint> = mods
        .iter()
        .map(|e| ServiceEndpoint {
            file_path: extract_file_from_node_id(&e.source_id),
            service_name: e.target_id.clone(),
            methods: vec![],
            modern_equivalent: "ASP.NET Core Middleware".into(),
            called_by: vec![],
        })
        .collect();
    let route_handlers: Vec<ServiceEndpoint> = routes
        .iter()
        .map(|e| ServiceEndpoint {
            file_path: extract_file_from_node_id(&e.source_id),
            service_name: e.target_id.clone(),
            methods: vec![],
            modern_equivalent: "app.MapGet/MapPost route".into(),
            called_by: vec![],
        })
        .collect();

    let total = web_services.len()
        + http_handlers.len()
        + wcf_services.len()
        + http_modules.len()
        + route_handlers.len();

    ServiceEndpointSummary {
        web_services,
        http_handlers,
        wcf_services,
        http_modules,
        route_handlers,
        total_endpoints: total,
    }
}

/// Extract likely file path from a node ID (often "filepath::symbol" or just "filepath").
fn extract_file_from_node_id(node_id: &str) -> String {
    // Node IDs often contain "::" separator between file and symbol
    if let Some(idx) = node_id.find("::") {
        node_id[..idx].to_string()
    } else {
        node_id.to_string()
    }
}

// ── Anti-pattern summary ──────────────────────────────────────────────────────

fn build_anti_pattern_summary(graph: &Arc<GraphStore>, project_id: &str) -> AntiPatternSummary {
    let detected =
        pattern_detection_service::detect_design_antipatterns(graph, project_id, 15, 5, 4)
            .unwrap_or_else(|e| {
                tracing::warn!("anti-pattern detection failed: {e}");
                vec![]
            });

    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut critical_items = Vec::new();
    let mut migration_impact = Vec::new();

    for ap in &detected {
        *by_type.entry(ap.pattern_name.clone()).or_insert(0) += 1;

        let severity_str = format!("{:?}", ap.severity);
        let file_path = ap
            .affected_nodes
            .first()
            .map_or("(unknown)", |s| s.as_str());
        let detail = if ap.evidence.is_empty() {
            ap.description.clone()
        } else {
            ap.evidence.join("; ")
        };

        critical_items.push(AntiPatternItem {
            pattern_type: ap.pattern_name.clone(),
            file_path: file_path.to_string(),
            node_name: ap.affected_nodes.first().cloned().unwrap_or_default(),
            severity: severity_str,
            detail,
            recommendation: ap.refactoring_steps.join(" → "),
        });
    }

    // Build migration impact statements
    for (name, count) in &by_type {
        let impact = match name.as_str() {
            "God Object" => format!(
                "{count} God Object pages should be split BEFORE migration (Wave 0 refactoring)"
            ),
            "Session Soup" => format!(
                "Session Soup keys must be consolidated before parallel wave execution ({count} instances)"
            ),
            "Spaghetti Events" => format!(
                "{count} Spaghetti Event chains indicate hidden coupling — verify with characterization tests"
            ),
            "SqlDataSource Coupling" => format!(
                "{count} SqlDataSource usages have inline SQL — extract to repository pattern"
            ),
            "Tight GIS Coupling" => {
                format!("{count} tightly coupled GIS components — extract map service layer")
            }
            "Windows Service" => {
                format!("{count} Windows Services — migrate to IHostedService / BackgroundService")
            }
            _ => format!("{count} {name} instances detected"),
        };
        migration_impact.push(impact);
    }

    AntiPatternSummary {
        total_anti_patterns: detected.len(),
        by_type,
        critical_items,
        migration_impact,
    }
}

// ── JavaScript / jQuery analysis ──────────────────────────────────────────────

fn build_js_analysis(
    graph: &Arc<GraphStore>,
    project_id: &str,
    markup_files: &[FileContent],
    js_files: &[(String, String)],
) -> JsAnalysisSummary {
    let dom_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::ManipulatesDom, 10_000)
        .unwrap_or_default();
    let postback_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::TriggersPostback, 10_000)
        .unwrap_or_default();
    let api_call_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::ApiCall, 10_000)
        .unwrap_or_default();
    let contains_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::Contains, 50_000)
        .unwrap_or_default();

    // Build DOM manipulation refs
    let dom_manipulations: Vec<JsDomRef> = dom_edges
        .iter()
        .map(|e| {
            let selector_type = e
                .metadata
                .as_ref()
                .and_then(|m| m.get("selector_type").and_then(|v| v.as_str()))
                .unwrap_or("unknown")
                .to_string();
            JsDomRef {
                js_file: extract_file_from_node_id(&e.source_id),
                target_control: e.target_id.clone(),
                selector_type,
            }
        })
        .collect();

    // Build postback trigger refs
    let postback_triggers: Vec<JsPostbackRef> = postback_edges
        .iter()
        .map(|e| {
            let unique_id = e
                .metadata
                .as_ref()
                .and_then(|m| m.get("unique_id").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            JsPostbackRef {
                js_file: extract_file_from_node_id(&e.source_id),
                target_control: e.target_id.clone(),
                unique_id,
            }
        })
        .collect();

    // Build AJAX call refs
    let ajax_calls: Vec<JsAjaxCall> = api_call_edges
        .iter()
        .map(|e| {
            let meta = e.metadata.as_ref();
            let transport = meta
                .and_then(|m| m.get("transport").and_then(|v| v.as_str()))
                .unwrap_or("unknown")
                .to_string();
            let target_method = meta
                .and_then(|m| m.get("method").and_then(|v| v.as_str()))
                .map(String::from);
            let target_type = meta
                .and_then(|m| m.get("target_type").and_then(|v| v.as_str()))
                .unwrap_or("unknown")
                .to_string();
            JsAjaxCall {
                js_file: extract_file_from_node_id(&e.source_id),
                target_url: e.target_id.clone(),
                transport,
                target_method,
                target_type,
            }
        })
        .collect();

    // Build page→control ownership map from Contains edges
    let mut control_to_page: BTreeMap<String, String> = BTreeMap::new();
    for e in &contains_edges {
        let source_file = extract_file_from_node_id(&e.source_id);
        if source_file.to_lowercase().ends_with(".aspx")
            || source_file.to_lowercase().ends_with(".ascx")
            || source_file.to_lowercase().ends_with(".master")
        {
            control_to_page.insert(e.target_id.clone(), source_file);
        }
    }

    // Build page↔JS dependency map
    let mut page_js_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // From graph edges: which JS files reference controls owned by which pages
    for dom_ref in &dom_manipulations {
        if let Some(page) = control_to_page.get(&dom_ref.target_control) {
            let js_list = page_js_deps.entry(page.clone()).or_default();
            if !js_list.contains(&dom_ref.js_file) {
                js_list.push(dom_ref.js_file.clone());
            }
        }
    }
    for pb_ref in &postback_triggers {
        if let Some(page) = control_to_page.get(&pb_ref.target_control) {
            let js_list = page_js_deps.entry(page.clone()).or_default();
            if !js_list.contains(&pb_ref.js_file) {
                js_list.push(pb_ref.js_file.clone());
            }
        }
    }

    // From markup: scan <script src="..."> tags
    let script_src_re = regex::Regex::new(r#"<script[^>]+src\s*=\s*["']([^"']+\.js)["']"#).unwrap();
    for fc in markup_files {
        for cap in script_src_re.captures_iter(&fc.markup_content) {
            let js_ref = cap[1].to_string();
            let js_list = page_js_deps.entry(fc.file_path.clone()).or_default();
            if !js_list.contains(&js_ref) {
                js_list.push(js_ref);
            }
        }
    }

    // Detect inline <script> blocks (not src= external files)
    let inline_re = regex::Regex::new(r"(?i)<script(?![^>]*\bsrc\s*=)[^>]*>").unwrap();
    let mut inline_script_files = Vec::new();
    for fc in markup_files {
        if inline_re.is_match(&fc.markup_content) {
            inline_script_files.push(fc.file_path.clone());
        }
    }

    // Detect jQuery version hint from JS files
    let jquery_re = regex::Regex::new(r"jquery[.-](\d+\.\d+(?:\.\d+)?)").unwrap();
    let mut jquery_version_hint = None;
    for (path, _content) in js_files {
        if let Some(cap) = jquery_re.captures(&path.to_lowercase()) {
            jquery_version_hint = Some(cap[1].to_string());
            break;
        }
    }

    // Count JS files with server-side dependencies
    let mut js_files_with_deps: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for dr in &dom_manipulations {
        js_files_with_deps.insert(dr.js_file.clone());
    }
    for pr in &postback_triggers {
        js_files_with_deps.insert(pr.js_file.clone());
    }
    for ac in &ajax_calls {
        js_files_with_deps.insert(ac.js_file.clone());
    }

    JsAnalysisSummary {
        total_js_files: js_files.len(),
        js_files_with_server_deps: js_files_with_deps.len(),
        dom_manipulations,
        postback_triggers,
        ajax_calls,
        page_js_dependencies: page_js_deps,
        inline_script_files,
        jquery_version_hint,
    }
}

// ── GIS / Spatial analysis ────────────────────────────────────────────────────

fn build_gis_analysis(
    graph: &Arc<GraphStore>,
    project_id: &str,
    target_stack: &str,
) -> GisAnalysisSummary {
    let spatial_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::SpatialCall, 10_000)
        .unwrap_or_default();

    // Query insight nodes for GIS inventories
    let gis_insights = graph
        .query_nodes(project_id, Some("insight"), None, None, 1_000)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| {
            let name_lower = n.name.to_lowercase();
            name_lower.contains("gis_inventory")
                || name_lower.contains("google_maps")
                || name_lower.contains("esri")
                || name_lower.contains("leaflet")
                || name_lower.contains("openlayers")
                || name_lower.contains("spatial")
        })
        .collect::<Vec<_>>();

    if spatial_edges.is_empty() && gis_insights.is_empty() {
        return GisAnalysisSummary {
            has_gis: false,
            libraries_detected: vec![],
            total_spatial_calls: 0,
            files_with_gis: vec![],
            migration_complexity: "none".into(),
            modern_targets: GisModernTargets {
                react: vec![],
                blazor: vec![],
                angular: vec![],
            },
        };
    }

    // Collect files with GIS
    let mut files_with_gis: Vec<String> = spatial_edges
        .iter()
        .map(|e| extract_file_from_node_id(&e.source_id))
        .collect();
    files_with_gis.sort();
    files_with_gis.dedup();

    // Build library summaries from insight metadata
    let mut libraries: BTreeMap<String, GisLibrarySummary> = BTreeMap::new();
    for insight in &gis_insights {
        if let Some(ref meta) = insight.metadata {
            let library = meta
                .get("library")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let entry = libraries
                .entry(library.clone())
                .or_insert_with(|| GisLibrarySummary {
                    library: library.clone(),
                    files: vec![],
                    class_count: 0,
                    features: vec![],
                    has_3d: false,
                    has_drawing: false,
                    has_geocoding: false,
                    has_clustering: false,
                    has_wms: false,
                    api_keys_detected: 0,
                    api_style: None,
                });

            let file = insight.file_path.as_str().to_string();
            if !entry.files.contains(&file) {
                entry.files.push(file);
            }

            if let Some(cc) = meta.get("class_count").and_then(|v| v.as_u64()) {
                entry.class_count = entry.class_count.max(cc as usize);
            }
            if meta
                .get("has_places_api")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if !entry.features.contains(&"Places API".to_string()) {
                    entry.features.push("Places API".into());
                }
            }
            if meta
                .get("has_streetview")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if !entry.features.contains(&"StreetView".to_string()) {
                    entry.features.push("StreetView".into());
                }
            }
            if meta
                .get("has_directions")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if !entry.features.contains(&"Directions".into()) {
                    entry.features.push("Directions".into());
                }
            }
            if meta
                .get("has_heatmap")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if !entry.features.contains(&"Heatmap".into()) {
                    entry.features.push("Heatmap".into());
                }
            }
            if meta
                .get("has_drawing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_drawing = true;
                if !entry.features.contains(&"Drawing tools".into()) {
                    entry.features.push("Drawing tools".into());
                }
            }
            if meta
                .get("has_geocoding")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_geocoding = true;
                if !entry.features.contains(&"Geocoding".into()) {
                    entry.features.push("Geocoding".into());
                }
            }
            if meta
                .get("has_kml")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if !entry.features.contains(&"KML layers".into()) {
                    entry.features.push("KML layers".into());
                }
            }
            if meta
                .get("has_3d")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_3d = true;
            }
            if meta
                .get("has_clustering")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_clustering = true;
            }
            if meta
                .get("has_wms")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_wms = true;
            }
            if let Some(keys) = meta.get("api_keys_detected").and_then(|v| v.as_u64()) {
                entry.api_keys_detected = entry.api_keys_detected.max(keys as usize);
            }
            if let Some(style) = meta.get("api_style").and_then(|v| v.as_str()) {
                entry.api_style = Some(style.to_string());
            }
        }
    }

    // Also gather from spatial edges if insights are missing
    for edge in &spatial_edges {
        let meta = edge.metadata.as_ref();
        let library = meta
            .and_then(|m| m.get("library").and_then(|v| v.as_str()))
            .unwrap_or("unknown")
            .to_string();
        let entry = libraries
            .entry(library.clone())
            .or_insert_with(|| GisLibrarySummary {
                library: library.clone(),
                files: vec![],
                class_count: 0,
                features: vec![],
                has_3d: false,
                has_drawing: false,
                has_geocoding: false,
                has_clustering: false,
                has_wms: false,
                api_keys_detected: 0,
                api_style: None,
            });
        let file = extract_file_from_node_id(&edge.source_id);
        if !entry.files.contains(&file) {
            entry.files.push(file);
        }
    }

    let libraries_vec: Vec<GisLibrarySummary> = libraries.into_values().collect();

    // Determine overall complexity
    let max_complexity_insight = gis_insights
        .iter()
        .filter_map(|n| n.metadata.as_ref()?.get("migration_complexity")?.as_str())
        .max_by_key(|c| match *c {
            "high" => 3,
            "medium" => 2,
            _ => 1,
        })
        .unwrap_or("medium");
    let migration_complexity = if libraries_vec.len() > 1 || spatial_edges.len() > 20 {
        "high".to_string()
    } else {
        max_complexity_insight.to_string()
    };

    // Modern targets based on target_stack
    let modern_targets = build_gis_modern_targets(&libraries_vec, target_stack);

    GisAnalysisSummary {
        has_gis: true,
        libraries_detected: libraries_vec,
        total_spatial_calls: spatial_edges.len(),
        files_with_gis,
        migration_complexity,
        modern_targets,
    }
}

fn build_gis_modern_targets(
    libraries: &[GisLibrarySummary],
    target_stack: &str,
) -> GisModernTargets {
    let mut react = Vec::new();
    let mut blazor = Vec::new();
    let mut angular = Vec::new();

    for lib in libraries {
        match lib.library.to_lowercase().as_str() {
            "google_maps" | "google maps" => {
                react.push("@react-google-maps/api".into());
                blazor.push("BlazorGoogleMaps NuGet".into());
                angular.push("@angular/google-maps".into());
            }
            "leaflet" => {
                react.push("react-leaflet".into());
                blazor.push("BlazorLeaflet NuGet".into());
                angular.push("ngx-leaflet".into());
            }
            "openlayers" => {
                react.push("rlayers".into());
                blazor.push("OpenLayers JS interop".into());
                angular.push("ngx-openlayers".into());
            }
            "esri_arcgis" | "esri" | "arcgis" => {
                react.push("@arcgis/core + React wrapper".into());
                blazor.push("ArcGIS REST JS (@esri/arcgis-rest-request)".into());
                angular.push("@arcgis/core + Angular wrapper".into());
            }
            _ => {}
        }
    }

    // Highlight the one matching target_stack
    let ts = target_stack.to_lowercase();
    if ts.contains("blazor") {
        react.clear();
        angular.clear();
    } else if ts.contains("react") {
        blazor.clear();
        angular.clear();
    } else if ts.contains("angular") {
        react.clear();
        blazor.clear();
    }

    GisModernTargets {
        react,
        blazor,
        angular,
    }
}

// ── Classic ASP summary ───────────────────────────────────────────────────────

fn build_classic_asp_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    asp_files: &[(String, String)],
) -> ClassicAspSummary {
    if asp_files.is_empty() {
        // Check graph for any existing classic ASP insights
        let asp_insights = graph
            .query_nodes(project_id, Some("insight"), None, None, 1_000)
            .unwrap_or_default()
            .into_iter()
            .filter(|n| n.name.to_lowercase().contains("classic_asp"))
            .count();
        if asp_insights == 0 {
            return ClassicAspSummary {
                total_asp_files: 0,
                com_objects: vec![],
                ado_connections: 0,
                sql_statements: 0,
                includes: vec![],
                state_accesses: 0,
                migration_effort_hours: 0.0,
            };
        }
    }

    let include_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::IncludesFile, 5_000)
        .unwrap_or_default();

    let mut com_objects = Vec::new();
    let mut ado_connections = 0usize;
    let mut sql_statements = 0usize;
    let mut state_accesses = 0usize;
    let mut includes = Vec::new();

    // Scan ASP file contents for patterns
    let create_obj_re = regex::Regex::new(r#"(?i)Server\.CreateObject\s*\(\s*"([^"]+)""#).unwrap();
    let sql_re =
        regex::Regex::new(r"(?i)(?:\.Execute|\.CommandText|SELECT\s|INSERT\s|UPDATE\s|DELETE\s)")
            .unwrap();
    let state_re =
        regex::Regex::new(r"(?i)(?:Session|Application|Request\.Cookies|Response\.Cookies)\s*\(")
            .unwrap();
    let include_re =
        regex::Regex::new(r#"(?i)<!--\s*#include\s+(?:file|virtual)\s*=\s*"([^"]+)""#).unwrap();

    for (path, content) in asp_files {
        for cap in create_obj_re.captures_iter(content) {
            let prog_id = cap[1].to_string();
            if prog_id.to_lowercase().contains("adodb") {
                ado_connections += 1;
            }
            com_objects.push(ComObjectRef {
                file_path: path.clone(),
                prog_id,
            });
        }
        sql_statements += sql_re.find_iter(content).count();
        state_accesses += state_re.find_iter(content).count();
        for cap in include_re.captures_iter(content) {
            includes.push(IncludeRef {
                source_file: path.clone(),
                included_file: cap[1].to_string(),
            });
        }
    }

    // Also gather includes from graph edges for .asp files
    for e in &include_edges {
        let src = extract_file_from_node_id(&e.source_id);
        if src.to_lowercase().ends_with(".asp") {
            let inc = IncludeRef {
                source_file: src,
                included_file: e.target_id.clone(),
            };
            if !includes
                .iter()
                .any(|i| i.source_file == inc.source_file && i.included_file == inc.included_file)
            {
                includes.push(inc);
            }
        }
    }

    // Estimate effort: ~2h per ASP file + 0.5h per COM object + 0.25h per SQL statement
    let effort = (asp_files.len() as f64 * 2.0)
        + (com_objects.len() as f64 * 0.5)
        + (sql_statements as f64 * 0.25);

    ClassicAspSummary {
        total_asp_files: asp_files.len(),
        com_objects,
        ado_connections,
        sql_statements,
        includes,
        state_accesses,
        migration_effort_hours: effort,
    }
}

// ── Report summary ────────────────────────────────────────────────────────────

fn build_report_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    report_files: &[(String, String)],
) -> ReportSummary {
    // Query graph for report-related insights
    let all_insights = graph
        .query_nodes(project_id, Some("insight"), None, None, 2_000)
        .unwrap_or_default();

    let report_insights: Vec<_> = all_insights
        .iter()
        .filter(|n| {
            let name = n.name.to_lowercase();
            name.contains("report")
                || name.contains("crystal")
                || name.contains("ssrs")
                || name.contains("rdl")
        })
        .collect();

    // Also query for anti-pattern edges related to Crystal Reports
    let ap_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::AntiPattern, 5_000)
        .unwrap_or_default();
    let crystal_edges: Vec<_> = ap_edges
        .iter()
        .filter(|e| {
            e.metadata
                .as_ref()
                .and_then(|m| m.get("pattern").and_then(|v| v.as_str()))
                .map_or(false, |p| p.to_lowercase().contains("crystal"))
        })
        .collect();

    let mut ssrs_reports = Vec::new();
    let mut crystal_reports = Vec::new();
    let mut shared_data_sources = Vec::new();
    let mut has_binary_rpt = false;

    // Parse SSRS report files (.rdl, .rdlc)
    let dataset_re = regex::Regex::new(r#"<DataSet\s+Name\s*=\s*"([^"]+)""#).unwrap();
    let param_re = regex::Regex::new(r#"<ReportParameter\s+Name\s*=\s*"([^"]+)""#).unwrap();
    let subreport_re =
        regex::Regex::new(r#"<Subreport[^>]*>.*?<ReportName>([^<]+)</ReportName>"#).unwrap();
    let datasource_re = regex::Regex::new(r#"<DataSource\s+Name\s*=\s*"([^"]+)""#).unwrap();

    for (path, content) in report_files {
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
        if ext == "rdl" || ext == "rdlc" {
            let datasets: Vec<String> = dataset_re
                .captures_iter(content)
                .map(|c| c[1].to_string())
                .collect();
            let param_count = param_re.find_iter(content).count();
            let subreports: Vec<String> = subreport_re
                .captures_iter(content)
                .map(|c| c[1].to_string())
                .collect();
            for cap in datasource_re.captures_iter(content) {
                let ds = cap[1].to_string();
                if !shared_data_sources.contains(&ds) {
                    shared_data_sources.push(ds);
                }
            }
            ssrs_reports.push(ReportInfo {
                file_path: path.clone(),
                datasets,
                parameters: param_count,
                subreports,
                migration_target: if ext == "rdlc" {
                    "SSRS on modern / Power BI paginated".into()
                } else {
                    "Power BI / SSRS on modern SQL Server".into()
                },
            });
        }
    }

    // Crystal Reports from graph insights and anti-pattern edges
    for insight in &report_insights {
        let name = &insight.name;
        if name.to_lowercase().contains("crystal") {
            let file = insight.file_path.as_str().to_string();
            let rpt_file = insight
                .metadata
                .as_ref()
                .and_then(|m| m.get("report_file").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            if rpt_file.ends_with(".rpt") {
                has_binary_rpt = true;
            }
            crystal_reports.push(CrystalReportInfo {
                file_path: file,
                report_file: rpt_file,
                is_binary: true,
                modern_equivalent: "Power BI / SSRS / Telerik Reporting".into(),
            });
        }
    }

    // Also detect Crystal from anti-pattern edges
    for edge in &crystal_edges {
        let file = extract_file_from_node_id(&edge.source_id);
        if !crystal_reports.iter().any(|cr| cr.file_path == file) {
            crystal_reports.push(CrystalReportInfo {
                file_path: file,
                report_file: String::new(),
                is_binary: true,
                modern_equivalent: "Power BI / SSRS / Telerik Reporting".into(),
            });
            has_binary_rpt = true;
        }
    }

    let total = ssrs_reports.len() + crystal_reports.len();

    ReportSummary {
        ssrs_reports,
        crystal_reports,
        total_reports: total,
        has_binary_rpt_files: has_binary_rpt,
        shared_data_sources,
    }
}

// ── Markdown renderer ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_markdown(
    project_id: &str,
    target_stack: &str,
    generated_at: &str,
    order: &MigrationOrderPlan,
    state: &StateMigrationReport,
    auth: &AuthConfigMap,
    data_access: &[FileDataAccessProfile],
    dossiers: &[MigrationDossier],
    cross: &CrossCuttingSummary,
    wave_lookup: &BTreeMap<String, u32>,
    js: &JsAnalysisSummary,
    gis: &GisAnalysisSummary,
    webconfig: &WebConfigInventory,
    endpoints: &ServiceEndpointSummary,
    global: &GlobalAsaxSummary,
    anti: &AntiPatternSummary,
    asp: &ClassicAspSummary,
    rpt: &ReportSummary,
) -> String {
    let mut md = String::with_capacity(64_000);

    // ── Header ────────────────────────────────────────────────────────────
    md.push_str(&format!(
        "# Full Migration Analysis — {project_id}\n\n\
         Generated: {generated_at} | Target: **{target_stack}**\n\n"
    ));

    // ── Executive Summary ─────────────────────────────────────────────────
    md.push_str("## Executive Summary\n\n");
    md.push_str(&format!(
        "- **Total pages analyzed**: {}\n",
        cross.total_pages_analyzed
    ));
    for (complexity, count) in &cross.complexity_distribution {
        md.push_str(&format!("- {complexity} complexity: {count} files\n"));
    }
    md.push_str(&format!("- **Migration waves**: {}\n", order.waves.len()));
    md.push_str(&format!(
        "- **Circular dependencies**: {}\n",
        order.circular_dependencies.len()
    ));
    md.push_str(&format!(
        "- **Bottleneck files**: {}\n",
        order.bottleneck_files.len()
    ));
    md.push_str(&format!(
        "- **Total state keys**: {}\n",
        state.summary.total_state_keys
    ));
    md.push_str(&format!(
        "- **High-risk state keys**: {}\n",
        state.summary.high_risk_keys.len()
    ));
    md.push_str(&format!(
        "- **Total validators**: {}\n",
        cross.total_validators
    ));
    md.push_str(&format!(
        "- **Total UpdatePanels**: {}\n",
        cross.total_update_panels
    ));
    md.push_str(&format!(
        "- **Files with IsPostBack branching**: {}\n",
        cross.files_with_ispostback
    ));
    if cross.total_js_files > 0 {
        md.push_str(&format!(
            "- **JavaScript files**: {} ({} with server-side dependencies)\n",
            cross.total_js_files, js.js_files_with_server_deps
        ));
    }
    if cross.total_gis_libraries > 0 {
        md.push_str(&format!(
            "- **GIS libraries**: {}\n",
            cross.total_gis_libraries
        ));
    }
    if cross.total_anti_patterns > 0 {
        md.push_str(&format!(
            "- **Design anti-patterns**: {}\n",
            cross.total_anti_patterns
        ));
    }
    if cross.total_service_endpoints > 0 {
        md.push_str(&format!(
            "- **Service endpoints**: {} (ASMX/ASHX/WCF/Modules)\n",
            cross.total_service_endpoints
        ));
    }
    if cross.total_classic_asp_files > 0 {
        md.push_str(&format!(
            "- **Classic ASP files**: {}\n",
            cross.total_classic_asp_files
        ));
    }
    if cross.total_reports > 0 {
        md.push_str(&format!(
            "- **Reports (SSRS/Crystal)**: {}\n",
            cross.total_reports
        ));
    }
    if !cross.critical_risk_files.is_empty() {
        md.push_str(&format!(
            "- **Critical-risk files**: {}\n",
            cross.critical_risk_files.join(", ")
        ));
    }
    md.push_str("\n");

    // ── Authentication & Authorization ────────────────────────────────────
    md.push_str("## Authentication & Authorization\n\n");
    md.push_str(&format!(
        "**Auth mode**: {} | **Complexity**: {}\n\n",
        auth.auth_mode, auth.migration_complexity
    ));
    if let Some(ref fa) = auth.forms_auth {
        md.push_str(&format!(
            "- Forms Auth: login=`{}`, timeout={}min, cookie=`{}`\n",
            fa.login_url, fa.timeout_minutes, fa.cookie_name
        ));
    }
    if auth.windows_auth.is_some() {
        md.push_str("- Windows Authentication detected\n");
    }
    if !auth.location_rules.is_empty() {
        md.push_str(&format!(
            "- {} location authorization rules\n",
            auth.location_rules.len()
        ));
        for lr in &auth.location_rules {
            md.push_str(&format!("  - `{}`: ", lr.path));
            if !lr.allow_roles.is_empty() {
                md.push_str(&format!("allow [{}] ", lr.allow_roles.join(", ")));
            }
            if !lr.deny_users.is_empty() {
                md.push_str(&format!("deny [{}]", lr.deny_users.join(", ")));
            }
            md.push('\n');
        }
    }
    if !auth.code_auth_checks.is_empty() {
        md.push_str(&format!(
            "- {} code-level auth checks across {} files\n",
            auth.code_auth_checks.len(),
            {
                let mut files: Vec<&str> = auth
                    .code_auth_checks
                    .iter()
                    .map(|c| c.file_path.as_str())
                    .collect();
                files.sort();
                files.dedup();
                files.len()
            }
        ));
    }
    if !auth.session_auth_patterns.is_empty() {
        md.push_str(&format!(
            "- **{} session-based auth anti-patterns** (must migrate to Identity)\n",
            auth.session_auth_patterns.len()
        ));
    }
    if !auth.recommendations.is_empty() {
        md.push_str("\n**Recommendations:**\n");
        for r in &auth.recommendations {
            md.push_str(&format!(
                "- [{}] {}: {}\n",
                r.severity, r.category, r.recommendation
            ));
        }
    }
    md.push('\n');

    // ── Global.asax (Phase 32) ───────────────────────────────────────────
    if global.has_global_asax {
        md.push_str("## Application Lifecycle (Global.asax)\n\n");
        if let Some(ref cls) = global.codebehind_class {
            md.push_str(&format!("**Class**: `{cls}`\n\n"));
        }
        if !global.lifecycle_events.is_empty() {
            md.push_str("### Lifecycle Events\n");
            md.push_str("| Event | Lines | Key Actions | Modern Equivalent |\n");
            md.push_str("|-------|-------|-------------|-------------------|\n");
            for ev in &global.lifecycle_events {
                let actions = if ev.key_actions.is_empty() {
                    "(none detected)".to_string()
                } else {
                    ev.key_actions.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ev.event_name, ev.line_count, actions, ev.modern_equivalent
                ));
            }
            md.push('\n');
        }
        if !global.startup_registrations.is_empty() {
            md.push_str("### Startup Registrations (→ Program.cs)\n");
            for reg in &global.startup_registrations {
                md.push_str(&format!(
                    "- **{}**: {}\n",
                    reg.registration_type, reg.detail
                ));
            }
            md.push('\n');
        }
        if !global.modern_mapping.is_empty() {
            md.push_str("### Migration Notes\n");
            for mm in &global.modern_mapping {
                md.push_str(&format!("- {} → {}\n", mm.legacy, mm.modern));
            }
            md.push('\n');
        }
    }

    // ── web.config Inventory (Phase 32) ──────────────────────────────────
    if !webconfig.connection_strings.is_empty()
        || !webconfig.app_settings.is_empty()
        || webconfig.session_state.is_some()
        || !webconfig.http_handlers.is_empty()
        || !webconfig.http_modules.is_empty()
    {
        md.push_str("## Configuration (web.config)\n\n");

        if !webconfig.connection_strings.is_empty() {
            md.push_str("### Connection Strings\n");
            md.push_str("| Name | Provider | Integrated Auth | Used By |\n");
            md.push_str("|------|----------|-----------------|--------|\n");
            for cs in &webconfig.connection_strings {
                let used = if cs.used_by.is_empty() {
                    "(none found)".to_string()
                } else {
                    cs.used_by.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    cs.name, cs.provider, cs.has_integrated_security, used
                ));
            }
            md.push('\n');
        }

        if !webconfig.app_settings.is_empty() {
            md.push_str(&format!(
                "### App Settings ({} keys)\n",
                webconfig.app_settings.len()
            ));
            md.push_str("| Key | Preview | Used By |\n");
            md.push_str("|-----|---------|--------|\n");
            for a in &webconfig.app_settings {
                let used = if a.used_by.is_empty() {
                    "(none found)".to_string()
                } else {
                    a.used_by.join(", ")
                };
                md.push_str(&format!("| {} | {} | {} |\n", a.key, a.value_preview, used));
            }
            md.push('\n');
        }

        if let Some(ref ss) = webconfig.session_state {
            md.push_str(&format!("### Session State\n**Mode**: {}", ss.mode));
            if let Some(t) = ss.timeout_minutes {
                md.push_str(&format!(" | **Timeout**: {}min", t));
            }
            md.push('\n');
            let migration_hint = match ss.mode.as_str() {
                "InProc" => "Replace with IDistributedCache (Redis or SQL Server)",
                "StateServer" => "Replace with Redis-backed IDistributedCache",
                "SQLServer" => "Replace with distributed cache (Redis/IDistributedCache)",
                "Custom" => "Evaluate custom provider → IDistributedCache adapter",
                _ => "Replace with IDistributedCache",
            };
            md.push_str(&format!("→ Migration: {migration_hint}\n\n"));
        }

        if !webconfig.http_handlers.is_empty() {
            md.push_str(&format!(
                "### HTTP Handlers ({})\n",
                webconfig.http_handlers.len()
            ));
            md.push_str("| Verb | Path | Type |\n|------|------|------|\n");
            for h in &webconfig.http_handlers {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    h.verb, h.path, h.handler_type
                ));
            }
            md.push_str("→ Migration: Replace with Minimal API / Controller endpoints\n\n");
        }

        if !webconfig.http_modules.is_empty() {
            md.push_str(&format!(
                "### HTTP Modules ({})\n",
                webconfig.http_modules.len()
            ));
            md.push_str("| Name | Type |\n|------|------|\n");
            for m in &webconfig.http_modules {
                md.push_str(&format!("| {} | {} |\n", m.name, m.module_type));
            }
            md.push_str("→ Migration: Replace with ASP.NET Core middleware\n\n");
        }

        if let Some(ref ce) = webconfig.custom_errors {
            md.push_str(&format!("### Custom Errors\n**Mode**: {}", ce.mode));
            if let Some(ref dr) = ce.default_redirect {
                md.push_str(&format!(" | Default: {dr}"));
            }
            md.push('\n');
            for (code, redirect) in &ce.status_redirects {
                md.push_str(&format!("- {code} → {redirect}\n"));
            }
            md.push_str("→ Migration: Replace with UseExceptionHandler + UseStatusCodePagesWithReExecute\n\n");
        }

        if let Some(ref comp) = webconfig.compilation {
            md.push_str(&format!("### Compilation\n**Debug**: {}", comp.debug));
            if let Some(ref tf) = comp.target_framework {
                md.push_str(&format!(" | **Target Framework**: {tf}"));
            }
            md.push('\n');
            if !comp.assemblies.is_empty() {
                md.push_str(&format!(
                    "**Referenced assemblies**: {}\n",
                    comp.assemblies.len()
                ));
            }
            md.push('\n');
        }
    }

    // ── State Management ──────────────────────────────────────────────────
    md.push_str("## State Management (Project-Wide)\n\n");
    md.push_str(&format!(
        "**Total state keys**: {}\n\n",
        state.summary.total_state_keys
    ));
    if !state.summary.by_store.is_empty() {
        md.push_str("| Store | Keys |\n|-------|------|\n");
        for (store, count) in &state.summary.by_store {
            md.push_str(&format!("| {store} | {count} |\n"));
        }
        md.push('\n');
    }
    if !state.summary.by_target.is_empty() {
        md.push_str("**Migration targets:**\n");
        for (target, count) in &state.summary.by_target {
            md.push_str(&format!("- {target}: {count} keys\n"));
        }
        md.push('\n');
    }
    if !state.summary.high_risk_keys.is_empty() {
        md.push_str("**High-risk keys:**\n");
        for k in &state.summary.high_risk_keys {
            md.push_str(&format!("- `{k}`\n"));
        }
        md.push('\n');
    }

    // ── Data Access Patterns ──────────────────────────────────────────────
    if !data_access.is_empty() {
        md.push_str("## Data Access Patterns\n\n");
        let mut pattern_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut total_tables = 0usize;
        let mut injection_risk_files = Vec::new();
        for dap in data_access {
            *pattern_counts
                .entry(format!("{:?}", dap.primary_pattern))
                .or_insert(0) += 1;
            total_tables += dap.table_count;
            if dap.has_concatenated_sql {
                injection_risk_files.push(dap.file_path.clone());
            }
        }
        md.push_str(&format!(
            "**Files with data access**: {} | **Total tables**: {}\n\n",
            data_access.len(),
            total_tables
        ));
        md.push_str("| Pattern | Files |\n|---------|-------|\n");
        for (pattern, count) in &pattern_counts {
            md.push_str(&format!("| {pattern} | {count} |\n"));
        }
        md.push('\n');
        if !injection_risk_files.is_empty() {
            md.push_str(&format!(
                "**SQL injection risk** (concatenated SQL): {}\n\n",
                injection_risk_files.join(", ")
            ));
        }
    }

    // ── Service Endpoints (Phase 32) ─────────────────────────────────────
    if endpoints.total_endpoints > 0 {
        md.push_str("## Service Endpoints\n\n");
        if !endpoints.web_services.is_empty() {
            md.push_str(&format!(
                "**Web Services (ASMX)**: {}\n",
                endpoints.web_services.len()
            ));
            md.push_str("| File | Service | Methods | Called By |\n");
            md.push_str("|------|---------|---------|----------|\n");
            for ep in &endpoints.web_services {
                let methods = if ep.methods.is_empty() {
                    "(see code)".into()
                } else {
                    ep.methods.join(", ")
                };
                let callers = if ep.called_by.is_empty() {
                    "(none detected)".into()
                } else {
                    ep.called_by.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ep.file_path, ep.service_name, methods, callers
                ));
            }
            md.push('\n');
        }
        if !endpoints.http_handlers.is_empty() {
            md.push_str(&format!(
                "**HTTP Handlers (ASHX)**: {}\n",
                endpoints.http_handlers.len()
            ));
            md.push_str("| File | Handler | Modern Equivalent |\n");
            md.push_str("|------|---------|-------------------|\n");
            for ep in &endpoints.http_handlers {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    ep.file_path, ep.service_name, ep.modern_equivalent
                ));
            }
            md.push('\n');
        }
        if !endpoints.wcf_services.is_empty() {
            md.push_str(&format!(
                "**WCF Services (SVC)**: {}\n",
                endpoints.wcf_services.len()
            ));
            md.push_str("| File | Service | Modern Equivalent |\n");
            md.push_str("|------|---------|-------------------|\n");
            for ep in &endpoints.wcf_services {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    ep.file_path, ep.service_name, ep.modern_equivalent
                ));
            }
            md.push('\n');
        }
        if !endpoints.http_modules.is_empty() {
            md.push_str(&format!(
                "**HTTP Modules**: {}\n",
                endpoints.http_modules.len()
            ));
            md.push_str("| Module | Type | Modern Equivalent |\n");
            md.push_str("|--------|------|-------------------|\n");
            for ep in &endpoints.http_modules {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    ep.file_path, ep.service_name, ep.modern_equivalent
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Impact\n");
        if !endpoints.web_services.is_empty() {
            md.push_str(&format!(
                "- {} ASMX services → Web API / Minimal API controllers\n",
                endpoints.web_services.len()
            ));
        }
        if !endpoints.http_handlers.is_empty() {
            md.push_str(&format!(
                "- {} ASHX handlers → Middleware or endpoint routes\n",
                endpoints.http_handlers.len()
            ));
        }
        if !endpoints.wcf_services.is_empty() {
            md.push_str(&format!(
                "- {} WCF services → gRPC or REST API\n",
                endpoints.wcf_services.len()
            ));
        }
        if !endpoints.http_modules.is_empty() {
            md.push_str(&format!(
                "- {} HTTP modules → ASP.NET Core middleware pipeline\n",
                endpoints.http_modules.len()
            ));
        }
        md.push('\n');
    }

    // ── JavaScript & Client-Side Dependencies (Phase 32) ─────────────────
    if js.total_js_files > 0 || !js.dom_manipulations.is_empty() || !js.ajax_calls.is_empty() {
        md.push_str("## JavaScript & Client-Side Dependencies\n\n");
        md.push_str(&format!(
            "**JS files**: {} ({} with server-side dependencies)\n",
            js.total_js_files, js.js_files_with_server_deps
        ));
        if !js.dom_manipulations.is_empty() {
            let jquery_count = js
                .dom_manipulations
                .iter()
                .filter(|d| d.selector_type.contains("jquery"))
                .count();
            let getbyid_count = js
                .dom_manipulations
                .iter()
                .filter(|d| {
                    d.selector_type.contains("getelementbyid")
                        || d.selector_type.contains("getElementById")
                })
                .count();
            let clientid_count = js
                .dom_manipulations
                .iter()
                .filter(|d| {
                    d.selector_type.contains("client_id") || d.selector_type.contains("asp_client")
                })
                .count();
            md.push_str(&format!(
                "**DOM manipulations**: {} (jQuery: {}, getElementById: {}, ASP ClientID: {})\n",
                js.dom_manipulations.len(),
                jquery_count,
                getbyid_count,
                clientid_count
            ));
        }
        if !js.postback_triggers.is_empty() {
            md.push_str(&format!(
                "**Postback triggers**: {} __doPostBack calls from JS\n",
                js.postback_triggers.len()
            ));
        }
        if !js.ajax_calls.is_empty() {
            // Transport breakdown
            let mut transport_counts: BTreeMap<String, usize> = BTreeMap::new();
            for ac in &js.ajax_calls {
                *transport_counts.entry(ac.transport.clone()).or_insert(0) += 1;
            }
            let breakdown: Vec<String> = transport_counts
                .iter()
                .map(|(t, c)| format!("{t}: {c}"))
                .collect();
            md.push_str(&format!(
                "**AJAX calls**: {} ({})\n",
                js.ajax_calls.len(),
                breakdown.join(", ")
            ));
        }
        if let Some(ref jq) = js.jquery_version_hint {
            md.push_str(&format!("**jQuery version**: {jq}\n"));
        }
        md.push('\n');

        if !js.ajax_calls.is_empty() {
            md.push_str("### AJAX Endpoint Inventory\n");
            md.push_str("| JS File | Target URL | Transport | Method | Target Type |\n");
            md.push_str("|---------|-----------|-----------|--------|-------------|\n");
            for ac in &js.ajax_calls {
                let method = ac.target_method.as_deref().unwrap_or("(N/A)");
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    ac.js_file, ac.target_url, ac.transport, method, ac.target_type
                ));
            }
            md.push('\n');
        }

        if !js.page_js_dependencies.is_empty() {
            md.push_str("### Page ↔ JS Dependencies\n");
            md.push_str("| Page | JS Files | DOM Refs | Postbacks | AJAX Calls |\n");
            md.push_str("|------|----------|----------|-----------|------------|\n");
            for (page, js_files_list) in &js.page_js_dependencies {
                let dom_count = js
                    .dom_manipulations
                    .iter()
                    .filter(|d| js_files_list.contains(&d.js_file))
                    .count();
                let pb_count = js
                    .postback_triggers
                    .iter()
                    .filter(|p| js_files_list.contains(&p.js_file))
                    .count();
                let ajax_count = js
                    .ajax_calls
                    .iter()
                    .filter(|a| js_files_list.contains(&a.js_file))
                    .count();
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    page,
                    js_files_list.join(", "),
                    dom_count,
                    pb_count,
                    ajax_count
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Impact\n");
        if !js.dom_manipulations.is_empty() {
            md.push_str(&format!("- {} JS files manipulate server control IDs → must update to modern component selectors\n",
                js.js_files_with_server_deps));
        }
        if !js.postback_triggers.is_empty() {
            md.push_str(&format!("- {} `__doPostBack` calls → must replace with component event handlers / SignalR\n",
                js.postback_triggers.len()));
        }
        let asmx_ajax = js
            .ajax_calls
            .iter()
            .filter(|a| a.target_url.contains(".asmx"))
            .count();
        if asmx_ajax > 0 {
            md.push_str(&format!("- {asmx_ajax} AJAX calls to .asmx → must migrate to Web API / Minimal API endpoints\n"));
        }
        let page_methods = js
            .ajax_calls
            .iter()
            .filter(|a| a.transport == "page_methods")
            .count();
        if page_methods > 0 {
            md.push_str(&format!("- {page_methods} PageMethods calls → must migrate to Blazor JS interop / API calls\n"));
        }
        if !js.inline_script_files.is_empty() {
            md.push_str(&format!(
                "- {} files have inline `<script>` blocks → extract to separate JS modules\n",
                js.inline_script_files.len()
            ));
        }
        md.push('\n');
    }

    // ── GIS / Spatial Analysis (Phase 32) ────────────────────────────────
    if gis.has_gis {
        md.push_str("## GIS / Spatial Analysis\n\n");
        let lib_summary: Vec<String> = gis
            .libraries_detected
            .iter()
            .map(|l| format!("{} ({} files)", l.library, l.files.len()))
            .collect();
        md.push_str(&format!("**Libraries**: {}\n", lib_summary.join(", ")));
        md.push_str(&format!(
            "**Total spatial calls**: {}\n",
            gis.total_spatial_calls
        ));
        md.push_str(&format!(
            "**Migration complexity**: {}\n\n",
            gis.migration_complexity
        ));

        for lib in &gis.libraries_detected {
            md.push_str(&format!("### {}\n", lib.library));
            md.push_str(&format!("- **Files**: {}\n", lib.files.join(", ")));
            if !lib.features.is_empty() {
                md.push_str(&format!("- **Features**: {}\n", lib.features.join(", ")));
            }
            if let Some(ref style) = lib.api_style {
                md.push_str(&format!("- **API style**: {style}\n"));
            }
            if lib.has_3d {
                md.push_str("- **3D support**: Yes\n");
            }
            if lib.api_keys_detected > 0 {
                md.push_str(&format!(
                    "- **API keys detected**: {}\n",
                    lib.api_keys_detected
                ));
            }
            // Show modern target based on target_stack
            if !gis.modern_targets.blazor.is_empty() {
                md.push_str(&format!(
                    "- **Modern target ({target_stack})**: {}\n",
                    gis.modern_targets.blazor.join(", ")
                ));
            } else if !gis.modern_targets.react.is_empty() {
                md.push_str(&format!(
                    "- **Modern target ({target_stack})**: {}\n",
                    gis.modern_targets.react.join(", ")
                ));
            } else if !gis.modern_targets.angular.is_empty() {
                md.push_str(&format!(
                    "- **Modern target ({target_stack})**: {}\n",
                    gis.modern_targets.angular.join(", ")
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Considerations\n");
        for lib in &gis.libraries_detected {
            match lib.library.to_lowercase().as_str() {
                "google_maps" | "google maps" => {
                    md.push_str("- Google Maps JS API → wrapper component needed (direct DOM → component binding)\n");
                }
                "esri_arcgis" | "esri" | "arcgis" => {
                    md.push_str(
                        "- Esri AMD → ES module migration required (Dojo → modern bundler)\n",
                    );
                }
                "leaflet" => {
                    md.push_str("- Leaflet → wrapper component with proper lifecycle management\n");
                }
                "openlayers" => {
                    md.push_str(
                        "- OpenLayers → wrapper component with proper lifecycle management\n",
                    );
                }
                _ => {}
            }
        }
        let wms_count = gis.libraries_detected.iter().filter(|l| l.has_wms).count();
        if wms_count > 0 {
            md.push_str(&format!(
                "- {wms_count} WMS layer endpoint(s) must be preserved\n"
            ));
        }
        let key_count: usize = gis
            .libraries_detected
            .iter()
            .map(|l| l.api_keys_detected)
            .sum();
        if key_count > 0 {
            md.push_str(&format!(
                "- {key_count} API key(s) must be migrated to server-side configuration\n"
            ));
        }
        md.push('\n');
    }

    // ── Design Anti-Patterns (Phase 32) ──────────────────────────────────
    if anti.total_anti_patterns > 0 {
        md.push_str("## Design Anti-Patterns\n\n");
        md.push_str(&format!(
            "**Total detected**: {}\n\n",
            anti.total_anti_patterns
        ));
        md.push_str("| Type | Count | Impact |\n|------|-------|--------|\n");
        let impact_map: std::collections::HashMap<&str, &str> = [
            (
                "God Object",
                "Must split before migration — too many responsibilities",
            ),
            (
                "Session Soup",
                "Blocks parallel migration — shared mutable state",
            ),
            (
                "Spaghetti Events",
                "Cross-file event chains — map dependencies carefully",
            ),
            (
                "SqlDataSource Coupling",
                "Inline SQL + data binding — extract to repository",
            ),
            (
                "Tight GIS Coupling",
                "GIS tightly bound to data — extract map service",
            ),
            (
                "Windows Service",
                "Background processing — migrate to IHostedService",
            ),
        ]
        .into_iter()
        .collect();
        for (name, count) in &anti.by_type {
            let impact = impact_map
                .get(name.as_str())
                .unwrap_or(&"Review before migration");
            md.push_str(&format!("| {name} | {count} | {impact} |\n"));
        }
        md.push('\n');

        if !anti.critical_items.is_empty() {
            md.push_str("### Critical Items\n");
            for item in &anti.critical_items {
                md.push_str(&format!(
                    "- **{}**: `{}` — {} → {}\n",
                    item.pattern_type, item.file_path, item.detail, item.recommendation
                ));
            }
            md.push('\n');
        }

        if !anti.migration_impact.is_empty() {
            md.push_str("### Migration Impact\n");
            for impact in &anti.migration_impact {
                md.push_str(&format!("- {impact}\n"));
            }
            md.push('\n');
        }
    }

    // ── Classic ASP (Phase 32) ───────────────────────────────────────────
    if asp.total_asp_files > 0 {
        md.push_str("## Classic ASP Files\n\n");
        md.push_str(&format!(
            "**Files**: {} | **Estimated effort**: {:.0}h\n\n",
            asp.total_asp_files, asp.migration_effort_hours
        ));

        // Group COM objects by file
        let mut asp_by_file: BTreeMap<String, (Vec<String>, usize, Vec<String>, usize)> =
            BTreeMap::new();
        for co in &asp.com_objects {
            asp_by_file
                .entry(co.file_path.clone())
                .or_default()
                .0
                .push(co.prog_id.clone());
        }
        for inc in &asp.includes {
            asp_by_file
                .entry(inc.source_file.clone())
                .or_default()
                .2
                .push(inc.included_file.clone());
        }

        if !asp_by_file.is_empty() {
            md.push_str("| File | COM Objects | Includes |\n|------|-------------|----------|\n");
            for (file, (coms, _, incs, _)) in &asp_by_file {
                let com_list = if coms.is_empty() {
                    "(none)".into()
                } else {
                    coms.join(", ")
                };
                let inc_list = if incs.is_empty() {
                    "(none)".into()
                } else {
                    incs.join(", ")
                };
                md.push_str(&format!("| {file} | {com_list} | {inc_list} |\n"));
            }
            md.push('\n');
        }

        md.push_str("### Migration Path\n");
        md.push_str("- Classic ASP → ASP.NET Core Razor Pages or Blazor\n");
        md.push_str("- COM objects (ADODB) → Entity Framework Core / Dapper\n");
        md.push_str("- Server-side includes → Partial views / Razor components\n");
        md.push_str("- `Response.Write` → Razor template syntax\n\n");
    }

    // ── Reports (Phase 32) ──────────────────────────────────────────────
    if rpt.total_reports > 0 {
        md.push_str("## Reports (SSRS / Crystal)\n\n");
        md.push_str(&format!(
            "**SSRS reports**: {} | **Crystal Reports**: {}\n\n",
            rpt.ssrs_reports.len(),
            rpt.crystal_reports.len()
        ));

        if !rpt.ssrs_reports.is_empty() {
            md.push_str("### SSRS Reports\n");
            md.push_str("| File | Datasets | Parameters | Subreports | Target |\n");
            md.push_str("|------|----------|------------|------------|--------|\n");
            for r in &rpt.ssrs_reports {
                let ds = if r.datasets.is_empty() {
                    "(none)".into()
                } else {
                    r.datasets.join(", ")
                };
                let sub = if r.subreports.is_empty() {
                    "(none)".into()
                } else {
                    r.subreports.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    r.file_path, ds, r.parameters, sub, r.migration_target
                ));
            }
            md.push('\n');
        }

        if !rpt.crystal_reports.is_empty() {
            md.push_str("### Crystal Reports\n");
            md.push_str("| File | Report (.rpt) | Binary | Modern Equivalent |\n");
            md.push_str("|------|--------------|--------|-------------------|\n");
            for cr in &rpt.crystal_reports {
                let rpt_name = if cr.report_file.is_empty() {
                    "(embedded)".into()
                } else {
                    cr.report_file.clone()
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    cr.file_path, rpt_name, cr.is_binary, cr.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if rpt.has_binary_rpt_files {
            md.push_str(&format!("**Warning**: {} binary .rpt files cannot be automatically migrated — manual recreation required\n\n",
                rpt.crystal_reports.len()));
        }

        if !rpt.shared_data_sources.is_empty() {
            md.push_str(&format!(
                "**Shared data sources**: {}\n\n",
                rpt.shared_data_sources.join(", ")
            ));
        }
    }

    // ── Migration Wave Plan ───────────────────────────────────────────────
    md.push_str("## Migration Wave Plan\n\n");
    for wave in &order.waves {
        md.push_str(&format!("### Wave {} — {}\n", wave.wave_number, wave.theme));
        if !wave.prerequisites.is_empty() {
            md.push_str(&format!(
                "Prerequisites: {}\n",
                wave.prerequisites.join(", ")
            ));
        }
        for wf in &wave.files {
            md.push_str(&format!(
                "- `{}` ({}, deps:{}, dependents:{})\n",
                wf.path, wf.estimated_complexity, wf.dependency_count, wf.dependent_count
            ));
        }
        if wave.strangler_fig_checkpoint {
            md.push_str("**Integration checkpoint after this wave.**\n");
        }
        md.push('\n');
    }

    if !order.circular_dependencies.is_empty() {
        md.push_str("### Circular Dependencies\n");
        for cycle in &order.circular_dependencies {
            md.push_str(&format!("- {}\n", cycle.join(" -> ")));
        }
        md.push('\n');
    }

    if !order.bottleneck_files.is_empty() {
        md.push_str("### Bottleneck Files\n");
        for bf in &order.bottleneck_files {
            md.push_str(&format!(
                "- `{}` blocks {} downstream: {}\n",
                bf.path, bf.blocks_count, bf.suggestion
            ));
        }
        md.push('\n');
    }

    // ── Cross-Cutting Concerns ────────────────────────────────────────────
    md.push_str("## Cross-Cutting Concerns\n\n");

    if !cross.shared_sql_tables.is_empty() {
        md.push_str("### Shared SQL Tables\n");
        for si in &cross.shared_sql_tables {
            md.push_str(&format!(
                "- **{}** used by: {}\n",
                si.name,
                si.used_by.join(", ")
            ));
        }
        md.push('\n');
    }

    if !cross.shared_state_keys.is_empty() {
        md.push_str("### Shared State Keys\n");
        for si in &cross.shared_state_keys {
            md.push_str(&format!(
                "- **{}** used by: {}\n",
                si.name,
                si.used_by.join(", ")
            ));
        }
        md.push('\n');
    }

    if !cross.shared_user_controls.is_empty() {
        md.push_str("### Shared User Controls\n");
        for si in &cross.shared_user_controls {
            md.push_str(&format!(
                "- **{}** used by: {}\n",
                si.name,
                si.used_by.join(", ")
            ));
        }
        md.push('\n');
    }

    // ── Page-by-Page Dossiers ─────────────────────────────────────────────
    md.push_str("## Page-by-Page Dossiers\n\n");

    for d in dossiers {
        let wave_num = wave_lookup.get(&d.file_path).copied().unwrap_or(0);

        md.push_str(&format!(
            "### {} (Wave {}, {}, Risk {}/10)\n\n",
            d.file_path, wave_num, d.estimated_complexity, d.blast_radius_score
        ));

        if let Some(ref cls) = d.inherits_class {
            md.push_str(&format!("**Class**: `{cls}`\n"));
        }
        if let Some(ref mp) = d.master_page {
            md.push_str(&format!("**Master**: `{mp}`\n"));
        }

        // Dependencies
        if !d.user_controls.is_empty() {
            md.push_str(&format!(
                "**User controls**: {}\n",
                d.user_controls
                    .iter()
                    .map(|uc| format!("`{}`", uc.control_path))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        // Data layer
        if !d.tables_touched.is_empty() {
            md.push_str(&format!("**Tables**: {}\n", d.tables_touched.join(", ")));
        }
        if !d.connection_strings_used.is_empty() {
            md.push_str(&format!(
                "**Connection strings**: {}\n",
                d.connection_strings_used.join(", ")
            ));
        }

        // Lifecycle
        let lc = &d.lifecycle_summary;
        if lc.lifecycle_event_count > 0 || lc.control_event_count > 0 {
            md.push_str(&format!(
                "**Lifecycle**: {} events, {} control events",
                lc.lifecycle_event_count, lc.control_event_count
            ));
            if lc.has_ispostback_logic {
                md.push_str(" (has IsPostBack)");
            }
            md.push('\n');
            if !lc.events.is_empty() {
                md.push_str(&format!("  Events: {}\n", lc.events.join(", ")));
            }
        }

        // ViewState
        let vs = &d.viewstate_summary;
        if vs.total_state_fields > 0 {
            md.push_str(&format!(
                "**ViewState**: {} explicit, {} implicit",
                vs.explicit_keys, vs.implicit_controls
            ));
            if let Some(ref hc) = vs.heaviest_control {
                md.push_str(&format!(" (heaviest: {hc})"));
            }
            md.push('\n');
        }

        // AJAX
        let aj = &d.ajax_summary;
        if aj.update_panel_count > 0 || aj.has_script_manager {
            md.push_str(&format!(
                "**AJAX**: {} UpdatePanels, {} timers, ScriptManager: {}\n",
                aj.update_panel_count, aj.timer_count, aj.has_script_manager
            ));
        }

        // Validation
        let vl = &d.validation_summary;
        if vl.validator_count > 0 || vl.custom_validator_count > 0 {
            md.push_str(&format!(
                "**Validation**: {} standard, {} custom, {} groups\n",
                vl.validator_count, vl.custom_validator_count, vl.validation_group_count
            ));
        }

        // Auth
        let au = &d.auth_summary;
        if au.has_auth_rules || au.auth_check_count > 0 || au.session_auth_count > 0 {
            md.push_str("**Auth**: ");
            if !au.required_roles.is_empty() {
                md.push_str(&format!("roles [{}] ", au.required_roles.join(", ")));
            }
            if au.auth_check_count > 0 {
                md.push_str(&format!("{} code checks ", au.auth_check_count));
            }
            if au.session_auth_count > 0 {
                md.push_str(&format!("{} session-auth patterns", au.session_auth_count));
            }
            md.push('\n');
        }

        // Phase 32: JS dependencies per page
        if let Some(js_deps) = js.page_js_dependencies.get(&d.file_path) {
            let mut dep_parts: Vec<String> = Vec::new();
            for js_file in js_deps {
                let dom_count = js
                    .dom_manipulations
                    .iter()
                    .filter(|dr| &dr.js_file == js_file)
                    .count();
                let pb_count = js
                    .postback_triggers
                    .iter()
                    .filter(|pr| &pr.js_file == js_file)
                    .count();
                let ajax_count = js
                    .ajax_calls
                    .iter()
                    .filter(|ac| &ac.js_file == js_file)
                    .count();
                let mut parts = Vec::new();
                if dom_count > 0 {
                    parts.push(format!("{dom_count} DOM refs"));
                }
                if pb_count > 0 {
                    parts.push(format!("{pb_count} postback"));
                }
                if ajax_count > 0 {
                    parts.push(format!("{ajax_count} AJAX"));
                }
                if parts.is_empty() {
                    dep_parts.push(js_file.clone());
                } else {
                    dep_parts.push(format!("{js_file} ({})", parts.join(", ")));
                }
            }
            md.push_str(&format!("**JS dependencies**: {}\n", dep_parts.join(", ")));
        }

        // Phase 32: GIS per page
        if gis.has_gis {
            let page_gis: Vec<&GisLibrarySummary> = gis
                .libraries_detected
                .iter()
                .filter(|l| l.files.iter().any(|f| f == &d.file_path))
                .collect();
            // Also check if any JS dependency of this page has GIS
            let js_has_gis = js
                .page_js_dependencies
                .get(&d.file_path)
                .map(|deps| deps.iter().any(|jf| gis.files_with_gis.contains(jf)))
                .unwrap_or(false);
            if !page_gis.is_empty() || js_has_gis {
                let lib_names: Vec<String> = page_gis
                    .iter()
                    .map(|l| {
                        let features = if l.features.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", l.features.join(", "))
                        };
                        format!("{}{features}", l.library)
                    })
                    .collect();
                let desc = if lib_names.is_empty() {
                    "via JS dependencies".to_string()
                } else {
                    lib_names.join(", ")
                };
                md.push_str(&format!(
                    "**GIS**: {} — complexity: {}\n",
                    desc, gis.migration_complexity
                ));
            }
        }

        // Phase 32: Anti-patterns per page
        let page_anti: Vec<&AntiPatternItem> = anti
            .critical_items
            .iter()
            .filter(|item| item.file_path == d.file_path)
            .collect();
        if !page_anti.is_empty() {
            let summaries: Vec<String> = page_anti
                .iter()
                .map(|a| format!("{} ({})", a.pattern_type, a.detail))
                .collect();
            md.push_str(&format!("**Anti-patterns**: {}\n", summaries.join("; ")));
        }

        // Risk factors
        if !d.risk_factors.is_empty() {
            md.push_str(&format!(
                "**Risk factors**: {}\n",
                d.risk_factors.join("; ")
            ));
        }

        // Migration steps
        if !d.migration_steps.is_empty() {
            md.push_str("**Migration steps**:\n");
            for (i, step) in d.migration_steps.iter().enumerate() {
                md.push_str(&format!("  {}. {step}\n", i + 1));
            }
        }

        md.push('\n');
    }

    // ── Risk Assessment ───────────────────────────────────────────────────
    md.push_str("## Risk Assessment\n\n");
    md.push_str("| Risk Band | Files |\n|-----------|-------|\n");
    for (band, count) in &cross.risk_distribution {
        md.push_str(&format!("| {band} | {count} |\n"));
    }
    md.push('\n');

    if !cross.critical_risk_files.is_empty() {
        md.push_str("**Critical-risk files requiring special attention:**\n");
        for f in &cross.critical_risk_files {
            md.push_str(&format!("- `{f}`\n"));
        }
    }

    md
}

/// Convert epoch days (since 1970-01-01) to (year, month, day).
fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm from Howard Hinnant
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_cutting_empty_dossiers() {
        let state = StateMigrationReport {
            project_id: "test".into(),
            recommendations: vec![],
            viewstate_report: None,
            summary: state_migration_service::StateMigrationSummary {
                total_state_keys: 0,
                by_store: BTreeMap::new(),
                by_target: BTreeMap::new(),
                high_risk_keys: vec![],
            },
        };
        let cross = build_cross_cutting_summary(
            &[],
            &state,
            &empty_js(),
            &empty_gis(),
            &empty_anti(),
            &empty_endpoints(),
            &empty_asp(),
            &empty_rpt(),
        );
        assert_eq!(cross.total_pages_analyzed, 0);
        assert!(cross.shared_sql_tables.is_empty());
        assert!(cross.shared_state_keys.is_empty());
        assert_eq!(cross.total_validators, 0);
    }

    #[test]
    fn shared_item_requires_two_files() {
        // Items used by only one file should NOT appear in shared lists
        let state = StateMigrationReport {
            project_id: "test".into(),
            recommendations: vec![],
            viewstate_report: None,
            summary: state_migration_service::StateMigrationSummary {
                total_state_keys: 0,
                by_store: BTreeMap::new(),
                by_target: BTreeMap::new(),
                high_risk_keys: vec![],
            },
        };

        let dossier1 = make_test_dossier("Page1.aspx", vec!["Users"], 5);
        let dossier2 = make_test_dossier("Page2.aspx", vec!["Users", "Orders"], 3);
        let dossier3 = make_test_dossier("Page3.aspx", vec!["Logs"], 2);

        let cross = build_cross_cutting_summary(
            &[dossier1, dossier2, dossier3],
            &state,
            &empty_js(),
            &empty_gis(),
            &empty_anti(),
            &empty_endpoints(),
            &empty_asp(),
            &empty_rpt(),
        );

        // "Users" appears in Page1 and Page2 → shared
        assert_eq!(cross.shared_sql_tables.len(), 1);
        assert_eq!(cross.shared_sql_tables[0].name, "Users");
        assert_eq!(cross.shared_sql_tables[0].used_by.len(), 2);

        // "Orders" and "Logs" appear in only one file → not shared
    }

    fn make_test_dossier(file_path: &str, tables: Vec<&str>, blast_radius: u8) -> MigrationDossier {
        MigrationDossier {
            file_path: file_path.to_string(),
            page_type: "aspx".to_string(),
            target_stack: "blazor".to_string(),
            inherits_class: None,
            base_class: None,
            codebehind_file: None,
            master_page: None,
            user_controls: vec![],
            referenced_files: vec![],
            referenced_by: vec![],
            shared_modules: vec![],
            data_sources: vec![],
            sql_statements: vec![],
            connection_strings_used: vec![],
            tables_touched: tables.into_iter().map(String::from).collect(),
            lifecycle_summary: dossier_service::LifecycleSummary {
                lifecycle_event_count: 0,
                control_event_count: 0,
                has_ispostback_logic: false,
                events: vec![],
            },
            viewstate_summary: dossier_service::ViewStateSummary {
                explicit_keys: 0,
                implicit_controls: 0,
                total_state_fields: 0,
                heaviest_control: None,
            },
            ajax_summary: dossier_service::AjaxSummary {
                update_panel_count: 0,
                timer_count: 0,
                has_script_manager: false,
                suggested_components: 0,
            },
            validation_summary: dossier_service::ValidationSummary {
                validator_count: 0,
                custom_validator_count: 0,
                validation_group_count: 0,
                has_validation_summary: false,
            },
            auth_summary: dossier_service::AuthSummary {
                has_auth_rules: false,
                required_roles: vec![],
                auth_check_count: 0,
                session_auth_count: 0,
            },
            blast_radius_score: blast_radius,
            risk_factors: vec![],
            scaffold_preview: None,
            migration_steps: vec![],
            estimated_complexity: "Medium".to_string(),
        }
    }

    fn empty_js() -> JsAnalysisSummary {
        JsAnalysisSummary {
            total_js_files: 0,
            js_files_with_server_deps: 0,
            dom_manipulations: vec![],
            postback_triggers: vec![],
            ajax_calls: vec![],
            page_js_dependencies: BTreeMap::new(),
            inline_script_files: vec![],
            jquery_version_hint: None,
        }
    }
    fn empty_gis() -> GisAnalysisSummary {
        GisAnalysisSummary {
            has_gis: false,
            libraries_detected: vec![],
            total_spatial_calls: 0,
            files_with_gis: vec![],
            migration_complexity: "none".into(),
            modern_targets: GisModernTargets {
                react: vec![],
                blazor: vec![],
                angular: vec![],
            },
        }
    }
    fn empty_anti() -> AntiPatternSummary {
        AntiPatternSummary {
            total_anti_patterns: 0,
            by_type: BTreeMap::new(),
            critical_items: vec![],
            migration_impact: vec![],
        }
    }
    fn empty_endpoints() -> ServiceEndpointSummary {
        ServiceEndpointSummary {
            web_services: vec![],
            http_handlers: vec![],
            wcf_services: vec![],
            http_modules: vec![],
            route_handlers: vec![],
            total_endpoints: 0,
        }
    }
    fn empty_asp() -> ClassicAspSummary {
        ClassicAspSummary {
            total_asp_files: 0,
            com_objects: vec![],
            ado_connections: 0,
            sql_statements: 0,
            includes: vec![],
            state_accesses: 0,
            migration_effort_hours: 0.0,
        }
    }
    fn empty_rpt() -> ReportSummary {
        ReportSummary {
            ssrs_reports: vec![],
            crystal_reports: vec![],
            total_reports: 0,
            has_binary_rpt_files: false,
            shared_data_sources: vec![],
        }
    }
}
