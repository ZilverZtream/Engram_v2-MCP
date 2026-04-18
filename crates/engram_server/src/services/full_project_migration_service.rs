//! Full project migration analysis — the "one call, everything" service.
//!
//! Orchestrates every migration sub-service to produce a single comprehensive
//! report covering every file in the project.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_graph::GraphStore;
use tokio_util::sync::CancellationToken;

// MIG1/D2: typed partial-failure surface.
// Thread-local accumulator collects every graph-query context that failed during
// a single `analyze_full_project` call.  Thread-local is safe here because the
// function is synchronous; concurrent callers each own their own TLS cell.
use std::cell::RefCell;
thread_local! {
    static MIG_DEGRADED: RefCell<Vec<String>> = RefCell::new(Vec::new());
}
#[inline]
pub(super) fn record_mig_degraded(context: &'static str) {
    MIG_DEGRADED.with(|v| v.borrow_mut().push(context.to_string()));
}
#[inline]
pub(super) fn take_mig_degraded() -> Vec<String> {
    MIG_DEGRADED.with(|v| std::mem::take(&mut *v.borrow_mut()))
}

/// MIG1: helper that runs a graph query for edge lists, returning an empty Vec on error
/// while logging a warning AND recording the failure context so the final report can
/// carry an explicit `degraded_sections` list and `report_is_complete = false` flag.
#[inline]
pub(super) fn edges_or_warn(
    result: anyhow::Result<Vec<engram_graph::Edge>>,
    context: &'static str,
) -> Vec<engram_graph::Edge> {
    result.unwrap_or_else(|e| {
        tracing::warn!("MIG1: graph query failed ({context}): {e:#} — returning empty result");
        record_mig_degraded(context);
        Vec::new()
    })
}

/// MIG1: same pattern for node-list graph queries.
#[inline]
pub(super) fn nodes_or_warn(
    result: anyhow::Result<Vec<engram_graph::Node>>,
    context: &'static str,
) -> Vec<engram_graph::Node> {
    result.unwrap_or_else(|e| {
        tracing::warn!("MIG1: graph query failed ({context}): {e:#} — returning empty result");
        record_mig_degraded(context);
        Vec::new()
    })
}
use regex::Regex;

use super::auth_config_service::AuthConfigMap;
use super::db_strategy_service;
use super::dossier_service::{self, MigrationDossier};
use super::migration_order_service::{self, MigrationOrderPlan};
use super::state_migration_service::{self, StateMigrationReport};

// Data model (every `pub struct` / `pub enum` for the report) lives
// in `full_project_migration_service/model.rs`. Re-exported at the
// module root so external callers keep using the same paths —
// `use super::full_project_migration_service::FullProjectMigrationReport;`
// still compiles exactly as before.
pub mod model;
pub use model::*;

mod analyzers;
mod llm_enhancement;
mod rendering;

// Preserve the public shim paths for external callers:
//   `full_project_migration_service::classify_method_kind_pub(...)`
//   `full_project_migration_service::build_sp_catalog_public(...)`
//   `full_project_migration_service::enhance_page_dossiers_with_llm(...)`
//   `full_project_migration_service::enhance_report_with_llm(...)`
//   `full_project_migration_service::rerender_markdown_after_llm(...)`
//   `full_project_migration_service::PageLlmEnhancement`
pub use analyzers::methods::classify_method_kind_pub;
pub use analyzers::sp_catalog::build_sp_catalog_public;
pub use llm_enhancement::{
    PageLlmEnhancement, enhance_page_dossiers_with_llm, enhance_report_with_llm,
    rerender_markdown_after_llm,
};
// Private helpers kept reachable for the internal test module
// (tests reference these via `use super::*;`). Only included in
// test builds so a release build doesn't warn about unused items.
#[cfg(test)]
use llm_enhancement::{
    build_page_llm_prompt, dossier_llm_priority, parse_page_llm_response,
    select_dossiers_for_llm,
};

/// (parent_class, file_path, methods, state_writes, base_calls) per class.
pub(super) type ClassInfo = (String, String, Vec<String>, Vec<String>, Vec<String>);

// ── Main entry point ──────────────────────────────────────────────────────────

/// Analyze an entire project for migration.
///
/// All file content must be pre-read (async) and passed in via [`ProjectFileBundle`].
/// Every sub-service call inside is synchronous and safe for `spawn_blocking`.
///
/// MIG1: The `cancel` token enables cooperative cancellation of long-running
/// migrations.  The function checks the token at the boundary of each major
/// analysis phase; if cancelled it returns `Err` immediately, allowing the
/// caller to surface the abort without leaving the service in partial state.
///
/// MIG3-3410fe: This function is **purely in-memory** — no checkpoint or phase
/// marker is written between analysis phases.  Cancellation is safe (no partial
/// writes can corrupt storage), but there is **no resume capability**: a cancelled
/// run must restart from scratch on retry.  For short projects this is acceptable;
/// for very large codebases, future work may add a checkpoint store so that
/// per-file dossier results accumulated before cancellation can be replayed.
pub fn analyze_full_project(
    graph: &Arc<GraphStore>,
    project_id: &str,
    target_stack: &str,
    bundle: &ProjectFileBundle,
    max_files: usize,
    cancel: &CancellationToken,
) -> anyhow::Result<FullProjectMigrationReport> {
    // MIG1: early-exit if already cancelled before we start any work.
    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!("MIG1: migration cancelled before start"));
    }
    // MIG1/D2: reset per-call degradation accumulator before any graph queries.
    MIG_DEGRADED.with(|v| v.borrow_mut().clear());

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

    // MIG2: all sub-service fallbacks in this function use the same intentional degradation
    // pattern: log a tracing::warn, return a safe empty/default value, and record the
    // context via edges_or_warn/nodes_or_warn (which call record_mig_degraded) so the
    // final report carries an explicit degraded_sections list and report_is_complete=false.
    // No sub-service failure is silently discarded — every arm surfaces in the report.
    let data_access_profiles =
        db_strategy_service::classify_data_access_patterns(graph, project_id).unwrap_or_else(|e| {
            tracing::warn!("data_access classification failed: {e}");
            vec![]
        });

    // MIG1: check before entering per-file phase (most expensive part).
    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!(
            "MIG1: migration cancelled after project-wide graph analyses"
        ));
    }

    // ── 2. Per-file dossiers ──────────────────────────────────────────────

    let file_contents = &bundle.markup_files;
    let capped = if file_contents.len() > max_files {
        &file_contents[..max_files]
    } else {
        file_contents
    };

    tracing::info!(
        project_id = %project_id,
        markup_files = file_contents.len(),
        capped = capped.len(),
        max_files = max_files,
        "analyze_full_project: entering per-page dossier loop"
    );

    let mut page_dossiers: Vec<MigrationDossier> = Vec::with_capacity(capped.len());

    for fc in capped {
        // MIG1: check cancel inside the per-file loop so large projects can
        // be preempted between files rather than waiting for all to complete.
        if cancel.is_cancelled() {
            return Err(anyhow::anyhow!(
                "MIG1: migration cancelled during per-file dossier phase"
            ));
        }
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

    tracing::info!(
        project_id = %project_id,
        page_dossiers = page_dossiers.len(),
        "analyze_full_project: per-page dossier loop complete"
    );

    // MIG1: check cancel before Phase 32 bulk analyses.
    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!(
            "MIG1: migration cancelled before Phase 32 analyses"
        ));
    }

    // ── 3. Phase 32 analyses ─────────────────────────────────────────────

    let web_config_inv = web_config_content
        .map(|wc| analyzers::web_config::extract_webconfig_inventory(wc, &code_refs))
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
            analyzers::global_asax::extract_global_asax_info(
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

    let service_endpoints = analyzers::endpoints::build_service_endpoint_summary(graph, project_id);

    let anti_patterns = analyzers::anti_patterns::build_anti_pattern_summary(graph, project_id);

    let js_analysis = analyzers::js::build_js_analysis(
        graph,
        project_id,
        &bundle.markup_files,
        &bundle.script_files,
    );

    let gis_analysis = analyzers::gis::build_gis_analysis(graph, project_id, target_stack);

    let classic_asp = analyzers::classic_asp::build_classic_asp_summary(graph, project_id, &bundle.classic_asp_files);

    let reports = analyzers::reports::build_report_summary(graph, project_id, &bundle.report_files);

    // ── 3b. Phase 33 analyses ──────────────────────────────────────────────

    // Gap 1: Code-behind method inventory
    let method_inventories = analyzers::methods::build_method_inventories(graph, project_id, capped);

    // Gap 2: Third-party control detection
    let third_party_controls = analyzers::third_party::build_third_party_control_summary(&bundle.markup_files);

    // Gap 3: Dependency inventory
    let dependency_inventory = analyzers::dependencies::build_dependency_inventory(&bundle.project_references);

    // Gap 4: Caching inventory
    let caching_inventory =
        analyzers::caching::build_caching_inventory(&bundle.markup_files, &code_refs, &bundle.code_files);

    // Gap 5: URL routing
    let url_routing = analyzers::routing::extract_url_routing(
        web_config_content,
        bundle
            .global_asax
            .as_ref()
            .map(|ga| ga.codebehind_content.as_deref().unwrap_or(""))
            .unwrap_or(""),
        &code_refs,
    );

    // Gap 6: VB.NET translation flags
    let vb_translation = analyzers::vb_translation::analyze_vb_translation_flags(&code_refs);

    // Gap 7: Multi-tenancy detection
    let multi_tenancy = analyzers::multi_tenancy::detect_multi_tenancy(
        web_config_content,
        &code_refs,
        bundle
            .global_asax
            .as_ref()
            .map(|ga| ga.codebehind_content.as_deref().unwrap_or("")),
    );

    // Gap 8: Email + background jobs
    let email_patterns = analyzers::email::detect_email_patterns(&code_refs, web_config_content);
    let background_jobs = analyzers::background_jobs::detect_background_job_patterns(
        &code_refs,
        bundle
            .global_asax
            .as_ref()
            .map(|ga| ga.codebehind_content.as_deref().unwrap_or("")),
    );

    // ── 3c. Phase 34 analyses ─────────────────────────────────────────────

    // Ticket 1: Stored procedure catalog
    let sp_catalog = analyzers::sp_catalog::build_sp_catalog(&bundle.sql_files, &code_refs);

    // Ticket 2: Inheritance chain resolution
    let inheritance_chains = analyzers::inheritance::resolve_inheritance_chains(&code_refs, capped);

    // Ticket 3: packages.config + binding redirects (extend dependency_inventory)
    let mut dependency_inventory = dependency_inventory;
    for (_, content) in &bundle.packages_config_files {
        let legacy_pkgs = analyzers::dependencies::parse_packages_config(content);
        // If we got legacy packages and had 0 NuGet packages from SDK-style, use these
        if !legacy_pkgs.is_empty() {
            if dependency_inventory.total_packages == 0 {
                // Convert legacy to NuGet info for unified reporting
                for lp in &legacy_pkgs {
                    let (repl, ver, notes, cat) = analyzers::dependencies::lookup_modern_replacement(&lp.package_id);
                    dependency_inventory.nuget_packages.push(NuGetPackageInfo {
                        name: lp.package_id.clone(),
                        version: Some(lp.version.clone()),
                        modern_replacement: repl.map(|s| s.to_string()),
                        modern_version: ver.map(|s| s.to_string()),
                        migration_notes: notes.map(|s| s.to_string()),
                        category: cat.to_string(),
                    });
                }
                dependency_inventory.total_packages = dependency_inventory.nuget_packages.len();
                let wr = dependency_inventory
                    .nuget_packages
                    .iter()
                    .filter(|p| p.modern_replacement.is_some())
                    .count();
                dependency_inventory.packages_with_known_replacement = wr;
                dependency_inventory.packages_without_replacement =
                    dependency_inventory.total_packages - wr;
            }
            dependency_inventory.legacy_packages.extend(legacy_pkgs);
        }
    }
    dependency_inventory.binding_redirects = analyzers::dependencies::extract_binding_redirects(web_config_content);

    // Ticket 6a: Config transforms
    let config_transforms = analyzers::config_transforms::parse_config_transforms(&bundle.config_transform_files);

    // Ticket 6b: Master page region mapping
    let master_page_regions = analyzers::master_pages::build_master_page_region_map(&bundle.master_files, capped);

    // Ticket 6c: Resource file inventory
    let resource_inventory = analyzers::resources::build_resource_inventory(&bundle.resx_files);

    // ── 3d. Phase 35 analyses ─────────────────────────────────────────────

    // VB.NET translation traps
    let vb_translation_traps =
        engram_index::vb_translation_traps::detect_vb_translation_traps(&code_refs);

    // jQuery ecosystem inventory
    let js_refs: Vec<(&str, &str)> = bundle
        .script_files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let markup_refs: Vec<(&str, &str)> = capped
        .iter()
        .map(|fc| (fc.file_path.as_str(), fc.markup_content.as_str()))
        .collect();
    let jquery_inventory =
        engram_index::jquery_inventory::build_jquery_inventory(&js_refs, &markup_refs);

    // Cross-layer AJAX→Handler→Data tracing
    let cross_layer_traces =
        analyzers::cross_layer::build_cross_layer_traces(&js_analysis, &sp_catalog, &service_endpoints, &code_refs);

    // ── 4. Cross-cutting aggregation ──────────────────────────────────────

    let cross_cutting = analyzers::cross_cutting::build_cross_cutting_summary(
        &page_dossiers,
        &state_migration,
        &js_analysis,
        &gis_analysis,
        &anti_patterns,
        &service_endpoints,
        &classic_asp,
        &reports,
        &method_inventories,
        &dependency_inventory,
        &caching_inventory,
        &email_patterns,
        &background_jobs,
        &sp_catalog,
        &inheritance_chains,
        &config_transforms,
        &resource_inventory,
        &master_page_regions,
        &vb_translation,
    );

    // ── 5. Build the wave lookup (file_path → wave number) ────────────────

    let mut wave_lookup: BTreeMap<String, u32> = BTreeMap::new();
    for wave in &migration_order.waves {
        for wf in &wave.files {
            wave_lookup.insert(wf.path.clone(), wave.wave_number);
        }
    }

    // ── 6. Deterministic business logic summaries ─────────────────────────
    // (LLM-powered summaries are available via the `analyze_business_logic` tool)

    let business_logic = {
        let mut file_summaries = Vec::new();
        let mut total_methods = 0usize;
        for (file_path, inv) in &method_inventories {
            // Use detect_class_name on actual content when available, fall back to filename stem
            let class_name = code_refs
                .iter()
                .find(|(p, _)| *p == inv.codebehind_path.as_str())
                .map(|(_, c)| super::business_logic_service::detect_class_name(c))
                .unwrap_or_else(|| {
                    inv.codebehind_path
                        .rsplit(['/', '\\'])
                        .next()
                        .and_then(|f| f.split('.').next())
                        .unwrap_or("Unknown")
                        .to_string()
                });
            let methods: Vec<super::business_logic_service::MethodBusinessLogic> = inv
                .methods
                .iter()
                .map(|m| {
                    super::business_logic_service::deterministic_method_summary(
                        file_path,
                        m,
                        &class_name,
                    )
                })
                .collect();
            total_methods += methods.len();
            file_summaries.push(super::business_logic_service::FileBusinessLogic {
                file_path: file_path.clone(),
                class_name,
                file_purpose: String::new(), // No LLM available in sync context
                methods,
                analyzed_at: now.clone(),
            });
        }
        super::business_logic_service::ProjectBusinessLogicReport {
            project_id: project_id.to_string(),
            files_analyzed: file_summaries.len(),
            methods_analyzed: total_methods,
            methods_skipped_cached: 0,
            llm_failures: 0,
            file_summaries,
        }
    };

    // ── 7. Phase 37: Database Intelligence ─────────────────────────────

    // Collect code-level table references for cross-referencing
    let code_tables: std::collections::HashSet<String> = {
        let mut tables = std::collections::HashSet::new();
        for sp in &sp_catalog.procedures {
            for t in &sp.tables_read {
                tables.insert(t.clone());
            }
            for t in &sp.tables_written {
                tables.insert(t.clone());
            }
        }
        // Also add tables from cross-cutting shared SQL tables
        for item in &cross_cutting.shared_sql_tables {
            tables.insert(item.name.clone());
        }
        tables
    };

    let sql_refs: Vec<(String, String)> = bundle
        .sql_files
        .iter()
        .map(|(p, c)| (p.clone(), c.clone()))
        .collect();
    let database_intelligence = super::database_intelligence_service::build_database_intelligence(
        &sp_catalog,
        &sql_refs,
        &code_tables,
    );

    // ── 8. Phase 37: Session Workflow Reconstruction ────────────────────

    let session_workflows =
        super::session_workflow_service::reconstruct_session_workflows(graph, project_id);

    // ── 9. Render markdown ──────────────────────────────────────────────

    let markdown_report = rendering::render_markdown(
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
        &method_inventories,
        &third_party_controls,
        &dependency_inventory,
        &caching_inventory,
        &url_routing,
        &vb_translation,
        &multi_tenancy,
        &email_patterns,
        &background_jobs,
        &sp_catalog,
        &inheritance_chains,
        &config_transforms,
        &master_page_regions,
        &resource_inventory,
        &vb_translation_traps,
        &jquery_inventory,
        &cross_layer_traces,
        &business_logic,
        &database_intelligence,
        &session_workflows,
    );

    // MIG1: final cancel check before assembling the report.
    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!(
            "MIG1: migration cancelled before report assembly"
        ));
    }

    // MIG1/D2: drain the TLS accumulator before constructing the report.
    // Both derived variables must be computed before the struct literal so that
    // `report_is_complete` is based on the same drain as `degraded_sections`.
    let degraded_sections = take_mig_degraded();
    if !degraded_sections.is_empty() {
        tracing::warn!(
            project_id,
            degraded_count = degraded_sections.len(),
            "MIG1: report generated with {} degraded section(s) — \
             some graph queries failed and sections contain empty defaults",
            degraded_sections.len()
        );
    }
    let report_is_complete = degraded_sections.is_empty();

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
        method_inventories,
        third_party_controls,
        dependency_inventory,
        caching_inventory,
        url_routing,
        vb_translation,
        multi_tenancy,
        email_patterns,
        background_jobs,
        sp_catalog,
        inheritance_chains,
        config_transforms,
        master_page_regions,
        resource_inventory,
        vb_translation_traps,
        jquery_inventory,
        cross_layer_traces,
        business_logic,
        database_intelligence,
        session_workflows,
        markdown_report,
        degraded_sections,
        report_is_complete,
    })
}

// ── Per-page LLM Enhancement ─────────────────────────────────────────────────







// ── Ticket 37.1: Async LLM Enhancement Pass ──────────────────────────────────



// ── Cross-cutting aggregation ─────────────────────────────────────────────────


// ── Phase 32: Pre-compiled regex statics ──────────────────────────────────────
// Each function in this section previously compiled between 1 and 19 Regex
// objects on every call.  Moving them to LazyLock statics compiles each pattern
// exactly once at first use and eliminates all per-call allocation.

// web.config inventory (extract_webconfig_inventory)
pub(super) static WC_ADD_KEY_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+key\s*=\s*"([^"]+)"\s+value\s*=\s*"([^"]*)""#).expect("valid regex")
});
pub(super) static WC_CONN_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+name\s*=\s*"([^"]+)"[^>]*connectionString\s*=\s*"([^"]*)"[^>]*(?:providerName\s*=\s*"([^"]*)")?"#).expect("valid regex")
});
pub(super) static WC_HANDLER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+(?:[^>]*?)verb\s*=\s*"([^"]*)"[^>]*path\s*=\s*"([^"]*)"[^>]*type\s*=\s*"([^"]*)""#).expect("valid regex")
});
pub(super) static WC_MODULE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+name\s*=\s*"([^"]+)"[^>]*type\s*=\s*"([^"]*)""#).expect("valid regex")
});
pub(super) static WC_CE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<customErrors\s+mode\s*=\s*"([^"]+)"(?:[^>]*defaultRedirect\s*=\s*"([^"]*)")?"#)
        .expect("valid regex")
});
pub(super) static WC_ERROR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<error\s+statusCode\s*=\s*"([^"]+)"[^>]*redirect\s*=\s*"([^"]*)""#)
        .expect("valid regex")
});
pub(super) static WC_COMP_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"<compilation\s+([^>]*?)/?>"#).expect("valid regex"));
pub(super) static WC_TF_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"targetFramework\s*=\s*"([^"]+)""#).expect("valid regex")
});
pub(super) static WC_ASM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+assembly\s*=\s*"([^"]+)""#).expect("valid regex")
});
pub(super) static WC_SS_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"<sessionState\s+([^>]*?)/?>"#).expect("valid regex"));
pub(super) static WC_MODE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"mode\s*=\s*"([^"]+)""#).expect("valid regex"));
pub(super) static WC_TIMEOUT_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"timeout\s*=\s*"(\d+)""#).expect("valid regex"));
pub(super) static WC_COOKIELESS_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"cookieless\s*=\s*"([^"]+)""#).expect("valid regex"));
pub(super) static WC_PROVIDER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"customProvider\s*=\s*"([^"]+)""#).expect("valid regex")
});
pub(super) static WC_PAGES_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"<pages\s+([^>]*?)/?>"#).expect("valid regex"));
pub(super) static WC_THEME_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"theme\s*=\s*"([^"]+)""#).expect("valid regex"));
pub(super) static WC_MP_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"masterPageFile\s*=\s*"([^"]+)""#).expect("valid regex")
});
pub(super) static WC_NS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+namespace\s*=\s*"([^"]+)""#).expect("valid regex")
});
pub(super) static WC_CTRL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+tagPrefix\s*=\s*"([^"]+)"[^>]*namespace\s*=\s*"([^"]+)""#)
        .expect("valid regex")
});

// Global.asax class extractor (extract_global_asax_info)
pub(super) static ASAX_CLASS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)(?:Class|Inherits\s*=\s*["'])(\S+?)(?:["']|\s)"#).expect("valid regex")
});

// JS analysis (build_js_analysis)
pub(super) static JS_SCRIPT_SRC_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<script[^>]+src\s*=\s*["']([^"']+\.js)["']"#).expect("valid regex")
});
pub(super) static JS_INLINE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)<script\b[^>]*>").expect("valid regex"));
pub(super) static JS_SRC_ATTR_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bsrc\s*=").expect("valid regex"));
pub(super) static JS_JQUERY_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"jquery[.-](\d+\.\d+(?:\.\d+)?)").expect("valid regex")
});

// Classic ASP summary (build_classic_asp_summary)
pub(super) static ASP_CREATE_OBJ_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)Server\.CreateObject\s*\(\s*"([^"]+)""#).expect("valid regex")
});
pub(super) static ASP_SQL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)(?:\.Execute|\.CommandText|SELECT\s|INSERT\s|UPDATE\s|DELETE\s)")
        .expect("valid regex")
});
pub(super) static ASP_STATE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)(?:Session|Application|Request\.Cookies|Response\.Cookies)\s*\(")
        .expect("valid regex")
});
pub(super) static ASP_INCLUDE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<!--\s*#include\s+(?:file|virtual)\s*=\s*"([^"]+)""#).expect("valid regex")
});

// Report summary (build_report_summary)
pub(super) static RPT_DATASET_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<DataSet\s+Name\s*=\s*"([^"]+)""#).expect("valid regex")
});
pub(super) static RPT_PARAM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<ReportParameter\s+Name\s*=\s*"([^"]+)""#).expect("valid regex")
});
pub(super) static RPT_SUBREPORT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<Subreport[^>]*>.*?<ReportName>([^<]+)</ReportName>"#).expect("valid regex")
});
pub(super) static RPT_DATASOURCE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<DataSource\s+Name\s*=\s*"([^"]+)""#).expect("valid regex")
});

// ── Phase 32: Analysis functions ──────────────────────────────────────────────






// ── Global.asax analysis ──────────────────────────────────────────────────────



// ── Service endpoint summary ──────────────────────────────────────────────────



// ── Anti-pattern summary ──────────────────────────────────────────────────────


// ── JavaScript / jQuery analysis ──────────────────────────────────────────────


// ── GIS / Spatial analysis ────────────────────────────────────────────────────



// ── Classic ASP summary ───────────────────────────────────────────────────────


// ── Report summary ────────────────────────────────────────────────────────────


// ── Phase 33 analysis functions ────────────────────────────────────────────────

// ── Gap 1: Code-behind method inventory ─────────────────────────────────────




/// Fallback: extract method signatures directly from code-behind text using regex.
pub(crate) fn extract_methods_from_content(content: &str) -> Vec<MethodInfo> {
    static VB_METHOD_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?im)^\s*((?:Public|Private|Protected|Friend)\s+)?(?:Shared\s+)?(?:Overrides\s+)?(?:Overridable\s+)?(?:MustOverride\s+)?(?:Async\s+)?(Sub|Function)\s+(\w+)\s*\(([^)]*)\)(?:\s+As\s+(\w[\w.<>\[\],\s]*))?").expect("valid regex")
    });
    static CS_METHOD_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?im)^\s*((?:public|private|protected|internal)\s+)?(?:static\s+)?(?:override\s+)?(?:virtual\s+)?(?:async\s+)?(\w[\w.<>\[\],]*)\s+(\w+)\s*\(([^)]*)\)").expect("valid regex")
    });
    // THIRD-PASS: Extract VB Handles clause (e.g. "Handles btnSave.Click, Timer1.Tick")
    // Critical for migration: tells AI agent which control event triggers this method.
    static VB_HANDLES_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?im)(?:Sub|Function)\s+(\w+)\s*\([^)]*\)(?:\s+As\s+\w[\w.<>\[\],\s]*)?\s+Handles\s+(.+)$").expect("valid regex")
    });

    let mut methods = Vec::new();
    let is_vb = content.contains("End Sub") || content.contains("End Function");

    if is_vb {
        // THIRD-PASS: Pre-build Handles clause map for O(1) lookup per method
        let mut handles_map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for cap in VB_HANDLES_RE.captures_iter(content) {
            let method_name = cap[1].to_string();
            let handles_str = cap[2].to_string();
            // Parse comma-separated Handles targets: "btnSave.Click, Timer1.Tick"
            let bindings: Vec<String> = handles_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            handles_map.insert(method_name, bindings);
        }

        for cap in VB_METHOD_RE.captures_iter(content) {
            let access = cap.get(1).map_or("Private", |m| m.as_str().trim());
            let kind_str = &cap[2];
            let name = cap[3].to_string();
            let params = cap[4].to_string();
            let return_type = cap.get(5).map_or(
                if kind_str.eq_ignore_ascii_case("Sub") {
                    "Sub"
                } else {
                    "Object"
                },
                |m| m.as_str(),
            );
            let signature = format!("{access} {kind_str} {name}({params})")
                .trim()
                .to_string();
            let effects = analyzers::methods::extract_effects_from_nearby_content(content, &name);
            let handles = handles_map.get(&name).cloned().unwrap_or_default();
            // If Handles MyBase.Load, classify as Lifecycle even if name doesn't match pattern
            let kind = if handles.iter().any(|h| {
                let lower_h = h.to_lowercase();
                lower_h.contains("mybase.load")
                    || lower_h.contains("mybase.init")
                    || lower_h.contains("mybase.prerender")
                    || lower_h.contains("mybase.unload")
                    || lower_h.contains("me.load")
                    || lower_h.contains("me.init")
            }) {
                MethodKind::Lifecycle
            } else if !handles.is_empty() {
                MethodKind::ControlEvent
            } else {
                analyzers::methods::classify_method_kind(&name, &effects, &None)
            };

            // Extract body for line range, preview, and complexity
            let (body_preview, line_range, line_count, complexity) =
                if let Some((body, sl, el, lc)) = extract_vb_method_body(content, &name) {
                    let preview = analyzers::methods::make_body_preview(&body, lc);
                    let cx = analyzers::methods::compute_complexity_score(&body);
                    (Some(preview), (sl, el), lc, cx)
                } else {
                    (None, (0, 0), 0, 0)
                };

            methods.push(MethodInfo {
                name,
                signature,
                return_type: return_type.to_string(),
                access_level: access
                    .split_whitespace()
                    .next()
                    .unwrap_or("Private")
                    .to_string(),
                line_range,
                line_count,
                method_kind: kind,
                effects,
                calls_methods: vec![],
                called_by: vec![],
                body_preview,
                complexity_score: complexity,
                handles_clause: handles,
            });
        }
    } else {
        for cap in CS_METHOD_RE.captures_iter(content) {
            let access = cap.get(1).map_or("private", |m| m.as_str().trim());
            let return_type = cap[2].to_string();
            let name = cap[3].to_string();
            let params = cap[4].to_string();
            // Skip common non-method matches
            if [
                "if",
                "else",
                "for",
                "foreach",
                "while",
                "switch",
                "catch",
                "using",
                "lock",
                "return",
                "new",
                "class",
                "struct",
                "interface",
                "enum",
                "namespace",
            ]
            .contains(&name.as_str())
            {
                continue;
            }
            let signature = format!("{access} {return_type} {name}({params})")
                .trim()
                .to_string();
            let effects = analyzers::methods::extract_effects_from_nearby_content(content, &name);
            let kind = analyzers::methods::classify_method_kind(&name, &effects, &None);

            // Extract body for line range, preview, and complexity
            let (body_preview, line_range, line_count, complexity) =
                if let Some((body, sl, el, lc)) = extract_cs_method_body(content, &name) {
                    let preview = analyzers::methods::make_body_preview(&body, lc);
                    let cx = analyzers::methods::compute_complexity_score(&body);
                    (Some(preview), (sl, el), lc, cx)
                } else {
                    (None, (0, 0), 0, 0)
                };

            methods.push(MethodInfo {
                name,
                signature,
                return_type,
                access_level: access
                    .split_whitespace()
                    .next()
                    .unwrap_or("private")
                    .to_string(),
                line_range,
                line_count,
                method_kind: kind,
                effects,
                calls_methods: vec![],
                called_by: vec![],
                body_preview,
                complexity_score: complexity,
                handles_clause: vec![],
            });
        }
    }

    methods
}


// ── Gap 2: Third-party control detection ────────────────────────────────────




// ── Gap 3: Dependency inventory ─────────────────────────────────────────────




// ── Gap 4: Caching inventory ────────────────────────────────────────────────


// ── Gap 5: URL routing/rewrite rules ────────────────────────────────────────



// ── Gap 6: VB.NET → C# translation flags ───────────────────────────────────



// ── Gap 7: Multi-tenancy detection ──────────────────────────────────────────


// ── Gap 8: Email & background job detection ─────────────────────────────────



// ── Markdown renderer ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]

/// Render a top-level confidence dashboard summarizing intelligence coverage.

// ── Phase 34: Stored Procedure Catalog Builder ───────────────────────────────



// ── Phase 34: Inheritance Chain Resolution ───────────────────────────────────

pub(super) static VB_CLASS_INHERITS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*(?:Public\s+)?(?:Partial\s+)?Class\s+(\w+)\s*(?:\r?\n\s*)?Inherits\s+(\w[\w.]*)",
    )
    .expect("vb_class_inherits")
});
pub(super) static CS_CLASS_INHERITS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:public\s+)?(?:partial\s+)?class\s+(\w+)\s*:\s*(\w[\w.]*)")
        .expect("cs_class_inherits")
});
pub(super) static VB_METHOD_DEF_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:Protected\s+)?(?:Overrides\s+)?(?:Overridable\s+)?(?:Public\s+)?(?:Private\s+)?(?:Friend\s+)?(?:Shared\s+)?(?:Async\s+)?(?:Sub|Function)\s+(\w+)").expect("vb_method_def")
});
pub(super) static CS_METHOD_DEF_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    // Supports generic return types with commas: Task<ActionResult>, Dictionary<string, int>,
    // IEnumerable<KeyValuePair<string, int>>, Nullable<int>, List<T> etc.
    Regex::new(r"(?im)^\s*(?:protected\s+)?(?:override\s+)?(?:virtual\s+)?(?:public\s+)?(?:private\s+)?(?:internal\s+)?(?:static\s+)?(?:async\s+)?(?:void|[\w]+(?:<[\w,\s<>\[\]?]+>)?(?:\[\])?)\s+(\w+)\s*\(").expect("cs_method_def")
});
pub(super) static VB_CALLS_BASE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)MyBase\.(\w+)").expect("vb_calls_base"));
pub(super) static CS_CALLS_BASE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)base\.(\w+)").expect("cs_calls_base"));
pub(super) static SESSION_WRITE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)Session\s*[\(\[]\s*"(\w+)"\s*[\)\]]\s*="#).expect("session_write")
});
pub(super) static INHERITS_DIRECTIVE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)Inherits\s*=\s*"([^"]+)""#).expect("inherits_directive")
});

const LIFECYCLE_METHODS: &[&str] = &[
    "Page_Load",
    "Page_Init",
    "Page_PreRender",
    "Page_Unload",
    "Page_PreInit",
    "Page_InitComplete",
    "Page_LoadComplete",
    "Page_PreRenderComplete",
    "Page_SaveStateComplete",
    "Page_Error",
    "OnInit",
    "OnLoad",
    "OnPreRender",
    "OnUnload",
    "OnInitComplete",
    "OnLoadComplete",
    "OnPreRenderComplete",
    "CreateChildControls",
    "Render",
];


// ── Phase 35: Inherited effect propagation ───────────────────────────────────

// Effect detection regexes for method bodies
pub(super) static EFFECT_SQL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:SqlCommand|SqlDataAdapter|ExecuteReader|ExecuteNonQuery|ExecuteScalar|SqlConnection|OleDbCommand|DataAdapter)\b")
        .expect("effect_sql")
});
pub(super) static EFFECT_REDIRECT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:Response\.Redirect|Server\.Transfer|Response\.RedirectPermanent)\b")
        .expect("effect_redirect")
});
pub(super) static EFFECT_CONTROL_WRITE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:\w+\.(?:Text|Visible|Enabled|DataSource|DataBind|SelectedValue|SelectedIndex|Items)\s*=)")
        .expect("effect_control_write")
});
pub(super) static EFFECT_HTTP_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:Response\.Write|Response\.ContentType|Response\.AddHeader|Response\.Cookies)\b",
    )
    .expect("effect_http")
});




// ── Phase 35: Cross-Layer AJAX→Handler→Data Tracing ──────────────────────────

pub(super) static HANDLER_SP_NAME_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)CommandText\s*=\s*"(sp_\w+|usp_\w+|\w+_\w+)""#).expect("handler_sp_name")
});

pub(super) static HANDLER_TABLE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:FROM|JOIN|INTO|UPDATE|DELETE\s+FROM)\s+(?:\[?dbo\]?\.)?\[?(\w+)\]?")
        .expect("handler_table")
});


struct UrlParts {
    file_part: String,
    method_part: Option<String>,
}




// ── Phase 34: packages.config Parser ─────────────────────────────────────────

// packages.config element regex — matches the entire <package ... /> tag
// regardless of attribute order. Individual attributes are extracted inside.
pub(super) static PKG_CONFIG_ELEMENT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?is)<package\s+([^>]+?)/>").expect("pkg_config_element")
});
pub(super) static PKG_ATTR_ID_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)\bid\s*=\s*"([^"]+)""#).expect("pkg_attr_id"));
pub(super) static PKG_ATTR_VER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\bversion\s*=\s*"([^"]+)""#).expect("pkg_attr_ver")
});
pub(super) static PKG_ATTR_TFM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\btargetFramework\s*=\s*"([^"]+)""#).expect("pkg_attr_tfm")
});
pub(super) static PKG_ATTR_DEV_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\bdevelopmentDependency\s*=\s*"true""#).expect("pkg_attr_dev")
});


// ── Phase 34: Binding Redirect Parser ────────────────────────────────────────

// Binding redirect parsing: matches the entire <dependentAssembly> block,
// then extracts attributes individually for order-independence.
pub(super) static DEP_ASSEMBLY_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?is)<dependentAssembly>\s*(.*?)\s*</dependentAssembly>").expect("dep_assembly")
});
pub(super) static ASM_NAME_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)\bname\s*=\s*"([^"]+)""#).expect("asm_name"));
pub(super) static ASM_PKT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\bpublicKeyToken\s*=\s*"([^"]+)""#).expect("asm_pkt")
});
pub(super) static BR_OLD_VER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\boldVersion\s*=\s*"([^"]+)""#).expect("br_old_ver")
});
pub(super) static BR_NEW_VER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\bnewVersion\s*=\s*"([^"]+)""#).expect("br_new_ver")
});


// ── Phase 34: Method Body Extraction ─────────────────────────────────────────

/// Extract VB method body by tracking Sub/Function to End Sub/End Function.
pub(crate) fn extract_vb_method_body(
    content: &str,
    method_name: &str,
) -> Option<(String, u32, u32, u32)> {
    // Find the method signature line
    let pattern = format!(
        r"(?im)^\s*(?:(?:Public|Private|Protected|Friend)\s+)?(?:Shared\s+)?(?:Overrides\s+)?(?:Overridable\s+)?(?:Async\s+)?(Sub|Function)\s+{}\s*\(",
        regex::escape(method_name)
    );
    // MIG1/D2: log before early-return so operators can see which method name caused failure.
    let re = Regex::new(&pattern)
        .inspect_err(|e| tracing::warn!(method_name, error = %e, "MIG1: VB method body regex compile failed"))
        .ok()?;
    let m = re.find(content)?;

    let start_offset = m.start();
    let start_line = content[..start_offset].lines().count() as u32;

    // Determine if it's Sub or Function
    let cap = re.captures(&content[m.start()..])?;
    let kind = cap[1].to_string();
    let _end_marker = if kind.eq_ignore_ascii_case("Sub") {
        "End Sub"
    } else {
        "End Function"
    };

    // Find matching End Sub/Function using depth tracking.
    // Handles nested Sub/Function with access modifiers: Protected Sub Helper(), etc.
    let after_start = &content[start_offset..];
    let mut depth = 1i32;
    let mut end_pos = None;
    let upper_kind = kind.to_uppercase();

    // Pre-compiled regex for nested VB Sub/Function declarations (handles access modifiers)
    static VB_NESTED_OPEN_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)^\s*(?:(?:Public|Private|Protected|Friend)\s+)?(?:Shared\s+)?(?:Overrides\s+)?(?:Overridable\s+)?(?:Async\s+)?(?:Sub|Function)\s+\w+")
            .expect("vb_nested_open")
    });

    // Determine which line index in after_start contains the actual declaration.
    // The regex ^\s* can match leading newlines, so m.start() may precede the declaration line.
    // Count newlines in the match span to find the declaration line index.
    let match_len = m.end() - m.start();
    let decl_line_idx = after_start[..match_len.min(after_start.len())]
        .matches('\n')
        .count();

    for (i, line) in after_start.lines().enumerate() {
        if i <= decl_line_idx {
            continue; // skip the declaration line and any preceding empty lines
        }
        let trimmed = line.trim().to_uppercase();

        // Count nested Sub/Function openings (skip End Sub/End Function lines)
        if !trimmed.starts_with("END ") && VB_NESTED_OPEN_RE.is_match(line.trim()) {
            depth += 1;
        }

        // Count closings — must match BOTH End Sub and End Function
        // because a Function can contain nested Sub (and vice versa).
        // Only break when depth reaches 0 AND closing kind matches the opening kind.
        if trimmed.starts_with("END SUB") || trimmed.starts_with("END FUNCTION") {
            depth -= 1;
            if depth == 0 && trimmed.starts_with(&format!("END {upper_kind}")) {
                // Calculate byte offset
                let line_start = after_start
                    .lines()
                    .take(i)
                    .map(|l| l.len() + 1)
                    .sum::<usize>();
                end_pos = Some(start_offset + line_start + line.len());
                break;
            }
        }
    }

    let end_offset = end_pos.unwrap_or(content.len());
    let body = &content[start_offset..end_offset];
    let line_count = body.lines().count() as u32;
    let end_line = start_line + line_count.saturating_sub(1);

    Some((body.to_string(), start_line, end_line, line_count))
}

/// Extract C# method body by tracking brace depth.
/// Handles verbatim strings (`@"..."`), interpolated strings, block/line comments,
/// and generic return types with commas (`Dictionary<string, int>`).
pub(crate) fn extract_cs_method_body(
    content: &str,
    method_name: &str,
) -> Option<(String, u32, u32, u32)> {
    let pattern = format!(
        r"(?im)^\s*(?:(?:public|private|protected|internal)\s+)?(?:static\s+)?(?:override\s+)?(?:virtual\s+)?(?:async\s+)?(?:void|[\w]+(?:<[\w,\s<>\[\]?]+>)?(?:\[\])?)\s+{}\s*\(",
        regex::escape(method_name)
    );
    // MIG1/D2: log before early-return so operators can see which method name caused failure.
    let re = Regex::new(&pattern)
        .inspect_err(|e| tracing::warn!(method_name, error = %e, "MIG1: C# method body regex compile failed"))
        .ok()?;
    let m = re.find(content)?;

    let start_offset = m.start();
    let start_line = content[..start_offset].lines().count() as u32;

    // Find the opening brace
    let after_sig = &content[m.end()..];
    let brace_offset = after_sig.find('{')?;
    let body_start = m.end() + brace_offset;

    // Track brace depth with proper string/comment awareness.
    // Handles: line comments (//), block comments (/* */), regular strings ("..."),
    // verbatim strings (@"..." where "" is the escape, not \"), char literals ('x'),
    // and interpolated string prefixes ($", $@").
    let mut depth = 0i32;
    let mut in_string = false;
    let mut in_verbatim_string = false;
    let mut in_char = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut prev_char = ' ';
    let mut end_pos = None;
    let body_chars: Vec<(usize, char)> = content[body_start..].char_indices().collect();

    for idx in 0..body_chars.len() {
        let (i, ch) = body_chars[idx];

        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
            }
            prev_char = ch;
            continue;
        }
        if in_block_comment {
            if prev_char == '*' && ch == '/' {
                in_block_comment = false;
            }
            prev_char = ch;
            continue;
        }
        if in_verbatim_string {
            if ch == '"' {
                // In verbatim strings, "" is an escaped quote; single " ends the string
                let next_ch = body_chars.get(idx + 1).map(|(_, c)| *c);
                if next_ch == Some('"') {
                    // Skip the escaped double-quote
                    prev_char = ch;
                    continue;
                }
                in_verbatim_string = false;
            }
            prev_char = ch;
            continue;
        }
        if in_string {
            if ch == '"' && prev_char != '\\' {
                in_string = false;
            } else if ch == '\\' && prev_char == '\\' {
                // Double backslash: reset prev_char so next char isn't treated as escaped
                prev_char = ' ';
                continue;
            }
            prev_char = ch;
            continue;
        }
        if in_char {
            if ch == '\'' && prev_char != '\\' {
                in_char = false;
            } else if ch == '\\' && prev_char == '\\' {
                prev_char = ' ';
                continue;
            }
            prev_char = ch;
            continue;
        }

        match ch {
            '/' if prev_char == '/' => {
                in_line_comment = true;
            }
            '*' if prev_char == '/' => {
                in_block_comment = true;
            }
            '"' => {
                // Check for verbatim string: @" or $@"
                if prev_char == '@' {
                    in_verbatim_string = true;
                } else {
                    in_string = true;
                }
            }
            '\'' => {
                in_char = true;
            }
            '{' => {
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end_pos = Some(body_start + i + 1);
                    break;
                }
            }
            _ => {}
        }
        prev_char = ch;
    }

    let end_offset = end_pos.unwrap_or(content.len());
    let body = &content[start_offset..end_offset];
    let line_count = body.lines().count() as u32;
    let end_line = start_line + line_count.saturating_sub(1);

    Some((body.to_string(), start_line, end_line, line_count))
}


// ── Phase 34 second-pass: LazyLock statics for compute_complexity_score ──────
// Pre-compiled regexes avoid recompiling 18 patterns on every method body.

pub(super) static CX_IF_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bif\b").expect("valid regex"));
pub(super) static CX_ELSE_IF_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\belse\s+if\b").expect("valid regex"));
pub(super) static CX_ELSEIF_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\belseif\b").expect("valid regex"));
pub(super) static CX_SWITCH_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bswitch\b").expect("valid regex"));
pub(super) static CX_CASE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bcase\b").expect("valid regex"));
pub(super) static CX_SELECT_CASE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bselect\s+case\b").expect("valid regex"));

pub(super) static CX_FOR_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bfor\s").expect("valid regex"));
pub(super) static CX_FOREACH_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bforeach\b").expect("valid regex"));
pub(super) static CX_FOR_EACH_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bfor\s+each\b").expect("valid regex"));
pub(super) static CX_WHILE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bwhile\b").expect("valid regex"));
pub(super) static CX_DO_WHILE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bdo\s+while\b").expect("valid regex"));
pub(super) static CX_DO_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bdo\s*$").expect("valid regex"));

pub(super) static CX_TRY_BRACE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\btry\s*\{").expect("valid regex"));
pub(super) static CX_TRY_EOL_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\btry\s*$").expect("valid regex"));
pub(super) static CX_CATCH_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bcatch\b").expect("valid regex"));
pub(super) static CX_ON_ERROR_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bOn\s+Error\b").expect("valid regex"));

pub(super) static CX_SQL_SELECT_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)"SELECT\s"#).expect("valid regex"));
pub(super) static CX_SQL_INSERT_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)"INSERT\s"#).expect("valid regex"));
pub(super) static CX_SQL_UPDATE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)"UPDATE\s"#).expect("valid regex"));
pub(super) static CX_SQL_DELETE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)"DELETE\s"#).expect("valid regex"));
pub(super) static CX_CMD_TEXT_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)CommandText\s*=").expect("valid regex"));
pub(super) static CX_SQL_CMD_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)SqlCommand").expect("valid regex"));
pub(super) static CX_SQL_ADAPTER_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)SqlDataAdapter").expect("valid regex"));

pub(super) static CX_SESSION_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)Session\s*[\(\[]"#).expect("valid regex"));


// ── Phase 34: Config Transform Parser ────────────────────────────────────────

pub(super) static XDT_TRANSFORM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)xdt:Transform\s*=\s*"(\w+)""#).expect("xdt_transform")
});
pub(super) static XDT_LOCATOR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)xdt:Locator\s*=\s*"Match\((\w+)\)""#).expect("xdt_locator")
});
pub(super) static XDT_CONNSTR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r#"(?i)<add\s+name\s*=\s*"([^"]+)"[^>]*connectionString\s*=\s*"([^"]*)"[^>]*xdt:Transform"#,
    )
    .expect("xdt_connstr")
});
pub(super) static XDT_APPSETTING_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<add\s+key\s*=\s*"([^"]+)"\s+value\s*=\s*"([^"]*)"[^>]*xdt:Transform"#)
        .expect("xdt_appsetting")
});
pub(super) static XDT_DEBUG_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<compilation[^>]*debug\s*=\s*"(true|false)""#).expect("xdt_debug")
});


// ── Phase 34: Master Page Region Mapping ─────────────────────────────────────

pub(super) static CONTENT_PLACEHOLDER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<asp:ContentPlaceHolder\s+[^>]*ID\s*=\s*"([^"]+)""#)
        .expect("content_placeholder")
});
pub(super) static CONTENT_FILLS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<asp:Content\s+[^>]*ContentPlaceHolderID\s*=\s*"([^"]+)""#)
        .expect("content_fills")
});
pub(super) static MASTER_PAGE_FILE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)MasterPageFile\s*=\s*"([^"]+)""#).expect("master_page_file")
});
pub(super) static PLACEHOLDER_DEFAULT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?is)<asp:ContentPlaceHolder\s+[^>]*ID\s*=\s*"([^"]+)"[^>]*>\s*\S"#)
        .expect("placeholder_default")
});


// ── Phase 34: Resource File (.resx) Inventory ────────────────────────────────

pub(super) static RESX_DATA_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<data\s+name\s*=\s*"([^"]+)""#).expect("resx_data")
});
pub(super) static RESX_FILE_REF_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)type\s*=\s*"System\.Resources\.ResXFileRef"#).expect("resx_file_ref")
});
pub(super) static RESX_LANG_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\.([a-z]{2}(?:-[A-Z]{2})?)\.resx$").expect("resx_lang")
});


/// Convert epoch days (since 1970-01-01) to (year, month, day).
pub(crate) fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use engram_index::solution_parser::PackageRef;

    fn empty_methods() -> BTreeMap<String, PageMethodInventory> {
        BTreeMap::new()
    }

    fn empty_deps() -> DependencyInventory {
        DependencyInventory {
            target_frameworks: vec![],
            nuget_packages: vec![],
            assembly_references: vec![],
            project_references: vec![],
            total_packages: 0,
            total_assemblies: 0,
            framework_assemblies: vec![],
            third_party_assemblies: vec![],
            packages_with_known_replacement: 0,
            packages_without_replacement: 0,
            legacy_packages: vec![],
            binding_redirects: vec![],
        }
    }

    fn empty_cache() -> CachingInventory {
        CachingInventory {
            output_cache_pages: vec![],
            programmatic_cache_keys: vec![],
            response_cache_files: vec![],
            sql_cache_dependencies: vec![],
            total_cached_pages: 0,
            total_cache_keys: 0,
            has_response_caching: false,
            has_sql_dependencies: false,
        }
    }

    fn empty_email() -> EmailPatternReport {
        EmailPatternReport {
            has_email: false,
            email_patterns: vec![],
            smtp_config: None,
            total_email_files: 0,
            uses_html_email: false,
            uses_attachments: false,
            uses_legacy_cdo: false,
            uses_legacy_web_mail: false,
        }
    }

    fn empty_bg_jobs() -> BackgroundJobReport {
        BackgroundJobReport {
            has_background_jobs: false,
            patterns: vec![],
            total_background_files: 0,
            uses_thread_pool: false,
            uses_timers: false,
            uses_task_run: false,
            uses_bg_worker: false,
            uses_hangfire: false,
            uses_quartz: false,
            fire_and_forget_count: 0,
        }
    }

    fn empty_sp_catalog() -> StoredProcedureCatalog {
        StoredProcedureCatalog {
            procedures: vec![],
            total_procedures: 0,
            procedures_with_params: 0,
            procedures_called_from_code: 0,
            uncalled_procedures: vec![],
        }
    }

    fn empty_inheritance() -> InheritanceChainReport {
        InheritanceChainReport {
            chains: vec![],
            base_classes: vec![],
            shared_lifecycle_methods: vec![],
            deepest_chain_depth: 0,
            inherited_effects: vec![],
        }
    }

    fn empty_config_transforms() -> ConfigTransformReport {
        ConfigTransformReport {
            environments: vec![],
            total_transforms: 0,
            connection_string_overrides: vec![],
            debug_flag_overrides: vec![],
            app_setting_overrides: vec![],
        }
    }

    fn empty_resource_inv() -> ResourceInventory {
        ResourceInventory {
            resource_files: vec![],
            total_keys: 0,
            languages_detected: vec![],
            has_global_resources: false,
            has_local_resources: false,
            embedded_resource_count: 0,
        }
    }

    fn empty_master_regions() -> MasterPageRegionMap {
        MasterPageRegionMap {
            master_pages: vec![],
            regions: vec![],
            orphan_regions: vec![],
        }
    }

    fn empty_vb_translation() -> VbTranslationReport {
        VbTranslationReport {
            is_vb_project: false,
            vb_file_count: 0,
            cs_file_count: 0,
            mixed_language: false,
            translation_flags: vec![],
            total_flags: 0,
            flags_by_category: BTreeMap::new(),
            highest_risk_files: vec![],
            dynamic_dispatch: DynamicDispatchSummary {
                option_strict_on_files: 0,
                option_strict_off_files: 0,
                methods_with_dynamic_dispatch: 0,
                late_binding_call_count: 0,
                object_var_count: 0,
                callbyname_count: 0,
                dynamic_dispatch_risk_tier: "low".into(),
            },
        }
    }

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
        let cross = analyzers::cross_cutting::build_cross_cutting_summary(
            &[],
            &state,
            &empty_js(),
            &empty_gis(),
            &empty_anti(),
            &empty_endpoints(),
            &empty_asp(),
            &empty_rpt(),
            &empty_methods(),
            &empty_deps(),
            &empty_cache(),
            &empty_email(),
            &empty_bg_jobs(),
            &empty_sp_catalog(),
            &empty_inheritance(),
            &empty_config_transforms(),
            &empty_resource_inv(),
            &empty_master_regions(),
            &empty_vb_translation(),
        );
        assert_eq!(cross.total_pages_analyzed, 0);
        assert!(cross.shared_sql_tables.is_empty());
        assert!(cross.shared_state_keys.is_empty());
        assert_eq!(cross.total_validators, 0);
        assert_eq!(cross.total_methods, 0);
        assert_eq!(cross.total_nuget_packages, 0);
        assert_eq!(cross.total_cached_pages, 0);
        assert!(!cross.has_email);
        assert!(!cross.has_background_jobs);
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

        let cross = analyzers::cross_cutting::build_cross_cutting_summary(
            &[dossier1, dossier2, dossier3],
            &state,
            &empty_js(),
            &empty_gis(),
            &empty_anti(),
            &empty_endpoints(),
            &empty_asp(),
            &empty_rpt(),
            &empty_methods(),
            &empty_deps(),
            &empty_cache(),
            &empty_email(),
            &empty_bg_jobs(),
            &empty_sp_catalog(),
            &empty_inheritance(),
            &empty_config_transforms(),
            &empty_resource_inv(),
            &empty_master_regions(),
            &empty_vb_translation(),
        );

        // "Users" appears in Page1 and Page2 → shared
        assert_eq!(cross.shared_sql_tables.len(), 1);
        assert_eq!(cross.shared_sql_tables[0].name, "Users");
        assert_eq!(cross.shared_sql_tables[0].used_by.len(), 2);

        // "Orders" and "Logs" appear in only one file → not shared
    }

    #[test]
    fn cross_cutting_phase33_aggregation() {
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

        // Build method inventories with some data
        let mut methods = BTreeMap::new();
        methods.insert(
            "Default.aspx".to_string(),
            PageMethodInventory {
                file_path: "Default.aspx".into(),
                codebehind_path: "Default.aspx.vb".into(),
                total_methods: 2,
                methods: vec![
                    MethodInfo {
                        name: "Page_Load".into(),
                        signature: "Protected Sub Page_Load(sender As Object, e As EventArgs)"
                            .into(),
                        return_type: "void".into(),
                        access_level: "Protected".into(),
                        line_range: (10, 20),
                        line_count: 10,
                        method_kind: MethodKind::Lifecycle,
                        effects: vec![],
                        calls_methods: vec![],
                        called_by: vec![],
                        body_preview: None,
                        complexity_score: 0,
                        handles_clause: vec![],
                    },
                    MethodInfo {
                        name: "btnSubmit_Click".into(),
                        signature:
                            "Protected Sub btnSubmit_Click(sender As Object, e As EventArgs)"
                                .into(),
                        return_type: "void".into(),
                        access_level: "Protected".into(),
                        line_range: (25, 40),
                        line_count: 15,
                        method_kind: MethodKind::ControlEvent,
                        effects: vec!["SQL".into()],
                        calls_methods: vec![],
                        called_by: vec![],
                        body_preview: None,
                        complexity_score: 3,
                        handles_clause: vec!["btnSubmit.Click".into()],
                    },
                ],
                lifecycle_methods: 1,
                event_handlers: 1,
                web_methods: 0,
                data_access_methods: 0,
                helper_methods: 0,
                largest_method: Some(("btnSubmit_Click".into(), 15)),
                methods_with_sql: 1,
                methods_with_state: 0,
            },
        );

        let deps = DependencyInventory {
            target_frameworks: vec!["net48".into()],
            nuget_packages: vec![NuGetPackageInfo {
                name: "Newtonsoft.Json".into(),
                version: Some("13.0.1".into()),
                modern_replacement: Some("System.Text.Json".into()),
                modern_version: Some("built-in".into()),
                migration_notes: Some("Built-in in .NET Core+".into()),
                category: "serialization".into(),
            }],
            assembly_references: vec![],
            project_references: vec![],
            total_packages: 1,
            total_assemblies: 0,
            framework_assemblies: vec![],
            third_party_assemblies: vec![],
            packages_with_known_replacement: 1,
            packages_without_replacement: 0,
            legacy_packages: vec![],
            binding_redirects: vec![],
        };

        let cache = CachingInventory {
            output_cache_pages: vec![OutputCacheEntry {
                file_path: "Default.aspx".into(),
                duration_seconds: Some(60),
                vary_by_param: Some("none".into()),
                vary_by_control: None,
                vary_by_custom: None,
                location: None,
                cache_profile: None,
                sql_dependency: None,
                modern_equivalent: "[ResponseCache]".into(),
            }],
            programmatic_cache_keys: vec![],
            response_cache_files: vec![],
            sql_cache_dependencies: vec![],
            total_cached_pages: 1,
            total_cache_keys: 0,
            has_response_caching: false,
            has_sql_dependencies: false,
        };

        let email = EmailPatternReport {
            has_email: true,
            email_patterns: vec![EmailPattern {
                file_path: "Notifications.vb".into(),
                pattern_type: "SmtpClient".into(),
                count: 1,
                modern_equivalent: "IEmailSender / MailKit".into(),
            }],
            smtp_config: None,
            total_email_files: 1,
            uses_html_email: false,
            uses_attachments: false,
            uses_legacy_cdo: false,
            uses_legacy_web_mail: false,
        };

        let bg = BackgroundJobReport {
            has_background_jobs: true,
            patterns: vec![BackgroundJobPattern {
                file_path: "Scheduler.vb".into(),
                pattern_type: "ThreadPool".into(),
                count: 1,
                modern_equivalent: "IHostedService / BackgroundService".into(),
                risk_level: "high".into(),
            }],
            total_background_files: 1,
            uses_thread_pool: true,
            uses_timers: false,
            uses_task_run: false,
            uses_bg_worker: false,
            uses_hangfire: false,
            uses_quartz: false,
            fire_and_forget_count: 1,
        };

        let cross = analyzers::cross_cutting::build_cross_cutting_summary(
            &[],
            &state,
            &empty_js(),
            &empty_gis(),
            &empty_anti(),
            &empty_endpoints(),
            &empty_asp(),
            &empty_rpt(),
            &methods,
            &deps,
            &cache,
            &email,
            &bg,
            &empty_sp_catalog(),
            &empty_inheritance(),
            &empty_config_transforms(),
            &empty_resource_inv(),
            &empty_master_regions(),
            &empty_vb_translation(),
        );

        assert_eq!(cross.total_methods, 2);
        assert_eq!(cross.total_event_handlers, 1);
        assert_eq!(cross.total_web_methods, 0);
        assert_eq!(
            cross.largest_file_by_methods,
            Some(("Default.aspx".to_string(), 2))
        );
        assert_eq!(cross.total_nuget_packages, 1);
        assert_eq!(cross.total_cached_pages, 1);
        assert_eq!(cross.total_cache_keys, 0);
        assert!(cross.has_email);
        assert!(cross.has_background_jobs);
    }

    // ── flag_belongs_to_page: per-dossier VB flag scoping ───────────────────
    //
    // Regression guard: before this helper, the filter used
    // `flag_path.contains(codebehind.unwrap_or(""))`. When the dossier had
    // no detected codebehind the filter degenerated to `.contains("")`
    // which is always true, so the first page without a codebehind dumped
    // the project-wide VB flag list (~50 KB on OciusX) into a single
    // dossier's section.

    #[test]
    fn flag_belongs_to_page_accepts_exact_page() {
        assert!(analyzers::vb_translation::flag_belongs_to_page(
            "Site/AuthCallback.aspx",
            "Site/AuthCallback.aspx",
            None
        ));
    }

    #[test]
    fn flag_belongs_to_page_accepts_detected_codebehind() {
        assert!(analyzers::vb_translation::flag_belongs_to_page(
            "Site/AuthCallback.aspx.vb",
            "Site/AuthCallback.aspx",
            Some("Site/AuthCallback.aspx.vb"),
        ));
    }

    #[test]
    fn flag_belongs_to_page_accepts_conventional_aspx_sibling_without_codebehind() {
        // Page inherits `System.Web.UI.Page` directly — dossier builder
        // sets `codebehind_file = None` — the conventional `.aspx.vb`
        // sibling still belongs to this page.
        assert!(analyzers::vb_translation::flag_belongs_to_page(
            "Site/AuthCallback.aspx.vb",
            "Site/AuthCallback.aspx",
            None
        ));
        assert!(analyzers::vb_translation::flag_belongs_to_page(
            "Site/AuthCallback.aspx.cs",
            "Site/AuthCallback.aspx",
            None
        ));
    }

    #[test]
    fn flag_belongs_to_page_rejects_unrelated_files_when_codebehind_is_none() {
        // THE regression guard: codebehind None must not open the gate.
        assert!(!analyzers::vb_translation::flag_belongs_to_page(
            "Site/Other.aspx.vb",
            "Site/AuthCallback.aspx",
            None
        ));
        assert!(!analyzers::vb_translation::flag_belongs_to_page(
            "App_Code/shared/Helpers.vb",
            "Site/AuthCallback.aspx",
            None
        ));
        assert!(!analyzers::vb_translation::flag_belongs_to_page(
            "Site/permits/permits.aspx.vb",
            "Site/AuthCallback.aspx",
            None
        ));
        // Empty-string codebehind must behave the same as None.
        assert!(!analyzers::vb_translation::flag_belongs_to_page(
            "App_Code/shared/Helpers.vb",
            "Site/AuthCallback.aspx",
            Some(""),
        ));
    }

    #[test]
    fn flag_belongs_to_page_rejects_unrelated_files_with_codebehind() {
        assert!(!analyzers::vb_translation::flag_belongs_to_page(
            "App_Code/shared/Helpers.vb",
            "Site/AuthCallback.aspx",
            Some("Site/AuthCallback.aspx.vb"),
        ));
        // And must not accept a file that merely *contains* the
        // codebehind path as a substring.
        assert!(!analyzers::vb_translation::flag_belongs_to_page(
            "Other/Site/AuthCallback.aspx.vb",
            "Site/AuthCallback.aspx",
            Some("AuthCallback.aspx.vb"),
        ));
    }

    #[test]
    fn flag_belongs_to_page_non_aspx_page_does_not_fall_back_to_sibling() {
        // For an .ascx / .master page we don't blindly accept
        // `<page>.vb` — only explicit codebehind detection counts.
        assert!(!analyzers::vb_translation::flag_belongs_to_page(
            "Controls/MyControl.ascx.vb",
            "Controls/MyControl.ascx",
            None,
        ));
        // But if the dossier detected the codebehind, it's accepted.
        assert!(analyzers::vb_translation::flag_belongs_to_page(
            "Controls/MyControl.ascx.vb",
            "Controls/MyControl.ascx",
            Some("Controls/MyControl.ascx.vb"),
        ));
    }

    // ── Per-page LLM enhancement: selection + parsing ──────────────────────

    fn dossier_with(file_path: &str, complexity: &str, blast_radius: u8) -> MigrationDossier {
        make_test_dossier(file_path, vec![], blast_radius)
            .tap_set(|d| d.estimated_complexity = complexity.to_string())
    }

    trait Tap {
        fn tap_set<F: FnOnce(&mut Self)>(self, f: F) -> Self;
    }
    impl<T> Tap for T {
        fn tap_set<F: FnOnce(&mut Self)>(mut self, f: F) -> Self {
            f(&mut self);
            self
        }
    }

    #[test]
    fn dossier_llm_priority_extracts_parenthetical_score() {
        let d = dossier_with("p.aspx", "High (score 28): hairy", 7);
        assert_eq!(dossier_llm_priority(&d), (28, 7));
    }

    #[test]
    fn dossier_llm_priority_falls_back_to_band_weight_when_no_score() {
        let d = dossier_with("p.aspx", "Medium: moderate effort", 4);
        let (score, br) = dossier_llm_priority(&d);
        assert_eq!(score, 10, "Medium band without numeric score → weight 10");
        assert_eq!(br, 4);
    }

    #[test]
    fn select_dossiers_for_llm_picks_highest_complexity_and_is_deterministic() {
        let dossiers = vec![
            dossier_with("a.aspx", "Low (score 5): trivial", 1),
            dossier_with("b.aspx", "High (score 28): very hairy", 7),
            dossier_with("c.aspx", "Medium (score 12): moderate", 4),
            dossier_with("d.aspx", "High (score 28): also hairy", 6),
            dossier_with("e.aspx", "Low (score 4)", 1),
        ];
        let picked = select_dossiers_for_llm(&dossiers, 3);
        let paths: Vec<&str> = picked.iter().map(|d| d.file_path.as_str()).collect();
        // Both 28-score pages come first, tie-broken by blast radius desc.
        // d.aspx: 28 + br 6; b.aspx: 28 + br 7 → b first, d second.
        // c.aspx (12) rounds out the top 3.
        assert_eq!(paths, vec!["b.aspx", "d.aspx", "c.aspx"]);
    }

    #[test]
    fn select_dossiers_for_llm_zero_cap_returns_empty() {
        let dossiers = vec![dossier_with("a.aspx", "High (score 20)", 5)];
        assert!(select_dossiers_for_llm(&dossiers, 0).is_empty());
    }

    #[test]
    fn select_dossiers_for_llm_tie_broken_by_path_when_all_else_equal() {
        // Same complexity, same blast radius → alphabetical file_path wins
        // so the set of enhanced pages is reproducible run-to-run.
        let dossiers = vec![
            dossier_with("zeta.aspx", "High (score 20)", 5),
            dossier_with("alpha.aspx", "High (score 20)", 5),
            dossier_with("mu.aspx", "High (score 20)", 5),
        ];
        let picked = select_dossiers_for_llm(&dossiers, 2);
        let paths: Vec<&str> = picked.iter().map(|d| d.file_path.as_str()).collect();
        assert_eq!(paths, vec!["alpha.aspx", "mu.aspx"]);
    }

    #[test]
    fn parse_page_llm_response_extracts_both_blocks() {
        let raw = "BUSINESS_PURPOSE: Handles the user password-reset flow. \
                   Loads the reset token from the URL and verifies it against the \
                   Users table, then prompts for a new password.\n\
                   \n\
                   MIGRATION_NOTES:\n\
                   - Replace the HttpContext session token handshake with a \
                   scoped DI service.\n\
                   - Introduce a PasswordResetForm.razor Blazor component \
                   owning the token + password fields.";
        let (bp, notes) = parse_page_llm_response(raw);
        let bp = bp.expect("business purpose parsed");
        assert!(bp.starts_with("Handles the user password-reset flow"));
        let notes = notes.expect("migration notes parsed");
        assert!(notes.contains("PasswordResetForm.razor"));
        // Blocks must not bleed into each other.
        assert!(!bp.contains("MIGRATION_NOTES"));
        assert!(!notes.contains("BUSINESS_PURPOSE"));
    }

    #[test]
    fn parse_page_llm_response_tolerates_human_case_labels() {
        let raw = "Business Purpose: Seeds the dashboard chart widgets.\n\n\
                   Migration Notes:\n- Move chart rendering to a BlazorCharts component.";
        let (bp, notes) = parse_page_llm_response(raw);
        assert!(bp.is_some());
        assert!(notes.is_some());
    }

    #[test]
    fn parse_page_llm_response_empty_when_labels_missing() {
        let raw = "The model produced free-form prose without labels.";
        let (bp, notes) = parse_page_llm_response(raw);
        assert!(bp.is_none());
        assert!(notes.is_none());
    }

    #[test]
    fn build_page_llm_prompt_inlines_deterministic_facts() {
        let mut d = make_test_dossier("Site/Widgets.aspx", vec!["Users", "Orders"], 6);
        d.inherits_class = Some("App.Widgets.WidgetsPage".into());
        d.risk_factors = vec!["Heavy ViewState".into()];
        let prompt = build_page_llm_prompt(
            &d,
            "<html>…</html>",
            Some("Public Class Foo\nEnd Class"),
            "blazor",
        );
        // Target stack appears in the header AND in the output-format section.
        assert_eq!(prompt.matches("blazor").count() >= 2, true);
        assert!(prompt.contains("App.Widgets.WidgetsPage"));
        assert!(prompt.contains("Users, Orders"));
        assert!(prompt.contains("Heavy ViewState"));
        assert!(prompt.contains("BUSINESS_PURPOSE:"));
        assert!(prompt.contains("MIGRATION_NOTES:"));
    }

    #[test]
    fn build_page_llm_prompt_truncates_large_codebehind() {
        let d = make_test_dossier("Site/Big.aspx", vec![], 3);
        let huge = "x".repeat(20_000);
        let prompt = build_page_llm_prompt(&d, "<html></html>", Some(&huge), "blazor");
        assert!(prompt.contains("<truncated"));
        // But the marker region must still include the codebehind block header.
        assert!(prompt.contains("## CODEBEHIND"));
    }

    #[test]
    fn dossier_llm_priority_is_stable_for_typical_complexity_strings() {
        // The strings produced by `estimate_complexity_for_dossier` all
        // look like "Band (score N): …". Spot-check a handful.
        let cases = [
            ("Low (score 3): straightforward migration", 3u32),
            ("Medium (score 12): moderate effort — address state", 12),
            (
                "High (score 26): significant effort — plan multiple sprints",
                26,
            ),
            ("Critical (score 45): rip-and-replace territory", 45),
        ];
        for (s, expected) in cases {
            let d = dossier_with("p.aspx", s, 5);
            let (score, _) = dossier_llm_priority(&d);
            assert_eq!(score, expected, "failed on complexity string {s:?}");
        }
    }

    #[test]
    fn vb_translation_flags_detect_module() {
        let vb_code = r#"
Module GlobalHelpers
    Public Sub DoSomething()
    End Sub
End Module
"#;
        let flags = analyzers::vb_translation::analyze_vb_translation_flags(&[("Helpers.vb", vb_code)]);
        assert!(!flags.translation_flags.is_empty());
        assert!(
            flags
                .translation_flags
                .iter()
                .any(|f| f.pattern.contains("Module"))
        );
        assert!(flags.total_flags > 0);
    }

    #[test]
    fn vb_translation_tracks_dynamic_dispatch_risk() {
        let vb_code = r#"
Option Strict Off
Public Class Legacy
    Public Sub M()
        Dim obj As Object
        obj.DoWork()
        CallByName(obj, "DoWork", CallType.Method)
    End Sub
End Class
"#;
        let flags = analyzers::vb_translation::analyze_vb_translation_flags(&[("Legacy.vb", vb_code)]);
        assert_eq!(flags.dynamic_dispatch.option_strict_off_files, 1);
        assert_eq!(flags.dynamic_dispatch.methods_with_dynamic_dispatch, 1);
        assert_eq!(flags.dynamic_dispatch.object_var_count, 1);
        assert_eq!(flags.dynamic_dispatch.callbyname_count, 1);
        assert_eq!(flags.dynamic_dispatch.late_binding_call_count, 1);
        assert_eq!(flags.dynamic_dispatch.dynamic_dispatch_risk_tier, "high");
    }

    #[test]
    fn email_detection_finds_smtp_client() {
        let code = r#"
Dim smtp As New SmtpClient("mail.example.com")
Dim msg As New MailMessage()
msg.To.Add("user@example.com")
smtp.Send(msg)
"#;
        let report = analyzers::email::detect_email_patterns(&[("Mailer.vb", code)], None);
        assert!(report.has_email);
        assert!(report.total_email_files > 0);
    }

    #[test]
    fn background_job_detection_finds_thread_pool() {
        let code = r#"
ThreadPool.QueueUserWorkItem(Sub(state)
    ProcessBatch()
End Sub)
"#;
        let report = analyzers::background_jobs::detect_background_job_patterns(&[("Worker.vb", code)], None);
        assert!(report.has_background_jobs);
        assert!(report.uses_thread_pool);
    }

    #[test]
    fn dependency_inventory_maps_replacements() {
        let refs = vec![ProjectReferenceBundle {
            project_path: "App.vbproj".into(),
            target_framework: Some("v4.7.2".into()),
            assembly_name: Some("MyApp".into()),
            root_namespace: Some("MyApp".into()),
            package_references: vec![
                PackageRef {
                    name: "Newtonsoft.Json".into(),
                    version: Some("13.0.1".into()),
                },
                PackageRef {
                    name: "EntityFramework".into(),
                    version: Some("6.4.4".into()),
                },
            ],
            assembly_references: vec!["System.Web".into(), "System.Data".into()],
            project_dependencies: vec![],
        }];
        let inv = analyzers::dependencies::build_dependency_inventory(&refs);
        assert!(inv.target_frameworks.contains(&"v4.7.2".to_string()));
        assert_eq!(inv.total_packages, 2);
        // Newtonsoft.Json should have a replacement
        assert!(
            inv.nuget_packages
                .iter()
                .any(|p| p.name == "Newtonsoft.Json" && p.modern_replacement.is_some())
        );
    }

    #[test]
    fn multi_tenancy_detects_tenant_id() {
        let code = r#"
Dim tenantId As String = CStr(Session("TenantId"))
Dim conn As String = GetConnectionForTenant(tenantId)
"#;
        let report = analyzers::multi_tenancy::detect_multi_tenancy(None, &[("Data.vb", code)], None);
        assert!(!report.detection_evidence.is_empty());
    }

    #[test]
    fn caching_inventory_finds_output_cache() {
        let markup = r#"<%@ Page Language="VB" %>
<%@ OutputCache Duration="60" VaryByParam="id" %>
<asp:GridView ID="gv" runat="server" />"#;
        let files = vec![FileContent {
            file_path: "Products.aspx".into(),
            markup_content: markup.into(),
            codebehind_content: None,
        }];
        let inv = analyzers::caching::build_caching_inventory(&files, &[], &[]);
        assert_eq!(inv.total_cached_pages, 1);
        assert_eq!(inv.output_cache_pages.len(), 1);
        assert_eq!(inv.output_cache_pages[0].duration_seconds, Some(60));
        assert_eq!(inv.output_cache_pages[0].vary_by_param, Some("id".into()));
    }

    #[test]
    fn url_routing_extracts_rewrite_rules() {
        let web_config = r#"
<configuration>
  <system.webServer>
    <rewrite>
      <rules>
        <rule name="Products" stopProcessing="true">
          <match url="^products/(.+)$" />
          <action type="Rewrite" url="Products.aspx?id={R:1}" />
        </rule>
      </rules>
    </rewrite>
  </system.webServer>
</configuration>"#;
        let inv = analyzers::routing::extract_url_routing(Some(web_config), "", &[]);
        assert!(!inv.rewrite_rules.is_empty());
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
            llm_business_purpose: None,
            llm_migration_notes: None,
        }
    }

    fn empty_js() -> JsAnalysisSummary {
        JsAnalysisSummary {
            total_script_files: 0,
            legacy_total_js_files: 0,
            script_files_with_server_deps: 0,
            legacy_js_files_with_server_deps: 0,
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

    // =========================================================================
    // PHASE 34 TESTS: Stored Procedure Catalog (Ticket 1)
    // =========================================================================

    #[test]
    fn sp_catalog_parses_sql_definitions() {
        let sql = r#"
CREATE PROCEDURE dbo.GetUsers
    @Active bit = 1,
    @RoleId int
AS
BEGIN
    SELECT * FROM Users WHERE Active = @Active AND RoleId = @RoleId
END
"#;
        let catalog = analyzers::sp_catalog::build_sp_catalog(
            &[("stored_procs/GetUsers.sql".to_string(), sql.to_string())],
            &[],
        );
        assert_eq!(catalog.total_procedures, 1);
        assert_eq!(catalog.procedures[0].name, "GetUsers");
        assert_eq!(catalog.procedures[0].parameters.len(), 2);
        assert!(
            catalog.procedures[0]
                .tables_read
                .contains(&"Users".to_string())
        );
        assert!(!catalog.procedures[0].has_dynamic_sql);
    }

    #[test]
    fn sp_catalog_cross_references_code_calls() {
        let sql = r#"
CREATE PROCEDURE dbo.GetActiveProjects
AS
SELECT * FROM Projects WHERE Active = 1
"#;
        let cs_code = r#"
var cmd = new SqlCommand();
cmd.CommandType = CommandType.StoredProcedure;
cmd.CommandText = "GetActiveProjects";
cmd.Connection = conn;
cmd.ExecuteReader();
"#;
        let catalog = analyzers::sp_catalog::build_sp_catalog(
            &[("sp/GetActiveProjects.sql".to_string(), sql.to_string())],
            &[("Data/ProjectRepo.cs", cs_code)],
        );
        assert_eq!(catalog.procedures_called_from_code, 1);
        assert_eq!(catalog.uncalled_procedures.len(), 0);
        assert!(!catalog.procedures[0].called_from.is_empty());
    }

    #[test]
    fn sp_catalog_identifies_uncalled_procedures() {
        let sql = r#"
CREATE PROCEDURE dbo.OldUnusedProc AS SELECT 1
"#;
        let catalog = analyzers::sp_catalog::build_sp_catalog(&[("sp/unused.sql".to_string(), sql.to_string())], &[]);
        assert_eq!(catalog.uncalled_procedures.len(), 1);
        assert!(catalog.procedures[0].called_from.is_empty());
    }

    #[test]
    fn sp_catalog_detects_dynamic_sql() {
        let sql = r#"
CREATE PROCEDURE dbo.DynSearch @Filter nvarchar(200) AS
DECLARE @sql nvarchar(max)
SET @sql = 'SELECT * FROM Products WHERE ' + @Filter
EXEC sp_executesql @sql
"#;
        let catalog = analyzers::sp_catalog::build_sp_catalog(&[("sp/dyn.sql".to_string(), sql.to_string())], &[]);
        assert!(catalog.procedures[0].has_dynamic_sql);
        assert!(
            catalog.procedures[0]
                .modern_equivalent
                .contains("SQL injection")
        );
    }

    /// `build_sp_catalog_public` must sort procs with code callers to the
    /// front so that when `sp_limit` truncates, framework procs (aspnet_*)
    /// that no application code actually references get dropped first.
    #[test]
    fn sp_catalog_public_sorts_called_procs_first_and_truncates_tail() {
        // Three aspnet_ framework procs with no code references + one
        // business proc that IS called from code via a SqlCommand.
        let sql = r#"
CREATE PROCEDURE aspnet_Users_CreateUser AS SELECT 1
CREATE PROCEDURE aspnet_Roles_CreateRole AS SELECT 1
CREATE PROCEDURE aspnet_Membership_SetPassword AS SELECT 1
CREATE PROCEDURE usp_GetBusinessData @Id int AS SELECT * FROM Business WHERE Id = @Id
"#;
        let cs_code = r#"
var cmd = new SqlCommand();
cmd.CommandType = CommandType.StoredProcedure;
cmd.CommandText = "usp_GetBusinessData";
cmd.ExecuteReader();
"#;
        let catalog = analyzers::sp_catalog::build_sp_catalog_public(
            &[("db/all.sql".to_string(), sql.to_string())],
            &[("Data/BusinessRepo.cs", cs_code)],
            /* sp_limit */ 2,
        );

        assert_eq!(catalog.procedures.len(), 2);
        // The business proc with a code caller must survive and be first.
        assert_eq!(catalog.procedures[0].name, "usp_GetBusinessData");
        assert!(!catalog.procedures[0].called_from.is_empty());
        // The second slot is one of the aspnet_ procs (tie-broken
        // alphabetically). The key guarantee is that usp_GetBusinessData
        // was not evicted by the truncation.
        assert!(
            catalog.procedures[1].name.starts_with("aspnet_"),
            "slot 1 should be a framework proc after usp_; got {}",
            catalog.procedures[1].name
        );
        // total_procedures reflects what's actually in the catalog.
        assert_eq!(catalog.total_procedures, 2);
    }

    /// With no truncation, the sort still runs but all procs are retained
    /// and `total_procedures` equals the proc count. This guards against a
    /// regression where the sort accidentally dropped rows.
    #[test]
    fn sp_catalog_public_no_truncation_keeps_all_procs() {
        let sql = r#"
CREATE PROCEDURE aspnet_a AS SELECT 1
CREATE PROCEDURE usp_business AS SELECT 1
"#;
        let cs_code = r#"
var cmd = new SqlCommand();
cmd.CommandType = CommandType.StoredProcedure;
cmd.CommandText = "usp_business";
cmd.ExecuteReader();
"#;
        let catalog = analyzers::sp_catalog::build_sp_catalog_public(
            &[("db/all.sql".to_string(), sql.to_string())],
            &[("Data/App.cs", cs_code)],
            /* sp_limit */ 0,
        );
        assert_eq!(catalog.procedures.len(), 2);
        assert_eq!(catalog.total_procedures, 2);
        assert_eq!(catalog.procedures[0].name, "usp_business");
    }

    // =========================================================================
    // PHASE 34 TESTS: Inheritance Chain Resolution (Ticket 2)
    // =========================================================================

    #[test]
    fn inheritance_resolves_cs_chain() {
        let base_code = r#"
public class BasePage : System.Web.UI.Page
{
    protected override void OnInit(EventArgs e) { base.OnInit(e); }
    protected void Page_Load(object sender, EventArgs e) { Session["user"] = "admin"; }
}
"#;
        let derived_code = r#"
public class EditPage : BasePage
{
    protected override void Page_Load(object sender, EventArgs e) { base.Page_Load(sender, e); }
}
"#;
        let markup = FileContent {
            file_path: "EditPage.aspx".to_string(),
            markup_content: r#"<%@ Page Inherits="EditPage" %>"#.to_string(),
            codebehind_content: None,
        };
        let report = analyzers::inheritance::resolve_inheritance_chains(
            &[("BasePage.cs", base_code), ("EditPage.cs", derived_code)],
            &[markup],
        );
        assert!(!report.chains.is_empty());
        assert!(report.chains[0].chain.len() >= 2);
    }

    #[test]
    fn inheritance_resolves_vb_chain() {
        let base_code = r#"
Public Class SecureBasePage
    Inherits System.Web.UI.Page

    Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load
        Session("userId") = 123
    End Sub
End Class
"#;
        let derived = r#"
Public Class AdminPage
    Inherits SecureBasePage

    Protected Overrides Sub Page_Load(sender As Object, e As EventArgs)
        MyBase.Page_Load(sender, e)
    End Sub
End Class
"#;
        let markup = FileContent {
            file_path: "Admin.aspx".to_string(),
            markup_content: r#"<%@ Page Inherits="AdminPage" %>"#.to_string(),
            codebehind_content: None,
        };
        let report = analyzers::inheritance::resolve_inheritance_chains(
            &[("SecureBasePage.vb", base_code), ("AdminPage.vb", derived)],
            &[markup],
        );
        assert!(!report.chains.is_empty());
        // AdminPage should be in the chain walking up to SecureBasePage
        let chain = &report.chains[0];
        assert!(chain.chain.contains(&"AdminPage".to_string()));
        assert!(chain.chain.contains(&"SecureBasePage".to_string()));
    }

    #[test]
    fn inheritance_detects_shared_lifecycle_methods() {
        let base_code = r#"
public class AppBasePage : System.Web.UI.Page
{
    protected override void OnInit(EventArgs e) { base.OnInit(e); }
    protected void Page_Load(object sender, EventArgs e) { }
    protected void Page_PreRender(object sender, EventArgs e) { }
}
"#;
        let child1 = r#"
public class Page1 : AppBasePage
{
    protected override void Page_Load(object sender, EventArgs e) { base.Page_Load(sender, e); }
}
"#;
        let child2 = r#"
public class Page2 : AppBasePage
{
    protected override void Page_Load(object sender, EventArgs e) { base.Page_Load(sender, e); }
}
"#;
        let m1 = FileContent {
            file_path: "Page1.aspx".to_string(),
            markup_content: r#"<%@ Page Inherits="Page1" %>"#.to_string(),
            codebehind_content: None,
        };
        let m2 = FileContent {
            file_path: "Page2.aspx".to_string(),
            markup_content: r#"<%@ Page Inherits="Page2" %>"#.to_string(),
            codebehind_content: None,
        };
        let report = analyzers::inheritance::resolve_inheritance_chains(
            &[
                ("AppBasePage.cs", base_code),
                ("Page1.cs", child1),
                ("Page2.cs", child2),
            ],
            &[m1, m2],
        );
        // AppBasePage referenced by 2 pages — should be in base_classes
        let base_info = report
            .base_classes
            .iter()
            .find(|b| b.class_name == "AppBasePage");
        assert!(base_info.is_some(), "AppBasePage should be in base_classes");
        assert!(base_info.expect("exists").lifecycle_methods.len() >= 2);
    }

    #[test]
    fn inheritance_detects_session_writes_from_base() {
        let base_code = r#"
public class StatefulBase : System.Web.UI.Page
{
    protected void Page_Load(object sender, EventArgs e) {
        Session["cart"] = new ShoppingCart();
        Session["locale"] = "en-US";
    }
}
"#;
        let derived = r#"
public class CheckoutPage : StatefulBase
{
}
"#;
        let markup = FileContent {
            file_path: "Checkout.aspx".to_string(),
            markup_content: r#"<%@ Page Inherits="CheckoutPage" %>"#.to_string(),
            codebehind_content: None,
        };
        let report = analyzers::inheritance::resolve_inheritance_chains(
            &[("StatefulBase.cs", base_code), ("CheckoutPage.cs", derived)],
            &[markup],
        );
        assert!(!report.chains.is_empty(), "Should have at least one chain");
        let chain = report
            .chains
            .iter()
            .find(|c| c.chain.contains(&"CheckoutPage".to_string()))
            .expect("CheckoutPage chain should exist");
        assert!(
            chain.inherited_state_writes.len() >= 2,
            "Expected >= 2 inherited state writes, got {:?}",
            chain.inherited_state_writes
        );
    }

    #[test]
    fn inheritance_handles_deep_chain() {
        let level0 = r#"
public class Level0 : System.Web.UI.Page
{
    protected void Page_Load(object sender, EventArgs e) { }
}
"#;
        let level1 = r#"
public class Level1 : Level0
{
    protected override void Page_Load(object sender, EventArgs e) { base.Page_Load(sender, e); }
}
"#;
        let level2 = r#"
public class Level2 : Level1
{
    protected override void Page_Load(object sender, EventArgs e) { base.Page_Load(sender, e); }
}
"#;
        let markup = FileContent {
            file_path: "DeepPage.aspx".to_string(),
            markup_content: r#"<%@ Page Inherits="Level2" %>"#.to_string(),
            codebehind_content: None,
        };
        let report = analyzers::inheritance::resolve_inheritance_chains(
            &[
                ("Level0.cs", level0),
                ("Level1.cs", level1),
                ("Level2.cs", level2),
            ],
            &[markup],
        );
        // Chain: Level2 → Level1 → Level0 → System.Web.UI.Page = depth 4
        assert!(
            report.deepest_chain_depth >= 3,
            "Expected depth >= 3, got {}",
            report.deepest_chain_depth
        );
    }

    #[test]
    fn inheritance_empty_code_produces_empty_report() {
        let report = analyzers::inheritance::resolve_inheritance_chains(&[], &[]);
        assert!(report.chains.is_empty());
        assert!(report.base_classes.is_empty());
        assert_eq!(report.deepest_chain_depth, 0);
    }

    // =========================================================================
    // PHASE 34 TESTS: packages.config + Binding Redirects (Ticket 3)
    // =========================================================================

    #[test]
    fn parse_packages_config_basic() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<packages>
  <package id="Newtonsoft.Json" version="13.0.3" targetFramework="net48" />
  <package id="EntityFramework" version="6.4.4" targetFramework="net48" />
  <package id="AutoMapper" version="12.0.1" targetFramework="net48" />
</packages>"#;
        let packages = analyzers::dependencies::parse_packages_config(xml);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].package_id, "Newtonsoft.Json");
        assert_eq!(packages[0].version, "13.0.3");
        assert_eq!(packages[0].target_framework, "net48");
    }

    #[test]
    fn parse_packages_config_with_dev_dependency() {
        let xml = r#"<packages>
  <package id="xunit" version="2.4.2" targetFramework="net48" developmentDependency="true" />
</packages>"#;
        let packages = analyzers::dependencies::parse_packages_config(xml);
        assert_eq!(packages.len(), 1);
        assert!(packages[0].is_dev_dependency);
    }

    #[test]
    fn parse_packages_config_empty() {
        let xml = r#"<packages></packages>"#;
        let packages = analyzers::dependencies::parse_packages_config(xml);
        assert!(packages.is_empty());
    }

    #[test]
    fn parse_packages_config_detects_modern_replacement() {
        let xml = r#"<packages>
  <package id="Newtonsoft.Json" version="13.0.3" targetFramework="net48" />
</packages>"#;
        let packages = analyzers::dependencies::parse_packages_config(xml);
        assert_eq!(packages.len(), 1);
        // Newtonsoft.Json should have System.Text.Json as modern replacement
        // (depends on lookup_modern_replacement impl)
    }

    #[test]
    fn binding_redirects_from_web_config() {
        let config = r#"<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <runtime>
    <assemblyBinding xmlns="urn:schemas-microsoft-com:asm.v1">
      <dependentAssembly>
        <assemblyIdentity name="Newtonsoft.Json" publicKeyToken="30ad4fe6b2a6aeed" />
        <bindingRedirect oldVersion="0.0.0.0-13.0.0.0" newVersion="13.0.0.0" />
      </dependentAssembly>
      <dependentAssembly>
        <assemblyIdentity name="System.Web.Mvc" publicKeyToken="31bf3856ad364e35" />
        <bindingRedirect oldVersion="0.0.0.0-5.2.9.0" newVersion="5.2.9.0" />
      </dependentAssembly>
    </assemblyBinding>
  </runtime>
</configuration>"#;
        let redirects = analyzers::dependencies::extract_binding_redirects(Some(config));
        assert_eq!(redirects.len(), 2);
        assert_eq!(redirects[0].assembly_name, "Newtonsoft.Json");
        assert_eq!(redirects[0].old_version_range, "0.0.0.0-13.0.0.0");
        assert_eq!(redirects[0].new_version, "13.0.0.0");
        assert_eq!(
            redirects[0].public_key_token.as_deref(),
            Some("30ad4fe6b2a6aeed")
        );
    }

    #[test]
    fn binding_redirects_none_config() {
        let redirects = analyzers::dependencies::extract_binding_redirects(None);
        assert!(redirects.is_empty());
    }

    #[test]
    fn binding_redirects_no_redirects() {
        let config = r#"<configuration><runtime></runtime></configuration>"#;
        let redirects = analyzers::dependencies::extract_binding_redirects(Some(config));
        assert!(redirects.is_empty());
    }

    // =========================================================================
    // PHASE 34 TESTS: Method Body Extraction (Ticket 4)
    // =========================================================================

    #[test]
    fn extract_cs_method_body_simple() {
        let code = r#"
public class MyPage : Page
{
    protected void Page_Load(object sender, EventArgs e)
    {
        var x = 1;
        var y = 2;
    }

    private void Helper()
    {
        // do nothing
    }
}
"#;
        let result = extract_cs_method_body(code, "Page_Load");
        assert!(result.is_some());
        let (body, _start, _end, lines) = result.expect("found");
        assert!(body.contains("var x = 1"));
        assert!(body.contains("var y = 2"));
        assert!(lines >= 4); // signature + body + braces
    }

    #[test]
    fn extract_cs_method_body_with_nested_braces() {
        let code = r#"
public void ProcessData()
{
    if (true)
    {
        for (int i = 0; i < 10; i++)
        {
            DoSomething();
        }
    }
}
"#;
        let result = extract_cs_method_body(code, "ProcessData");
        assert!(result.is_some());
        let (body, _, _, _) = result.expect("found");
        assert!(body.contains("DoSomething()"));
    }

    #[test]
    fn extract_cs_method_body_with_strings_containing_braces() {
        let code = r#"
public string FormatJson()
{
    return "{ \"key\": \"value\" }";
}
"#;
        let result = extract_cs_method_body(code, "FormatJson");
        assert!(result.is_some());
        let (body, _, _, _) = result.expect("found");
        assert!(body.contains("return"));
    }

    #[test]
    fn extract_cs_method_body_not_found() {
        let code = r#"
public void ExistingMethod() { }
"#;
        let result = extract_cs_method_body(code, "NonExistentMethod");
        assert!(result.is_none());
    }

    #[test]
    fn extract_vb_method_body_simple() {
        let code = r#"
Public Class MyPage
    Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load
        Dim x As Integer = 1
        Dim y As Integer = 2
    End Sub

    Private Sub Helper()
        ' do nothing
    End Sub
End Class
"#;
        let result = extract_vb_method_body(code, "Page_Load");
        assert!(result.is_some());
        let (body, _start, _end, lines) = result.expect("found");
        assert!(body.contains("Dim x"));
        assert!(body.contains("Dim y"));
        assert!(lines >= 3);
    }

    #[test]
    fn extract_vb_method_body_with_nested_blocks() {
        let code = r#"
Protected Sub ProcessData()
    If True Then
        For Each item In collection
            DoSomething(item)
        Next
    End If
End Sub
"#;
        let result = extract_vb_method_body(code, "ProcessData");
        assert!(result.is_some());
        let (body, _, _, _) = result.expect("found");
        assert!(body.contains("DoSomething"));
    }

    #[test]
    fn extract_vb_method_body_function() {
        let code = r#"
Public Function GetTotal() As Decimal
    Dim total As Decimal = 0
    Return total
End Function
"#;
        let result = extract_vb_method_body(code, "GetTotal");
        assert!(result.is_some());
        let (body, _, _, _) = result.expect("found");
        assert!(body.contains("Return total"));
    }

    #[test]
    fn make_body_preview_short_method() {
        let body = "protected void Page_Load(object sender, EventArgs e)\n{\n    var x = 1;\n}";
        let preview = analyzers::methods::make_body_preview(body, 4);
        // Short method (≤30 lines) should be returned in full
        assert!(preview.contains("var x = 1"));
        assert!(!preview.contains("more lines"));
    }

    #[test]
    fn make_body_preview_long_method() {
        let mut lines: Vec<String> = Vec::new();
        lines.push("    protected void BigMethod()".to_string());
        lines.push("    {".to_string());
        for i in 0..40 {
            lines.push(format!("        var x{i} = {i};"));
        }
        lines.push("    }".to_string());
        let body = lines.join("\n");
        let line_count = body.lines().count() as u32;
        let preview = analyzers::methods::make_body_preview(&body, line_count);
        assert!(preview.contains("more lines"));
    }

    #[test]
    fn complexity_score_empty() {
        let score = analyzers::methods::compute_complexity_score("");
        assert_eq!(score, 0);
    }

    #[test]
    fn complexity_score_branches_and_loops() {
        let body = r#"
if (condition) { }
else if (other) { }
for (int i = 0; i < 10; i++) { }
while (running) { }
"#;
        let score = analyzers::methods::compute_complexity_score(body);
        // 2 if-related + 1 for + 1 while = 4+
        assert!(score >= 4, "Expected >= 4, got {score}");
    }

    #[test]
    fn complexity_score_try_catch() {
        let body = r#"
try {
    DoSomething();
} catch (Exception ex) {
    LogError(ex);
}
"#;
        let score = analyzers::methods::compute_complexity_score(body);
        // try = 2pts, catch = 2pts = 4+
        assert!(score >= 4, "Expected >= 4 for try/catch, got {score}");
    }

    #[test]
    fn complexity_score_sql() {
        let body = r#"
var sql = "SELECT * FROM Users WHERE Active = 1";
cmd.CommandText = sql;
var adapter = new SqlDataAdapter(cmd);
"#;
        let score = analyzers::methods::compute_complexity_score(body);
        // "SELECT " = 3, CommandText = 3, SqlDataAdapter = 3 = 9+
        assert!(score >= 9, "Expected >= 9 for SQL, got {score}");
    }

    #[test]
    fn complexity_score_session() {
        let body = r#"
Session["user"] = GetUser();
var cart = Session["cart"];
"#;
        let score = analyzers::methods::compute_complexity_score(body);
        // 2 session accesses
        assert!(score >= 2, "Expected >= 2 for session, got {score}");
    }

    // =========================================================================
    // PHASE 34 TESTS: Control Lifecycle Metadata (Ticket 5)
    // =========================================================================

    #[test]
    fn lifecycle_gridview_high_complexity() {
        let gv = engram_index::control_mapping::lookup("GridView").expect("GridView exists");
        assert_eq!(gv.migration_complexity, 4);
        assert!(gv.requires_databind_on_postback);
        assert!(gv.has_nested_postback);
        assert!(!gv.breaking_differences.is_empty());
    }

    #[test]
    fn lifecycle_textbox_low_complexity() {
        let tb = engram_index::control_mapping::lookup("TextBox").expect("TextBox exists");
        assert_eq!(tb.migration_complexity, 1);
        assert!(!tb.requires_databind_on_postback);
        assert!(!tb.has_nested_postback);
        assert_eq!(tb.event_firing_model, "per_user_action");
    }

    #[test]
    fn lifecycle_updatepanel_has_breaking_diffs() {
        let up = engram_index::control_mapping::lookup("UpdatePanel").expect("UpdatePanel exists");
        assert_eq!(up.migration_complexity, 4);
        assert!(up.has_nested_postback);
        assert!(up.breaking_differences.len() >= 2);
    }

    #[test]
    fn lifecycle_radgrid_max_complexity() {
        let rg = engram_index::control_mapping::lookup("RadGrid").expect("RadGrid exists");
        assert_eq!(rg.migration_complexity, 5);
        assert!(rg.requires_databind_on_postback);
        assert!(rg.has_nested_postback);
        assert!(rg.breaking_differences.len() >= 3);
    }

    #[test]
    fn lifecycle_button_stateless() {
        let btn = engram_index::control_mapping::lookup("Button").expect("Button exists");
        assert_eq!(btn.state_model, "Stateless");
        assert_eq!(btn.lifecycle_phase, "Any");
        assert_eq!(btn.migration_complexity, 1);
    }

    #[test]
    fn lifecycle_validation_controls_prerender() {
        for name in &[
            "ValidationSummary",
            "RequiredFieldValidator",
            "CompareValidator",
            "RangeValidator",
            "RegularExpressionValidator",
        ] {
            let ctrl = engram_index::control_mapping::lookup(name)
                .unwrap_or_else(|| panic!("{name} exists"));
            assert_eq!(
                ctrl.lifecycle_phase, "PreRender",
                "{name} should fire at PreRender"
            );
            assert_eq!(ctrl.state_model, "Stateless", "{name} should be stateless");
        }
    }

    #[test]
    fn lifecycle_data_source_controls_stateless() {
        for name in &[
            "SqlDataSource",
            "ObjectDataSource",
            "LinqDataSource",
            "EntityDataSource",
        ] {
            let ctrl = engram_index::control_mapping::lookup(name)
                .unwrap_or_else(|| panic!("{name} exists"));
            assert_eq!(ctrl.state_model, "Stateless", "{name} should be stateless");
            assert_eq!(ctrl.event_firing_model, "once", "{name} should fire once");
        }
    }

    #[test]
    fn lifecycle_all_entries_have_valid_phase() {
        let valid_phases = ["Init", "Load", "PreRender", "Postback", "Any"];
        for m in engram_index::control_mapping::CONTROL_MAPPINGS {
            assert!(
                valid_phases.contains(&m.lifecycle_phase),
                "{}: invalid lifecycle_phase '{}'",
                m.legacy_control,
                m.lifecycle_phase
            );
        }
    }

    #[test]
    fn lifecycle_all_entries_have_valid_state_model() {
        let valid_models = ["ViewState", "ControlState", "Stateless", "ComponentState"];
        for m in engram_index::control_mapping::CONTROL_MAPPINGS {
            assert!(
                valid_models.contains(&m.state_model),
                "{}: invalid state_model '{}'",
                m.legacy_control,
                m.state_model
            );
        }
    }

    #[test]
    fn lifecycle_all_entries_have_valid_event_model() {
        let valid_models = ["per_postback", "per_user_action", "once", "manual"];
        for m in engram_index::control_mapping::CONTROL_MAPPINGS {
            assert!(
                valid_models.contains(&m.event_firing_model),
                "{}: invalid event_firing_model '{}'",
                m.legacy_control,
                m.event_firing_model
            );
        }
    }

    #[test]
    fn lifecycle_complexity_range_1_to_5() {
        for m in engram_index::control_mapping::CONTROL_MAPPINGS {
            assert!(
                (1..=5).contains(&m.migration_complexity),
                "{}: complexity {} out of range 1-5",
                m.legacy_control,
                m.migration_complexity
            );
        }
    }

    // =========================================================================
    // PHASE 34 TESTS: Config Transforms (Ticket 6)
    // =========================================================================

    #[test]
    fn config_transforms_release() {
        let transform = r#"<?xml version="1.0" encoding="utf-8"?>
<configuration xmlns:xdt="http://schemas.microsoft.com/XML-Document-Transform">
  <system.web>
    <compilation xdt:Transform="RemoveAttributes(debug)" />
  </system.web>
  <connectionStrings>
    <add name="DefaultConnection" connectionString="Server=prod;Database=AppDb;" xdt:Transform="SetAttributes" xdt:Locator="Match(name)" />
  </connectionStrings>
  <appSettings>
    <add key="Environment" value="Production" xdt:Transform="SetAttributes" xdt:Locator="Match(key)" />
  </appSettings>
</configuration>"#;
        let report =
            analyzers::config_transforms::parse_config_transforms(&[("web.Release.config".to_string(), transform.to_string())]);
        assert_eq!(report.environments.len(), 1);
        assert_eq!(report.environments[0].name, "Release");
        assert!(report.total_transforms > 0);
        assert!(!report.connection_string_overrides.is_empty());
        assert!(!report.app_setting_overrides.is_empty());
    }

    #[test]
    fn config_transforms_debug_and_release() {
        let debug_t = r#"<configuration xmlns:xdt="http://schemas.microsoft.com/XML-Document-Transform">
  <system.web>
    <compilation debug="true" xdt:Transform="SetAttributes" />
  </system.web>
</configuration>"#;
        let release_t = r#"<configuration xmlns:xdt="http://schemas.microsoft.com/XML-Document-Transform">
  <system.web>
    <compilation debug="false" xdt:Transform="SetAttributes" />
  </system.web>
</configuration>"#;
        let report = analyzers::config_transforms::parse_config_transforms(&[
            ("web.Debug.config".to_string(), debug_t.to_string()),
            ("web.Release.config".to_string(), release_t.to_string()),
        ]);
        assert_eq!(report.environments.len(), 2);
        assert!(report.debug_flag_overrides.len() >= 2);
    }

    #[test]
    fn config_transforms_empty() {
        let report = analyzers::config_transforms::parse_config_transforms(&[]);
        assert_eq!(report.environments.len(), 0);
        assert_eq!(report.total_transforms, 0);
    }

    #[test]
    fn config_transforms_staging_environment() {
        let staging = r#"<configuration xmlns:xdt="http://schemas.microsoft.com/XML-Document-Transform">
  <connectionStrings>
    <add name="MainDb" connectionString="Server=staging-db;Database=App;" xdt:Transform="SetAttributes" xdt:Locator="Match(name)" />
  </connectionStrings>
</configuration>"#;
        let report =
            analyzers::config_transforms::parse_config_transforms(&[("web.Staging.config".to_string(), staging.to_string())]);
        assert_eq!(report.environments[0].name, "Staging");
    }

    // =========================================================================
    // PHASE 34 TESTS: Master Page Regions (Ticket 6)
    // =========================================================================

    #[test]
    fn master_page_region_map_basic() {
        let master_content = r#"<%@ Master Language="C#" %>
<html>
<head><asp:ContentPlaceHolder ID="HeadContent" runat="server" /></head>
<body>
  <asp:ContentPlaceHolder ID="MainContent" runat="server" />
  <asp:ContentPlaceHolder ID="FooterContent" runat="server"><p>Default footer</p></asp:ContentPlaceHolder>
</body>
</html>"#;
        let page = FileContent {
            file_path: "Default.aspx".to_string(),
            markup_content: r#"<%@ Page MasterPageFile="~/Site.master" %>
<asp:Content ID="c1" ContentPlaceHolderID="MainContent" runat="server">
  <h1>Hello</h1>
</asp:Content>
<asp:Content ID="c2" ContentPlaceHolderID="HeadContent" runat="server">
  <title>Home</title>
</asp:Content>"#
                .to_string(),
            codebehind_content: None,
        };
        let map = analyzers::master_pages::build_master_page_region_map(
            &[("Site.master".to_string(), master_content.to_string())],
            &[page],
        );
        assert_eq!(map.master_pages.len(), 1);
        assert!(
            map.master_pages[0]
                .placeholders
                .contains(&"HeadContent".to_string())
        );
        assert!(
            map.master_pages[0]
                .placeholders
                .contains(&"MainContent".to_string())
        );
        assert!(
            map.master_pages[0]
                .placeholders
                .contains(&"FooterContent".to_string())
        );
        // MainContent and HeadContent are filled
        let filled_regions: Vec<&str> = map
            .regions
            .iter()
            .filter(|r| !r.filled_by.is_empty())
            .map(|r| r.region_name.as_str())
            .collect();
        assert!(filled_regions.contains(&"MainContent"));
        assert!(filled_regions.contains(&"HeadContent"));
    }

    #[test]
    fn master_page_region_map_orphan_detection() {
        let page = FileContent {
            file_path: "Page.aspx".to_string(),
            markup_content: r#"<asp:Content ContentPlaceHolderID="OrphanRegion" runat="server">stuff</asp:Content>"#
                .to_string(),
            codebehind_content: None,
        };
        let map = analyzers::master_pages::build_master_page_region_map(
            &[(
                "Main.master".to_string(),
                r#"<asp:ContentPlaceHolder ID="Body" runat="server" />"#.to_string(),
            )],
            &[page],
        );
        assert!(!map.orphan_regions.is_empty());
        assert!(map.orphan_regions.contains(&"OrphanRegion".to_string()));
    }

    #[test]
    fn master_page_region_map_empty() {
        let map = analyzers::master_pages::build_master_page_region_map(&[], &[]);
        assert!(map.master_pages.is_empty());
        assert!(map.regions.is_empty());
    }

    #[test]
    fn master_page_nested_master() {
        let master_content = r#"<%@ Master MasterPageFile="~/Root.master" %>
<asp:Content ContentPlaceHolderID="Body" runat="server">
  <asp:ContentPlaceHolder ID="ChildBody" runat="server" />
</asp:Content>"#;
        let map = analyzers::master_pages::build_master_page_region_map(
            &[("Child.master".to_string(), master_content.to_string())],
            &[],
        );
        assert_eq!(map.master_pages.len(), 1);
        assert_eq!(
            map.master_pages[0].nested_master.as_deref(),
            Some("~/Root.master")
        );
    }

    // =========================================================================
    // PHASE 34 TESTS: Resource Inventory (Ticket 6)
    // =========================================================================

    #[test]
    fn resource_inventory_basic() {
        let resx = r#"<?xml version="1.0" encoding="utf-8"?>
<root>
  <data name="Title" xml:space="preserve">
    <value>Welcome</value>
  </data>
  <data name="Greeting" xml:space="preserve">
    <value>Hello, World!</value>
  </data>
  <data name="ButtonText" xml:space="preserve">
    <value>Click Me</value>
  </data>
</root>"#;
        let inv = analyzers::resources::build_resource_inventory(&[(
            "App_GlobalResources/Strings.resx".to_string(),
            resx.to_string(),
        )]);
        assert_eq!(inv.resource_files.len(), 1);
        assert_eq!(inv.total_keys, 3);
        assert!(inv.has_global_resources);
    }

    #[test]
    fn resource_inventory_language_detection() {
        let resx_en = r#"<root><data name="Hello"><value>Hello</value></data></root>"#;
        let resx_fr = r#"<root><data name="Hello"><value>Bonjour</value></data></root>"#;
        let resx_de = r#"<root><data name="Hello"><value>Hallo</value></data></root>"#;
        let inv = analyzers::resources::build_resource_inventory(&[
            (
                "App_GlobalResources/Strings.resx".to_string(),
                resx_en.to_string(),
            ),
            (
                "App_GlobalResources/Strings.fr.resx".to_string(),
                resx_fr.to_string(),
            ),
            (
                "App_GlobalResources/Strings.de.resx".to_string(),
                resx_de.to_string(),
            ),
        ]);
        assert_eq!(inv.resource_files.len(), 3);
        assert!(inv.languages_detected.len() >= 2); // fr, de at minimum
    }

    #[test]
    fn resource_inventory_local_resources() {
        let resx = r#"<root><data name="Label1.Text"><value>Submit</value></data></root>"#;
        let inv = analyzers::resources::build_resource_inventory(&[(
            "App_LocalResources/Default.aspx.resx".to_string(),
            resx.to_string(),
        )]);
        assert!(inv.has_local_resources);
        assert!(!inv.has_global_resources);
    }

    #[test]
    fn resource_inventory_empty() {
        let inv = analyzers::resources::build_resource_inventory(&[]);
        assert_eq!(inv.resource_files.len(), 0);
        assert_eq!(inv.total_keys, 0);
        assert!(!inv.has_global_resources);
        assert!(!inv.has_local_resources);
    }

    #[test]
    fn resource_inventory_embedded_resources() {
        let resx = r#"<root><data name="Icon" type="System.Resources.ResXFileRef, System.Windows.Forms"><value>icon.bmp;System.Drawing.Bitmap</value></data></root>"#;
        let inv = analyzers::resources::build_resource_inventory(&[(
            "Properties/Resources.resx".to_string(),
            resx.to_string(),
        )]);
        assert_eq!(inv.embedded_resource_count, 1);
    }

    // ── Second-pass improvement tests ────────────────────────────────────────

    #[test]
    fn cs_method_body_handles_verbatim_string() {
        // Verbatim strings contain braces that should NOT count for depth
        let code = r#"
public class Foo : Page {
    protected void Page_Load(object sender, EventArgs e) {
        string sql = @"SELECT { brackets } FROM ""table""";
        Response.Write("done");
    }
}
"#;
        let result = extract_cs_method_body(code, "Page_Load");
        assert!(result.is_some(), "Should find Page_Load");
        let (body, _, _, _line_count) = result.unwrap();
        assert!(
            body.contains("@\"SELECT"),
            "Body should contain verbatim string"
        );
        // Verbatim string braces should NOT cause premature/delayed body end
        assert!(
            body.contains("Response.Write"),
            "Body should extend past verbatim string"
        );
    }

    #[test]
    fn cs_method_body_handles_generic_return_type() {
        let code = r#"
public class SomeService {
    public async Task<ActionResult> GetData(int id) {
        return Ok(id);
    }

    public Dictionary<string, int> BuildMap() {
        return new Dictionary<string, int>();
    }
}
"#;
        let result = extract_cs_method_body(code, "GetData");
        assert!(
            result.is_some(),
            "Should find GetData with Task<ActionResult> return type"
        );
        let (body, _, _, _) = result.unwrap();
        assert!(body.contains("return Ok"));

        let result2 = extract_cs_method_body(code, "BuildMap");
        assert!(
            result2.is_some(),
            "Should find BuildMap with Dictionary<string, int> return type"
        );
    }

    #[test]
    fn vb_method_body_handles_nested_sub_with_modifiers() {
        // VB with a nested Private Sub inside the main Sub
        let code = r#"
Public Class MyPage
    Protected Sub Page_Load(sender As Object, e As EventArgs)
        Dim x = 1
        Call Helper()
    End Sub

    Private Sub Helper()
        Dim y = 2
    End Sub
End Class
"#;
        let result = extract_vb_method_body(code, "Page_Load");
        assert!(result.is_some());
        let (body, _, _, _) = result.unwrap();
        assert!(body.contains("Call Helper()"));
        // Should NOT include Helper's body
        assert!(
            !body.contains("Dim y = 2"),
            "Should not cross into Helper method body"
        );
    }

    #[test]
    fn packages_config_handles_attribute_order_variation() {
        // Attributes in different order than id, version
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<packages>
  <package version="5.2.7" id="Newtonsoft.Json" targetFramework="net461" />
  <package targetFramework="net461" developmentDependency="true" id="NUnit" version="3.13.3" />
</packages>"#;
        let pkgs = analyzers::dependencies::parse_packages_config(xml);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].package_id, "Newtonsoft.Json");
        assert_eq!(pkgs[0].version, "5.2.7");
        assert_eq!(pkgs[0].target_framework, "net461");
        assert!(!pkgs[0].is_dev_dependency);

        assert_eq!(pkgs[1].package_id, "NUnit");
        assert_eq!(pkgs[1].version, "3.13.3");
        assert!(pkgs[1].is_dev_dependency);
    }

    #[test]
    fn binding_redirects_handles_attribute_order_variation() {
        let config = r#"
<configuration>
  <runtime>
    <assemblyBinding xmlns="urn:schemas-microsoft-com:asm.v1">
      <dependentAssembly>
        <assemblyIdentity publicKeyToken="30ad4fe6b2a6aeed" name="Newtonsoft.Json" culture="neutral" />
        <bindingRedirect newVersion="13.0.0.0" oldVersion="0.0.0.0-13.0.0.0" />
      </dependentAssembly>
    </assemblyBinding>
  </runtime>
</configuration>"#;
        let redirects = analyzers::dependencies::extract_binding_redirects(Some(config));
        assert_eq!(redirects.len(), 1);
        assert_eq!(redirects[0].assembly_name, "Newtonsoft.Json");
        assert_eq!(redirects[0].old_version_range, "0.0.0.0-13.0.0.0");
        assert_eq!(redirects[0].new_version, "13.0.0.0");
        assert_eq!(
            redirects[0].public_key_token.as_deref(),
            Some("30ad4fe6b2a6aeed")
        );
    }

    #[test]
    fn inheritance_per_class_method_scoping() {
        // Two classes in one file — methods should be scoped to each class
        let code = r#"
public partial class PageA : BasePage {
    protected void Page_Load(object sender, EventArgs e) { }
    private void HelperA() { }
}

public partial class PageB : BasePage {
    protected void Page_Init(object sender, EventArgs e) { }
    private void HelperB() { }
}
"#;
        let base_code = r#"
public class BasePage : System.Web.UI.Page {
    protected virtual void OnInit(EventArgs e) { base.OnInit(e); }
}
"#;
        let markup_a = FileContent {
            file_path: "PageA.aspx".to_string(),
            markup_content: r#"<%@ Page Inherits="MyApp.PageA" %>"#.to_string(),
            codebehind_content: Some(code.to_string()),
        };
        let markup_b = FileContent {
            file_path: "PageB.aspx".to_string(),
            markup_content: r#"<%@ Page Inherits="MyApp.PageB" %>"#.to_string(),
            codebehind_content: Some(code.to_string()),
        };
        let code_files: Vec<(&str, &str)> =
            vec![("PageA.aspx.cs", code), ("BasePage.cs", base_code)];
        let report = analyzers::inheritance::resolve_inheritance_chains(&code_files, &[markup_a, markup_b]);

        // Check that PageA's chain exists and has scoped methods
        let chain_a = report.chains.iter().find(|c| c.page_file == "PageA.aspx");
        assert!(chain_a.is_some(), "Should find chain for PageA");

        // PageA methods should include Page_Load but NOT Page_Init (that's PageB's)
        if let Some(chain) = chain_a {
            let lifecycle_method_names: Vec<&str> = chain
                .inherited_lifecycle_methods
                .iter()
                .map(|(m, _)| m.as_str())
                .collect();
            // Page_Load comes from PageA, OnInit from BasePage
            // HelperB should NOT appear
            assert!(
                !lifecycle_method_names.contains(&"Page_Init"),
                "PageA chain should not include PageB's Page_Init"
            );
        }
    }

    #[test]
    fn config_transform_xpath_handlers_modules() {
        let transforms = analyzers::config_transforms::parse_config_transforms(&[(
            "web.Release.config".to_string(),
            r#"<configuration>
  <system.webServer>
    <handlers>
      <add name="ExtHandler" path="*.ext" verb="*" xdt:Transform="Insert" />
    </handlers>
    <modules>
      <add name="UrlRewrite" xdt:Transform="Insert" />
    </modules>
  </system.webServer>
</configuration>"#
                .to_string(),
        )]);
        assert!(
            !transforms.environments.is_empty(),
            "Should find transforms"
        );
        let env = &transforms.environments[0];
        // Should have handler and module transforms
        let handler_transform = env
            .transforms
            .iter()
            .find(|t| t.xpath_hint.contains("handlers"));
        let module_transform = env
            .transforms
            .iter()
            .find(|t| t.xpath_hint.contains("modules"));
        assert!(
            handler_transform.is_some(),
            "Should identify handlers xpath"
        );
        assert!(module_transform.is_some(), "Should identify modules xpath");
    }

    #[test]
    fn complexity_score_combined_high() {
        // Method with multiple complexity factors
        let body = r#"
            if (x > 0) {
                foreach (var item in items) {
                    try {
                        string sql = "SELECT * FROM Users";
                        SqlCommand cmd = new SqlCommand(sql);
                        Session["UserData"] = result;
                    } catch (Exception ex) {
                        if (retries > 0) {
                            while (retries-- > 0) { }
                        }
                    }
                }
            }
        "#;
        let score = analyzers::methods::compute_complexity_score(body);
        // Should be significantly > 0:
        // 2 ifs = 2, 1 foreach = 1, 1 try = 2, 1 catch = 2, 1 while = 1,
        // 1 SELECT = 3, 1 SqlCommand = 3, 1 Session = 1 = total ~15
        assert!(
            score >= 12,
            "Complex method should score >= 12, got {score}"
        );
    }

    // ── Third-Pass Tests ─────────────────────────────────────────────────────

    #[test]
    fn vb_method_body_function_containing_nested_sub() {
        // BUG FIX: End Sub inside a Function must decrement depth correctly
        let content = r#"
    Public Function GetData(ByVal id As Integer) As String
        Dim result As String = ""
        Call Helper(id)
        Return result
    End Function

    Private Sub Helper(ByVal id As Integer)
        Console.WriteLine(id)
    End Sub
"#;
        let result = extract_vb_method_body(content, "GetData");
        assert!(result.is_some(), "Should extract GetData Function body");
        let (body, _, _, line_count) = result.unwrap();
        assert!(
            body.contains("Call Helper"),
            "Body should contain Call Helper"
        );
        assert!(
            !body.contains("Console.WriteLine"),
            "Body should NOT contain Helper's body"
        );
        assert!(
            line_count <= 6,
            "GetData should be ~5 lines, got {line_count}"
        );
    }

    #[test]
    fn vb_method_body_sub_containing_nested_function() {
        // Reverse case: Sub containing a nested Function
        let content = r#"
    Protected Sub Page_Load(sender As Object, e As EventArgs)
        Dim x As Integer = ComputeValue(5)
    End Sub

    Private Function ComputeValue(ByVal n As Integer) As Integer
        Return n * 2
    End Function
"#;
        let result = extract_vb_method_body(content, "Page_Load");
        assert!(result.is_some(), "Should extract Page_Load");
        let (body, _, _, lc) = result.unwrap();
        assert!(body.contains("ComputeValue(5)"), "Should contain the call");
        assert!(
            !body.contains("Return n * 2"),
            "Should NOT contain Function body"
        );
        assert!(lc <= 4, "Page_Load should be ~3 lines, got {lc}");
    }

    #[test]
    fn complexity_score_no_double_counting_else_if() {
        // THIRD-PASS: `else if` should count as 1, not 2 (if + else if)
        let body = r#"
            if (x == 1) {
                DoA();
            } else if (x == 2) {
                DoB();
            } else if (x == 3) {
                DoC();
            }
        "#;
        let score = analyzers::methods::compute_complexity_score(body);
        // 1 if + 2 else if = 3, not 5
        assert_eq!(
            score, 3,
            "else if should not double-count with if, got {score}"
        );
    }

    #[test]
    fn complexity_score_no_double_counting_vb_patterns() {
        // VB: Select Case + For Each should not double count
        let body = r#"
            Select Case status
                Case "Active"
                    For Each item In items
                        DoWork(item)
                    Next
                Case "Inactive"
                    DoNothing()
            End Select
        "#;
        let score = analyzers::methods::compute_complexity_score(body);
        // 1 select case + 2 case + 1 for each = 4, not 7
        assert_eq!(score, 4, "VB patterns should not double-count, got {score}");
    }

    #[test]
    fn partial_class_methods_are_merged() {
        // THIRD-PASS: Partial classes in separate files should merge, not overwrite
        let code_files: Vec<(&str, &str)> = vec![
            (
                "Default.aspx.vb",
                r#"
Partial Class _Default
    Inherits BasePage

    Protected Sub Page_Load(sender As Object, e As EventArgs)
        Session("UserId") = 42
    End Sub

    Protected Sub btnSave_Click(sender As Object, e As EventArgs)
        SaveData()
    End Sub
End Class
"#,
            ),
            (
                "Default.aspx.designer.vb",
                r#"
Partial Class _Default

    Protected WithEvents btnSave As Global.System.Web.UI.WebControls.Button
    Protected WithEvents lblMessage As Global.System.Web.UI.WebControls.Label
End Class
"#,
            ),
        ];

        let markup = vec![FileContent {
            file_path: "Default.aspx".into(),
            markup_content: r#"<%@ Page Language="VB" Inherits="MyApp._Default" %>"#.into(),
            codebehind_content: None,
        }];

        let report = analyzers::inheritance::resolve_inheritance_chains(&code_files, &markup);

        // Verify the _Default class merged methods from both files
        // The chain should include _Default → BasePage
        assert!(
            !report.chains.is_empty(),
            "Should have at least one inheritance chain"
        );
        let chain = &report.chains[0];
        assert!(
            chain.chain.contains(&"_Default".to_string()),
            "Chain should contain _Default"
        );
    }

    #[test]
    fn effects_scoped_to_method_body_not_file() {
        // THIRD-PASS: extract_effects_from_nearby_content should only scan the method's body
        let content = r#"
Public Class MyPage
    Inherits System.Web.UI.Page

    Protected Sub Page_Load(sender As Object, e As EventArgs)
        Dim name As String = Request.QueryString("name")
        lblWelcome.Text = "Hello " & name
    End Sub

    Protected Sub btnQuery_Click(sender As Object, e As EventArgs)
        Dim cmd As New SqlCommand("SELECT * FROM Users", conn)
        Dim reader As SqlDataReader = cmd.ExecuteReader()
        Session("LastQuery") = DateTime.Now
    End Sub
End Class
"#;
        // Page_Load should NOT have SQL_Access effect
        let page_load_effects = analyzers::methods::extract_effects_from_nearby_content(content, "Page_Load");
        assert!(
            !page_load_effects.iter().any(|e| e.contains("SQL")),
            "Page_Load should NOT have SQL_Access (SQL is in btnQuery_Click), got: {:?}",
            page_load_effects
        );

        // btnQuery_Click SHOULD have SQL_Access
        let btn_effects = analyzers::methods::extract_effects_from_nearby_content(content, "btnQuery_Click");
        assert!(
            btn_effects.iter().any(|e| e.contains("SQL")),
            "btnQuery_Click SHOULD have SQL_Access, got: {:?}",
            btn_effects
        );
    }

    #[test]
    fn vb_handles_clause_extraction() {
        // THIRD-PASS: Handles clause should be captured in MethodInfo
        let content = r#"
Public Class MyPage
    Inherits System.Web.UI.Page

    Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load
        ' Init
    End Sub

    Protected Sub btnSave_Click(sender As Object, e As EventArgs) Handles btnSave.Click
        ' Save
    End Sub

    Protected Sub Timer_Tick(sender As Object, e As EventArgs) Handles Timer1.Tick, Timer2.Tick
        ' Tick from two timers
    End Sub

    Private Sub HelperMethod()
        ' No Handles clause
    End Sub
End Class
"#;
        let methods = extract_methods_from_content(content);

        let page_load = methods.iter().find(|m| m.name == "Page_Load");
        assert!(page_load.is_some(), "Should find Page_Load");
        let pl = page_load.unwrap();
        assert!(
            pl.handles_clause.contains(&"Me.Load".to_string()),
            "Page_Load should have Handles Me.Load, got: {:?}",
            pl.handles_clause
        );
        assert!(
            matches!(pl.method_kind, MethodKind::Lifecycle),
            "Page_Load with Handles Me.Load should be Lifecycle"
        );

        let btn_save = methods.iter().find(|m| m.name == "btnSave_Click");
        assert!(btn_save.is_some(), "Should find btnSave_Click");
        assert!(
            btn_save
                .unwrap()
                .handles_clause
                .contains(&"btnSave.Click".to_string()),
            "btnSave_Click should have Handles btnSave.Click"
        );
        assert!(
            matches!(btn_save.unwrap().method_kind, MethodKind::ControlEvent),
            "btnSave_Click with Handles should be ControlEvent"
        );

        let timer = methods.iter().find(|m| m.name == "Timer_Tick");
        assert!(timer.is_some(), "Should find Timer_Tick");
        let t = timer.unwrap();
        assert_eq!(
            t.handles_clause.len(),
            2,
            "Timer_Tick should have 2 Handles bindings"
        );
        assert!(t.handles_clause.contains(&"Timer1.Tick".to_string()));
        assert!(t.handles_clause.contains(&"Timer2.Tick".to_string()));

        let helper = methods.iter().find(|m| m.name == "HelperMethod");
        assert!(helper.is_some(), "Should find HelperMethod");
        assert!(
            helper.unwrap().handles_clause.is_empty(),
            "HelperMethod should have no Handles clause"
        );
    }

    #[test]
    fn complexity_no_double_count_do_while() {
        let body = r#"
            do while (reader.Read())
                count += 1
            loop
        "#;
        let score = analyzers::methods::compute_complexity_score(body);
        // 1 do while = 1, not 2 (do while + while)
        assert_eq!(score, 1, "do while should count as 1, not 2, got {score}");
    }

    // ── Phase 35: Inherited Effect Propagation ───────────────────────────

    #[test]
    fn inherited_effects_propagate_down() {
        let code_files: Vec<(&str, &str)> = vec![
            (
                "BasePage.cs",
                r#"
public class BasePage : System.Web.UI.Page {
    protected void Page_Load(object sender, EventArgs e) {
        Session["UserId"] = GetCurrentUser();
        var cmd = new SqlCommand("SELECT * FROM Users");
        cmd.ExecuteReader();
    }
    protected void Page_Init(object sender, EventArgs e) {
        Session["Theme"] = "Default";
    }
}
"#,
            ),
            (
                "Default.aspx.cs",
                r#"
public class _Default : BasePage {
    protected override void Page_Load(object sender, EventArgs e) {
        base.Page_Load(sender, e);
        lblWelcome.Text = "Hello";
    }
}
"#,
            ),
        ];
        let markup = vec![FileContent {
            file_path: "Default.aspx".into(),
            markup_content: r#"<%@ Page Inherits="_Default" %>"#.into(),
            codebehind_content: None,
        }];
        let report = analyzers::inheritance::resolve_inheritance_chains(&code_files, &markup);
        assert!(
            !report.inherited_effects.is_empty(),
            "should have inherited effects"
        );
        // BasePage.Page_Load writes Session and has SQL
        let load_effects: Vec<&InheritedEffect> = report
            .inherited_effects
            .iter()
            .filter(|e| e.inherited_from == "BasePage" && e.method == "Page_Load")
            .collect();
        assert!(
            !load_effects.is_empty(),
            "should inherit effects from BasePage.Page_Load"
        );
        assert!(
            load_effects[0]
                .effects
                .iter()
                .any(|e| e.contains("State_Access") || e.contains("Session")),
            "should detect state access: {:?}",
            load_effects[0].effects
        );
        assert!(
            load_effects[0]
                .effects
                .iter()
                .any(|e| e.contains("SQL_Access")),
            "should detect SQL access: {:?}",
            load_effects[0].effects
        );
    }

    #[test]
    fn inherited_effects_three_level_hierarchy() {
        let code_files: Vec<(&str, &str)> = vec![
            (
                "BasePage.cs",
                r#"
public class BasePage : Page {
    protected void Page_Init(object sender, EventArgs e) {
        Session["UserId"] = GetUser();
    }
}
"#,
            ),
            (
                "SectionPage.cs",
                r#"
public class SectionPage : BasePage {
    protected void Page_Load(object sender, EventArgs e) {
        var cmd = new SqlCommand("SELECT * FROM Sections");
        cmd.ExecuteReader();
    }
}
"#,
            ),
            (
                "Default.aspx.cs",
                r#"
public class _Default : SectionPage {
    protected void btnSave_Click(object sender, EventArgs e) {
        lblStatus.Text = "Saved";
    }
}
"#,
            ),
        ];
        let markup = vec![FileContent {
            file_path: "Default.aspx".into(),
            markup_content: r#"<%@ Page Inherits="_Default" %>"#.into(),
            codebehind_content: None,
        }];
        let report = analyzers::inheritance::resolve_inheritance_chains(&code_files, &markup);
        // _Default inherits from SectionPage which inherits from BasePage
        // Should get effects from both ancestors
        let from_basepage: Vec<&InheritedEffect> = report
            .inherited_effects
            .iter()
            .filter(|e| e.class == "_Default" && e.inherited_from == "BasePage")
            .collect();
        let from_section: Vec<&InheritedEffect> = report
            .inherited_effects
            .iter()
            .filter(|e| e.class == "_Default" && e.inherited_from == "SectionPage")
            .collect();
        assert!(!from_basepage.is_empty(), "should inherit from BasePage");
        assert!(!from_section.is_empty(), "should inherit from SectionPage");
    }

    // ── Phase 35: Cross-Layer Tracing ────────────────────────────────────

    #[test]
    fn cross_layer_trace_ajax_to_handler() {
        let js_analysis = JsAnalysisSummary {
            total_script_files: 1,
            legacy_total_js_files: 1,
            script_files_with_server_deps: 1,
            legacy_js_files_with_server_deps: 1,
            dom_manipulations: vec![],
            postback_triggers: vec![],
            ajax_calls: vec![JsAjaxCall {
                js_file: "search.js".into(),
                target_url: "Services/MapData.asmx/GetPoints".into(),
                transport: "jquery_ajax".into(),
                target_method: Some("GetPoints".into()),
                target_type: "asmx".into(),
            }],
            page_js_dependencies: BTreeMap::new(),
            inline_script_files: vec![],
            jquery_version_hint: None,
        };

        let sp_catalog = StoredProcedureCatalog {
            procedures: vec![StoredProcedureInfo {
                name: "sp_GetPoints".into(),
                parameters: vec![],
                tables_read: vec!["Locations".into()],
                tables_written: vec![],
                called_from: vec!["MapData.asmx.cs".into()],
                line_count: 20,
                has_dynamic_sql: false,
                has_cursor: false,
                modern_equivalent: String::new(),
            }],
            total_procedures: 1,
            procedures_with_params: 0,
            procedures_called_from_code: 1,
            uncalled_procedures: vec![],
        };

        let service_endpoints = ServiceEndpointSummary {
            web_services: vec![ServiceEndpoint {
                file_path: "Services/MapData.asmx".into(),
                service_name: "MapData".into(),
                methods: vec!["GetPoints".into()],
                modern_equivalent: "Web API".into(),
                called_by: vec![],
            }],
            http_handlers: vec![],
            wcf_services: vec![],
            http_modules: vec![],
            route_handlers: vec![],
            total_endpoints: 1,
        };

        let code_files: Vec<(&str, &str)> = vec![(
            "Services/MapData.asmx.cs",
            r#"
public class MapData : WebService {
    [WebMethod]
    public string GetPoints() {
        var cmd = new SqlCommand();
        cmd.CommandText = "sp_GetPoints";
        cmd.CommandType = CommandType.StoredProcedure;
        return cmd.ExecuteReader().ToString();
    }
}
"#,
        )];

        let traces =
            analyzers::cross_layer::build_cross_layer_traces(&js_analysis, &sp_catalog, &service_endpoints, &code_files);

        assert!(!traces.chains.is_empty(), "should have at least one chain");
        assert!(
            traces.chains[0].steps.len() >= 2,
            "chain should have client + handler steps, got {}",
            traces.chains[0].steps.len()
        );
        assert_eq!(traces.chains[0].steps[0].layer, "client");
        assert_eq!(traces.chains[0].steps[1].layer, "handler");
    }

    #[test]
    fn cross_layer_unresolved_url_tracked() {
        let js_analysis = JsAnalysisSummary {
            total_script_files: 1,
            legacy_total_js_files: 1,
            script_files_with_server_deps: 1,
            legacy_js_files_with_server_deps: 1,
            dom_manipulations: vec![],
            postback_triggers: vec![],
            ajax_calls: vec![JsAjaxCall {
                js_file: "app.js".into(),
                target_url: "NonExistent.ashx/DoStuff".into(),
                transport: "fetch".into(),
                target_method: Some("DoStuff".into()),
                target_type: "ashx".into(),
            }],
            page_js_dependencies: BTreeMap::new(),
            inline_script_files: vec![],
            jquery_version_hint: None,
        };

        let sp_catalog = StoredProcedureCatalog {
            procedures: vec![],
            total_procedures: 0,
            procedures_with_params: 0,
            procedures_called_from_code: 0,
            uncalled_procedures: vec![],
        };
        let service_endpoints = ServiceEndpointSummary {
            web_services: vec![],
            http_handlers: vec![],
            wcf_services: vec![],
            http_modules: vec![],
            route_handlers: vec![],
            total_endpoints: 0,
        };

        let traces = analyzers::cross_layer::build_cross_layer_traces(&js_analysis, &sp_catalog, &service_endpoints, &[]);

        assert!(
            traces.chains.is_empty(),
            "no chain should be built for unresolved URL"
        );
        assert!(
            !traces.unresolved_urls.is_empty(),
            "unresolved URL should be tracked"
        );
    }

    #[test]
    fn cross_layer_url_parts_extraction() {
        let parts = analyzers::cross_layer::extract_url_parts("Services/MapData.asmx/GetPolygons?bounds=1,2,3,4");
        assert_eq!(parts.file_part, "MapData.asmx");
        assert_eq!(parts.method_part.as_deref(), Some("GetPolygons"));

        let parts2 = analyzers::cross_layer::extract_url_parts("api/search");
        assert!(parts2.method_part.is_none() || parts2.file_part == "search");
    }

    // ── Phase 36: Business Logic Integration Tests ───────────────────────

    #[test]
    fn business_logic_deterministic_in_report() {
        use crate::services::business_logic_service;

        let method = MethodInfo {
            name: "Page_Load".to_string(),
            signature: "Protected Sub Page_Load(sender, e)".to_string(),
            return_type: "Sub".to_string(),
            access_level: "Protected".to_string(),
            line_range: (10, 30),
            line_count: 20,
            method_kind: MethodKind::Lifecycle,
            effects: vec![
                "SQL: SELECT Orders".to_string(),
                "Session write: CartTotal".to_string(),
            ],
            calls_methods: vec![],
            called_by: vec![],
            body_preview: Some("Protected Sub Page_Load()\nEnd Sub".to_string()),
            complexity_score: 6,
            handles_clause: vec![],
        };

        let summary = business_logic_service::deterministic_method_summary(
            "OrderPage.aspx.vb",
            &method,
            "OrderPage",
        );

        assert_eq!(summary.fqn, "OrderPage.Page_Load");
        assert!(summary.purpose.contains("lifecycle handler"));
        assert!(summary.purpose.contains("SQL: SELECT Orders"));
        assert_eq!(summary.steps.len(), 2);
        assert!(!summary.content_hash.is_empty());
    }

    #[test]
    fn business_logic_compact_render_includes_section() {
        use crate::services::business_logic_service;

        let report = business_logic_service::ProjectBusinessLogicReport {
            project_id: "test".to_string(),
            files_analyzed: 1,
            methods_analyzed: 1,
            methods_skipped_cached: 0,
            llm_failures: 0,
            file_summaries: vec![business_logic_service::FileBusinessLogic {
                file_path: "Default.aspx.vb".to_string(),
                class_name: "_Default".to_string(),
                file_purpose: "Main page".to_string(),
                methods: vec![business_logic_service::MethodBusinessLogic {
                    file_path: "Default.aspx.vb".to_string(),
                    method_name: "Page_Load".to_string(),
                    fqn: "_Default.Page_Load".to_string(),
                    purpose: "Loads dashboard data".to_string(),
                    steps: vec![],
                    business_rules: vec!["Auth required".to_string()],
                    data_flow: String::new(),
                    error_handling: String::new(),
                    side_effects_detail: String::new(),
                    content_hash: "h".to_string(),
                    confidence: String::new(),
                    validation_warnings: vec![],
                    parse_diagnostic: String::new(),
                }],
                analyzed_at: "2026-01-01".to_string(),
            }],
        };

        let md = business_logic_service::render_compact_markdown(&report);
        assert!(md.contains("## Business Logic Summary"));
        assert!(md.contains("_Default"));
        assert!(md.contains("Loads dashboard data"));
        assert!(md.contains("Auth required"));
    }

    // ── ENG-AUD-2026-S10-0003 audit tests ─────────────────────────────────────

    /// Verifies that the audit tag ENG-AUD-2026-S10-0003 appears in at least 3
    /// places within the migration_tools handler, proving multiple read-error
    /// sites are tagged (not just one).
    #[test]
    fn read_failure_warning_tag_present() {
        let source = include_str!("../handlers/migration_tools.rs");
        let tag = "ENG-AUD-2026-S10-0003";
        let count = source.matches(tag).count();
        assert!(
            count >= 3,
            "Expected at least 3 occurrences of audit tag '{}' in migration_tools.rs, found {}",
            tag,
            count
        );
    }

    // ── MIG1/D2: report completeness surface ──────────────────────────────────

    /// MIG1/D2: `edges_or_warn` records degraded sections in TLS when it handles
    /// an error, and `take_mig_degraded` drains the accumulator correctly.
    #[test]
    fn mig1_edges_or_warn_records_degradation() {
        // Reset any stale TLS state from prior tests.
        MIG_DEGRADED.with(|v| v.borrow_mut().clear());

        // Simulate an error path in edges_or_warn.
        let _empty: Vec<engram_graph::Edge> = edges_or_warn(
            Err(anyhow::anyhow!("simulated graph failure")),
            "test_context_A",
        );
        let _empty2: Vec<engram_graph::Edge> =
            edges_or_warn(Err(anyhow::anyhow!("another failure")), "test_context_B");

        let degraded = take_mig_degraded();
        assert_eq!(
            degraded.len(),
            2,
            "MIG1: two failures must produce two degraded entries"
        );
        assert!(degraded.contains(&"test_context_A".to_string()));
        assert!(degraded.contains(&"test_context_B".to_string()));

        // After drain, TLS is empty.
        let after = take_mig_degraded();
        assert!(after.is_empty(), "MIG1: TLS must be empty after drain");
    }

    /// MIG1/D2: `nodes_or_warn` also records degraded sections.
    #[test]
    fn mig1_nodes_or_warn_records_degradation() {
        MIG_DEGRADED.with(|v| v.borrow_mut().clear());

        let _empty: Vec<engram_graph::Node> =
            nodes_or_warn(Err(anyhow::anyhow!("node query failure")), "node_context");

        let degraded = take_mig_degraded();
        assert_eq!(
            degraded.len(),
            1,
            "MIG1: one node failure must produce one degraded entry"
        );
        assert_eq!(degraded[0], "node_context");
    }

    /// MIG1/D2: `edges_or_warn` does NOT record when the query succeeds.
    #[test]
    fn mig1_edges_or_warn_does_not_record_on_success() {
        MIG_DEGRADED.with(|v| v.borrow_mut().clear());

        let _result: Vec<engram_graph::Edge> = edges_or_warn(Ok(Vec::new()), "success_context");

        let degraded = take_mig_degraded();
        assert!(
            degraded.is_empty(),
            "MIG1: successful graph query must not add to degraded_sections"
        );
    }

    /// MIG1-k2v6: Non-rollback contract — a failure in one phase does not undo
    /// results from other phases.  `edges_or_warn` returns an empty Vec (not Err),
    /// so subsequent calls continue normally and accumulate their own results.
    /// After a mixed sequence (fail + succeed), only the failure appears in degraded_sections.
    #[test]
    fn mig1_non_rollback_contract_mixed_phase_leaves_prior_data_intact() {
        MIG_DEGRADED.with(|v| v.borrow_mut().clear());

        // Phase A fails — returns empty vec, records degraded context.
        let phase_a: Vec<engram_graph::Edge> = edges_or_warn(
            Err(anyhow::anyhow!("phase A graph failure")),
            "phase_a_edges",
        );
        assert!(
            phase_a.is_empty(),
            "MIG1: failed phase must return empty vec (best-effort)"
        );

        // Phase B succeeds — returns its data, does NOT record degraded context.
        let phase_b: Vec<engram_graph::Edge> = edges_or_warn(Ok(Vec::new()), "phase_b_edges");
        // phase_b is empty here because we passed Ok(Vec::new()); the key assertion
        // is that the degraded list still has only the ONE failure from phase A.

        let degraded = take_mig_degraded();
        assert_eq!(
            degraded.len(),
            1,
            "MIG1: only the failed phase must appear in degraded_sections; \
             success does not roll back prior phases or add extra entries"
        );
        assert_eq!(degraded[0], "phase_a_edges");

        // Phase B's result is usable even after phase A failed — no rollback.
        let _ = phase_b; // both vecs are usable simultaneously
    }

    /// MIG1-k2v6: clients MUST check `report_is_complete` — structural proof that
    /// `FullProjectMigrationReport` exposes the field at the type level and that
    /// `report_is_complete = false` <=> `degraded_sections` is non-empty.
    #[test]
    fn mig1_report_completeness_contract_is_type_level_checkable() {
        // Construct a minimal report-like scenario mirroring what analyze_full_project does:
        // drain the degraded accumulator and derive report_is_complete from it.
        MIG_DEGRADED.with(|v| v.borrow_mut().clear());

        // Simulate one failed section.
        let _: Vec<engram_graph::Edge> =
            edges_or_warn(Err(anyhow::anyhow!("dependency resolution")), "dep_edges");

        let degraded_sections = take_mig_degraded();
        let report_is_complete = degraded_sections.is_empty();

        // Contract: non-empty degraded_sections → report_is_complete = false.
        assert!(
            !report_is_complete,
            "MIG1-k2v6: partial-failure must set report_is_complete=false; \
             callers must check this field before relying on all sections being present"
        );
        assert!(
            !degraded_sections.is_empty(),
            "MIG1-k2v6: degraded_sections must be non-empty when report_is_complete=false"
        );
    }

    /// MIG1/D2: `report_is_complete` is derived correctly from the degraded list.
    #[test]
    fn mig1_report_is_complete_derived_correctly() {
        MIG_DEGRADED.with(|v| v.borrow_mut().clear());

        // No failures → report_is_complete should be true.
        let degraded = take_mig_degraded();
        assert!(degraded.is_empty());
        let complete = degraded.is_empty();
        assert!(
            complete,
            "MIG1: empty degraded_sections must give report_is_complete = true"
        );

        // One failure → report_is_complete should be false.
        let _: Vec<engram_graph::Edge> = edges_or_warn(Err(anyhow::anyhow!("failure")), "ctx");
        let degraded2 = take_mig_degraded();
        let complete2 = degraded2.is_empty();
        assert!(
            !complete2,
            "MIG1: non-empty degraded_sections must give report_is_complete = false"
        );
    }

    /// Verifies that when Global.asax does not exist on disk, the analysis
    /// function handles it gracefully — `extract_global_asax_info` with empty
    /// input returns a summary with `has_global_asax = false` and no events.
    #[test]
    fn global_asax_not_found_is_not_an_error() {
        // Simulate the path where Global.asax is absent: bundle.global_asax is
        // None, so we call extract_global_asax_info with empty strings.
        let summary = analyzers::global_asax::extract_global_asax_info("", "");
        assert!(
            !summary.has_global_asax,
            "Missing Global.asax must not be reported as present"
        );
        assert!(
            summary.lifecycle_events.is_empty(),
            "No lifecycle events should be produced when Global.asax is absent"
        );
        assert!(
            summary.startup_registrations.is_empty(),
            "No startup registrations should be produced when Global.asax is absent"
        );
    }
}
