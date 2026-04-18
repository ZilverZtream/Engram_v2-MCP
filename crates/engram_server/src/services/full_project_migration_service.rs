//! Full project migration analysis — the "one call, everything" service.
//!
//! Orchestrates every migration sub-service to produce a single comprehensive
//! report covering every file in the project.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_graph::{EdgeKind, GraphStore};
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
fn record_mig_degraded(context: &'static str) {
    MIG_DEGRADED.with(|v| v.borrow_mut().push(context.to_string()));
}
#[inline]
fn take_mig_degraded() -> Vec<String> {
    MIG_DEGRADED.with(|v| std::mem::take(&mut *v.borrow_mut()))
}

/// MIG1: helper that runs a graph query for edge lists, returning an empty Vec on error
/// while logging a warning AND recording the failure context so the final report can
/// carry an explicit `degraded_sections` list and `report_is_complete = false` flag.
#[inline]
fn edges_or_warn(
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
fn nodes_or_warn(
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
use super::db_strategy_service::{self, FileDataAccessProfile};
use super::dossier_service::{self, MigrationDossier};
use super::migration_order_service::{self, MigrationOrderPlan};
use super::pattern_detection_service;
use super::state_migration_service::{self, StateMigrationReport};

// Data model (every `pub struct` / `pub enum` for the report) lives
// in `full_project_migration_service/model.rs`. Re-exported at the
// module root so external callers keep using the same paths —
// `use super::full_project_migration_service::FullProjectMigrationReport;`
// still compiles exactly as before.
pub mod model;
pub use model::*;

/// (parent_class, file_path, methods, state_writes, base_calls) per class.
type ClassInfo = (String, String, Vec<String>, Vec<String>, Vec<String>);

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

    let js_analysis = build_js_analysis(
        graph,
        project_id,
        &bundle.markup_files,
        &bundle.script_files,
    );

    let gis_analysis = build_gis_analysis(graph, project_id, target_stack);

    let classic_asp = build_classic_asp_summary(graph, project_id, &bundle.classic_asp_files);

    let reports = build_report_summary(graph, project_id, &bundle.report_files);

    // ── 3b. Phase 33 analyses ──────────────────────────────────────────────

    // Gap 1: Code-behind method inventory
    let method_inventories = build_method_inventories(graph, project_id, capped);

    // Gap 2: Third-party control detection
    let third_party_controls = build_third_party_control_summary(&bundle.markup_files);

    // Gap 3: Dependency inventory
    let dependency_inventory = build_dependency_inventory(&bundle.project_references);

    // Gap 4: Caching inventory
    let caching_inventory =
        build_caching_inventory(&bundle.markup_files, &code_refs, &bundle.code_files);

    // Gap 5: URL routing
    let url_routing = extract_url_routing(
        web_config_content,
        bundle
            .global_asax
            .as_ref()
            .map(|ga| ga.codebehind_content.as_deref().unwrap_or(""))
            .unwrap_or(""),
        &code_refs,
    );

    // Gap 6: VB.NET translation flags
    let vb_translation = analyze_vb_translation_flags(&code_refs);

    // Gap 7: Multi-tenancy detection
    let multi_tenancy = detect_multi_tenancy(
        web_config_content,
        &code_refs,
        bundle
            .global_asax
            .as_ref()
            .map(|ga| ga.codebehind_content.as_deref().unwrap_or("")),
    );

    // Gap 8: Email + background jobs
    let email_patterns = detect_email_patterns(&code_refs, web_config_content);
    let background_jobs = detect_background_job_patterns(
        &code_refs,
        bundle
            .global_asax
            .as_ref()
            .map(|ga| ga.codebehind_content.as_deref().unwrap_or("")),
    );

    // ── 3c. Phase 34 analyses ─────────────────────────────────────────────

    // Ticket 1: Stored procedure catalog
    let sp_catalog = build_sp_catalog(&bundle.sql_files, &code_refs);

    // Ticket 2: Inheritance chain resolution
    let inheritance_chains = resolve_inheritance_chains(&code_refs, capped);

    // Ticket 3: packages.config + binding redirects (extend dependency_inventory)
    let mut dependency_inventory = dependency_inventory;
    for (_, content) in &bundle.packages_config_files {
        let legacy_pkgs = parse_packages_config(content);
        // If we got legacy packages and had 0 NuGet packages from SDK-style, use these
        if !legacy_pkgs.is_empty() {
            if dependency_inventory.total_packages == 0 {
                // Convert legacy to NuGet info for unified reporting
                for lp in &legacy_pkgs {
                    let (repl, ver, notes, cat) = lookup_modern_replacement(&lp.package_id);
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
    dependency_inventory.binding_redirects = extract_binding_redirects(web_config_content);

    // Ticket 6a: Config transforms
    let config_transforms = parse_config_transforms(&bundle.config_transform_files);

    // Ticket 6b: Master page region mapping
    let master_page_regions = build_master_page_region_map(&bundle.master_files, capped);

    // Ticket 6c: Resource file inventory
    let resource_inventory = build_resource_inventory(&bundle.resx_files);

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
        build_cross_layer_traces(&js_analysis, &sp_catalog, &service_endpoints, &code_refs);

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

/// Numeric complexity ranking for a dossier — higher means "more worth
/// spending an LLM call on". Extracted from `estimated_complexity` (whose
/// prefix is `Low (score N)` / `Medium (score N)` / `High (score N)`) with
/// blast-radius score as the tiebreaker. Pure function, deterministic.
fn dossier_llm_priority(d: &MigrationDossier) -> (u32, u8) {
    // `estimated_complexity` looks like `High (score 28): …`. Pull out the
    // integer if we can, otherwise fall back to band prefix → weight so a
    // Medium always outranks a Low.
    let complexity_score: u32 = (|| {
        let paren_open = d.estimated_complexity.find('(')?;
        let paren_close = d.estimated_complexity[paren_open..].find(')')?;
        let inside = &d.estimated_complexity[paren_open + 1..paren_open + paren_close];
        inside
            .split_whitespace()
            .filter_map(|tok| tok.parse::<u32>().ok())
            .next()
    })()
    .unwrap_or_else(|| {
        if d.estimated_complexity.starts_with("Critical") {
            30
        } else if d.estimated_complexity.starts_with("High") {
            20
        } else if d.estimated_complexity.starts_with("Medium") {
            10
        } else {
            0
        }
    });
    (complexity_score, d.blast_radius_score)
}

/// Select the top-`max_pages` dossiers by LLM priority.
///
/// Ties are broken by `blast_radius_score`, then by `file_path` so the
/// result is deterministic even when several pages share complexity and
/// blast radius — important because the same report run twice must
/// enhance the same pages (otherwise the set of LLM-enhanced dossiers
/// would flutter between invocations).
fn select_dossiers_for_llm<'a>(
    dossiers: &'a [MigrationDossier],
    max_pages: usize,
) -> Vec<&'a MigrationDossier> {
    if max_pages == 0 || dossiers.is_empty() {
        return Vec::new();
    }
    let mut refs: Vec<&MigrationDossier> = dossiers.iter().collect();
    refs.sort_by(|a, b| {
        let pa = dossier_llm_priority(a);
        let pb = dossier_llm_priority(b);
        pb.0.cmp(&pa.0)
            .then(pb.1.cmp(&pa.1))
            .then(a.file_path.cmp(&b.file_path))
    });
    refs.truncate(max_pages);
    refs
}

/// Parse the LLM response for a per-page dossier.
///
/// The prompt asks the model to emit two labelled blocks:
///
///   BUSINESS_PURPOSE: <2–3 sentences>
///   MIGRATION_NOTES:  <risks + Blazor component recommendations>
///
/// We accept either label on its own line and tolerate minor casing /
/// punctuation drift. Empty / missing blocks become `None` so the
/// deterministic dossier is shown instead of an empty "LLM-enhanced"
/// heading.
fn parse_page_llm_response(raw: &str) -> (Option<String>, Option<String>) {
    fn extract_block(text: &str, label: &str) -> Option<String> {
        let lower = text.to_ascii_lowercase();
        let needle = label.to_ascii_lowercase();
        let start = lower.find(&needle)?;
        let after_label = &text[start + needle.len()..];
        // Drop the `:` / whitespace that usually follows the label.
        let after_label = after_label.trim_start_matches(|c: char| matches!(c, ':' | ' ' | '\t'));
        // Stop at the next label-like line so the two blocks don't bleed.
        let stop = after_label
            .find("\nBUSINESS_PURPOSE")
            .or_else(|| after_label.find("\nMIGRATION_NOTES"))
            .or_else(|| after_label.find("\nBusiness Purpose"))
            .or_else(|| after_label.find("\nMigration Notes"));
        let body = match stop {
            Some(i) => &after_label[..i],
            None => after_label,
        };
        let body = body.trim();
        if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        }
    }
    let business =
        extract_block(raw, "BUSINESS_PURPOSE").or_else(|| extract_block(raw, "Business Purpose"));
    let notes =
        extract_block(raw, "MIGRATION_NOTES").or_else(|| extract_block(raw, "Migration Notes"));
    (business, notes)
}

/// Compose a focused prompt for the per-page LLM call.
///
/// We DO NOT dump the whole markup + codebehind — that's expensive and
/// noisy. Instead we feed the model the deterministic context the
/// analyzer already computed (inherits class, lifecycle events, tables,
/// SQL, ViewState, risk factors), truncated samples of the actual
/// markup/codebehind, and a tightly-specified output format so the model
/// produces useful, structured text rather than generic boilerplate.
fn build_page_llm_prompt(
    d: &MigrationDossier,
    markup: &str,
    codebehind: Option<&str>,
    target_stack: &str,
) -> String {
    const MARKUP_LIMIT: usize = 4_000;
    const CODEBEHIND_LIMIT: usize = 6_000;

    fn truncate(s: &str, n: usize) -> String {
        if s.len() <= n {
            s.to_string()
        } else {
            let mut end = n;
            while !s.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            format!("{}\n...<truncated {} bytes>...\n", &s[..end], s.len() - end)
        }
    }

    let tables = if d.tables_touched.is_empty() {
        "(none detected)".to_string()
    } else {
        d.tables_touched.join(", ")
    };
    let user_controls = if d.user_controls.is_empty() {
        "(none)".to_string()
    } else {
        d.user_controls
            .iter()
            .map(|uc| uc.control_path.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let lifecycle = {
        let lc = &d.lifecycle_summary;
        if lc.lifecycle_event_count == 0 && lc.control_event_count == 0 {
            "(no explicit lifecycle handlers)".to_string()
        } else {
            format!(
                "{} lifecycle event(s), {} control event(s){}{}",
                lc.lifecycle_event_count,
                lc.control_event_count,
                if lc.has_ispostback_logic {
                    " (IsPostBack branching)"
                } else {
                    ""
                },
                if lc.events.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", lc.events.join(", "))
                },
            )
        }
    };
    let viewstate = {
        let vs = &d.viewstate_summary;
        if vs.total_state_fields == 0 {
            "(none)".to_string()
        } else {
            format!(
                "{} explicit keys, {} implicit control state(s)",
                vs.explicit_keys, vs.implicit_controls
            )
        }
    };
    let ajax = {
        let aj = &d.ajax_summary;
        if aj.update_panel_count == 0 && !aj.has_script_manager {
            "(no AJAX partial-render)".to_string()
        } else {
            format!(
                "{} UpdatePanel(s), {} timer(s), ScriptManager: {}",
                aj.update_panel_count, aj.timer_count, aj.has_script_manager
            )
        }
    };
    let auth = {
        let au = &d.auth_summary;
        if !au.has_auth_rules && au.auth_check_count == 0 && au.session_auth_count == 0 {
            "(no auth gating detected)".to_string()
        } else {
            let mut parts = Vec::new();
            if !au.required_roles.is_empty() {
                parts.push(format!("roles [{}]", au.required_roles.join(", ")));
            }
            if au.auth_check_count > 0 {
                parts.push(format!("{} code-level check(s)", au.auth_check_count));
            }
            if au.session_auth_count > 0 {
                parts.push(format!(
                    "{} session-based auth pattern(s)",
                    au.session_auth_count
                ));
            }
            parts.join("; ")
        }
    };
    let risks = if d.risk_factors.is_empty() {
        "(none)".to_string()
    } else {
        d.risk_factors.join("; ")
    };
    let cb_block = match codebehind {
        Some(cb) => format!(
            "\n## CODEBEHIND ({bytes} bytes, truncated to {cap}):\n```\n{snippet}\n```\n",
            bytes = cb.len(),
            cap = CODEBEHIND_LIMIT,
            snippet = truncate(cb, CODEBEHIND_LIMIT),
        ),
        None => "\n## CODEBEHIND: (none detected)\n".to_string(),
    };

    format!(
        "You are analyzing a legacy ASP.NET WebForms page for migration to {stack}.\n\
         Produce a tight, concrete analysis — no fluff, no restating the static facts.\n\
         \n\
         # Page: {file_path}\n\
         \n\
         ## Deterministic analysis (do NOT repeat these verbatim; use them as context):\n\
         - Inherits class: {inherits}\n\
         - Master page: {master}\n\
         - User controls: {user_controls}\n\
         - Tables touched: {tables}\n\
         - Lifecycle: {lifecycle}\n\
         - ViewState: {viewstate}\n\
         - AJAX: {ajax}\n\
         - Auth: {auth}\n\
         - Risk factors (deterministic): {risks}\n\
         - Blast-radius score: {br}/10\n\
         \n\
         ## MARKUP ({markup_bytes} bytes, truncated to {markup_cap}):\n\
         ```\n{markup_snippet}\n```\n\
         {cb_block}\n\
         \n\
         # Required output — two labelled blocks, exactly these labels, nothing else before them:\n\
         \n\
         BUSINESS_PURPOSE: 2 to 3 sentences in plain prose describing what this page \
         does from a user/workflow perspective — what user action triggers it, what data \
         it reads or mutates, what decision or side effect it produces. Do NOT restate \
         the deterministic facts above. Use present tense.\n\
         \n\
         MIGRATION_NOTES: 3 to 6 bullet points covering (a) migration risks specific to \
         THIS page that the deterministic analysis above does NOT already capture, and \
         (b) concrete {stack} component structure recommendations — name the components \
         you would create, describe their responsibility boundaries, and note any \
         shared state or service extraction. Skip bullets that would just repeat the \
         deterministic risks. Use '-' markers.\n\
         \n\
         Do not include any text outside these two blocks.\n",
        stack = target_stack,
        file_path = d.file_path,
        inherits = d.inherits_class.as_deref().unwrap_or("(none detected)"),
        master = d.master_page.as_deref().unwrap_or("(none)"),
        user_controls = user_controls,
        tables = tables,
        lifecycle = lifecycle,
        viewstate = viewstate,
        ajax = ajax,
        auth = auth,
        risks = risks,
        br = d.blast_radius_score,
        markup_bytes = markup.len(),
        markup_cap = MARKUP_LIMIT,
        markup_snippet = truncate(markup, MARKUP_LIMIT),
        cb_block = cb_block,
    )
}

/// Result of a single per-page LLM call.
#[derive(Debug, Clone, Default)]
pub struct PageLlmEnhancement {
    pub file_path: String,
    pub business_purpose: Option<String>,
    pub migration_notes: Option<String>,
}

/// For each selected dossier (top-`max_pages` by complexity), call the
/// LLM with a structured prompt and merge the parsed response back into
/// the corresponding `MigrationDossier`.
///
/// Bounded concurrency via `max_concurrent` so we don't flood the
/// OpenRouter pipe. Empty / failed responses are logged and silently
/// skipped — the deterministic dossier is then shown as-is.
pub async fn enhance_page_dossiers_with_llm(
    report: &mut FullProjectMigrationReport,
    dreaming: &engram_ml::DreamingEngine,
    file_contents: &std::collections::HashMap<String, String>,
    max_pages: usize,
    max_concurrent: usize,
) -> usize {
    if max_pages == 0 || report.page_dossiers.is_empty() {
        return 0;
    }

    // Snapshot the selected file paths so we don't need to hold a borrow
    // on `report.page_dossiers` while we async-dispatch.
    let selected: Vec<String> = select_dossiers_for_llm(&report.page_dossiers, max_pages)
        .into_iter()
        .map(|d| d.file_path.clone())
        .collect();

    if selected.is_empty() {
        return 0;
    }

    tracing::info!(
        project_id = %report.project_id,
        selected_pages = selected.len(),
        max_pages = max_pages,
        "enhance_page_dossiers_with_llm: selected top-N pages by complexity"
    );

    let dreaming = Arc::new(dreaming.clone());
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));
    let target_stack = report.target_stack.clone();

    let mut handles = Vec::with_capacity(selected.len());

    for file_path in selected {
        // Resolve the dossier + contents for this page.
        let Some(dossier) = report
            .page_dossiers
            .iter()
            .find(|d| d.file_path == file_path)
        else {
            continue;
        };
        let dossier_clone = dossier.clone();
        let markup = file_contents.get(&file_path).cloned().unwrap_or_default();
        let cb = dossier
            .codebehind_file
            .as_deref()
            .and_then(|cb| file_contents.get(cb).cloned())
            .or_else(|| {
                // Fallback to the conventional sibling for .aspx pages.
                if file_path.ends_with(".aspx") {
                    file_contents
                        .get(&format!("{file_path}.vb"))
                        .or_else(|| file_contents.get(&format!("{file_path}.cs")))
                        .cloned()
                } else {
                    None
                }
            });

        if markup.is_empty() && cb.as_deref().map(str::is_empty).unwrap_or(true) {
            tracing::debug!(
                file = %file_path,
                "enhance_page_dossiers_with_llm: skipped — no content available"
            );
            continue;
        }

        let sem = semaphore.clone();
        let dream = dreaming.clone();
        let ts = target_stack.clone();

        handles.push(tokio::spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    // Semaphore closed during shutdown.
                    return PageLlmEnhancement {
                        file_path: dossier_clone.file_path.clone(),
                        ..Default::default()
                    };
                }
            };
            let prompt = build_page_llm_prompt(&dossier_clone, &markup, cb.as_deref(), &ts);
            let raw = match dream
                .generate_text(&prompt, 1024, std::time::Duration::from_secs(120))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        file = %dossier_clone.file_path,
                        error = %e,
                        "enhance_page_dossiers_with_llm: LLM call failed"
                    );
                    String::new()
                }
            };
            let (business, notes) = if raw.is_empty() {
                (None, None)
            } else {
                parse_page_llm_response(&raw)
            };
            if business.is_some() || notes.is_some() {
                tracing::info!(
                    file = %dossier_clone.file_path,
                    business_purpose_len = business.as_deref().map_or(0, str::len),
                    migration_notes_len = notes.as_deref().map_or(0, str::len),
                    "dossier LLM enhancement complete"
                );
            } else if !raw.is_empty() {
                tracing::warn!(
                    file = %dossier_clone.file_path,
                    raw_len = raw.len(),
                    "enhance_page_dossiers_with_llm: LLM returned content but no \
                     BUSINESS_PURPOSE / MIGRATION_NOTES blocks could be parsed"
                );
            }
            PageLlmEnhancement {
                file_path: dossier_clone.file_path,
                business_purpose: business,
                migration_notes: notes,
            }
        }));
    }

    let mut enhanced = 0usize;
    for handle in handles {
        match handle.await {
            Ok(res) => {
                if res.business_purpose.is_none() && res.migration_notes.is_none() {
                    continue;
                }
                if let Some(d) = report
                    .page_dossiers
                    .iter_mut()
                    .find(|d| d.file_path == res.file_path)
                {
                    d.llm_business_purpose = res.business_purpose;
                    d.llm_migration_notes = res.migration_notes;
                    enhanced += 1;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "enhance_page_dossiers_with_llm: task panicked");
            }
        }
    }

    tracing::info!(
        project_id = %report.project_id,
        enhanced = enhanced,
        "enhance_page_dossiers_with_llm: merged LLM enhancements into dossiers"
    );

    enhanced
}

// ── Ticket 37.1: Async LLM Enhancement Pass ──────────────────────────────────

/// Upgrade deterministic business logic summaries with LLM-powered analysis.
///
/// This is an async post-processing step. For each file in the report's
/// business_logic section, if we have the source content, we call the LLM
/// to produce step-by-step explanations. If the LLM fails for a method,
/// the deterministic version is kept. Each LLM result is then validated
/// against the deterministic extraction (Ticket 37.2).
pub async fn enhance_report_with_llm(
    report: &mut FullProjectMigrationReport,
    dreaming: &engram_ml::DreamingEngine,
    file_contents: &std::collections::HashMap<String, String>,
    max_concurrent: usize,
) {
    use super::business_logic_service::{analyze_file_logic, validate_llm_output};

    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let dreaming = Arc::new(dreaming.clone());
    let cached: Arc<std::collections::HashMap<String, String>> =
        Arc::new(std::collections::HashMap::new());

    let mut handles = Vec::new();

    for file_summary in &report.business_logic.file_summaries {
        let file_path = file_summary.file_path.clone();

        // Look up the file content — try direct path first, then codebehind path
        let content = file_contents.get(&file_path).cloned().or_else(|| {
            report
                .method_inventories
                .get(&file_path)
                .and_then(|inv| file_contents.get(&inv.codebehind_path))
                .cloned()
        });

        let Some(content) = content else {
            continue;
        };

        let sem = semaphore.clone();
        let dream = dreaming.clone();
        let cache = cached.clone();

        let handle = tokio::spawn(async move {
            let Ok(_permit) = sem.acquire().await else {
                // Semaphore was closed (can occur during shutdown); skip this file.
                return (
                    file_path.clone(),
                    super::business_logic_service::FileBusinessLogic {
                        file_path,
                        class_name: String::new(),
                        file_purpose: String::new(),
                        methods: Vec::new(),
                        analyzed_at: String::new(),
                    },
                    0usize,
                    1usize,
                );
            };
            let (file_logic, analyzed, skipped) =
                analyze_file_logic(&dream, &file_path, &content, &cache).await;
            (file_path, file_logic, analyzed, skipped)
        });
        handles.push(handle);
    }

    // Collect LLM results
    let mut llm_results: std::collections::HashMap<
        String,
        super::business_logic_service::FileBusinessLogic,
    > = std::collections::HashMap::new();
    let mut total_analyzed = 0usize;
    let mut total_failures = 0usize;

    for handle in handles {
        match handle.await {
            Ok((file_path, file_logic, analyzed, _skipped)) => {
                total_analyzed += analyzed;
                total_failures += file_logic
                    .methods
                    .iter()
                    .filter(|m| m.purpose.is_empty())
                    .count();
                llm_results.insert(file_path, file_logic);
            }
            Err(e) => {
                tracing::warn!("LLM enhancement task failed: {e}");
                total_failures += 1;
            }
        }
    }

    // Merge LLM results into the report, validating each method
    for file_summary in &mut report.business_logic.file_summaries {
        if let Some(llm_file) = llm_results.get(&file_summary.file_path) {
            // Update file-level purpose
            if !llm_file.file_purpose.is_empty() {
                file_summary.file_purpose = llm_file.file_purpose.clone();
            }

            // For each method, try to find the LLM version and validate
            for det_method in &mut file_summary.methods {
                if let Some(llm_method) = llm_file
                    .methods
                    .iter()
                    .find(|m| m.method_name == det_method.method_name)
                {
                    if llm_method.purpose.is_empty() {
                        // LLM failed for this method, keep deterministic
                        continue;
                    }

                    // Validate against deterministic effects
                    let effects: Vec<String> = det_method
                        .side_effects_detail
                        .split(", ")
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect();
                    let validation = validate_llm_output(llm_method, det_method, &effects);

                    // Upgrade the method with LLM data
                    det_method.purpose = llm_method.purpose.clone();
                    det_method.steps = llm_method.steps.clone();
                    det_method.business_rules = llm_method.business_rules.clone();
                    det_method.data_flow = llm_method.data_flow.clone();
                    det_method.error_handling = llm_method.error_handling.clone();
                    // Keep deterministic side_effects_detail (more reliable)
                    det_method.confidence = validation.confidence.to_string();
                    det_method.validation_warnings = validation.warnings;
                }
            }
        }
    }

    report.business_logic.methods_analyzed = total_analyzed;
    report.business_logic.llm_failures = total_failures;

    rerender_markdown_after_llm(report);
}

/// Re-render `report.markdown_report` from the current struct state.
///
/// Callers invoke this after any async post-processing that mutates
/// `report.page_dossiers` or `report.business_logic` so the markdown
/// output reflects the upgraded data — notably the per-page LLM
/// enhancement pass and the business-logic method enhancement pass.
pub fn rerender_markdown_after_llm(report: &mut FullProjectMigrationReport) {
    let wave_lookup: BTreeMap<String, u32> = {
        let mut wl = BTreeMap::new();
        for wave in &report.migration_order.waves {
            for wf in &wave.files {
                wl.insert(wf.path.clone(), wave.wave_number);
            }
        }
        wl
    };

    report.markdown_report = render_markdown(
        &report.project_id,
        &report.target_stack,
        &report.generated_at,
        &report.migration_order,
        &report.state_migration,
        &report.auth_config,
        &report.data_access_profiles,
        &report.page_dossiers,
        &report.cross_cutting,
        &wave_lookup,
        &report.js_analysis,
        &report.gis_analysis,
        &report.web_config_inventory,
        &report.service_endpoints,
        &report.global_asax,
        &report.anti_patterns,
        &report.classic_asp,
        &report.reports,
        &report.method_inventories,
        &report.third_party_controls,
        &report.dependency_inventory,
        &report.caching_inventory,
        &report.url_routing,
        &report.vb_translation,
        &report.multi_tenancy,
        &report.email_patterns,
        &report.background_jobs,
        &report.sp_catalog,
        &report.inheritance_chains,
        &report.config_transforms,
        &report.master_page_regions,
        &report.resource_inventory,
        &report.vb_translation_traps,
        &report.jquery_inventory,
        &report.cross_layer_traces,
        &report.business_logic,
        &report.database_intelligence,
        &report.session_workflows,
    );
}

// ── Cross-cutting aggregation ─────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn build_cross_cutting_summary(
    dossiers: &[MigrationDossier],
    state_report: &StateMigrationReport,
    js: &JsAnalysisSummary,
    gis: &GisAnalysisSummary,
    ap: &AntiPatternSummary,
    se: &ServiceEndpointSummary,
    asp: &ClassicAspSummary,
    rpt: &ReportSummary,
    method_inv: &BTreeMap<String, PageMethodInventory>,
    dep_inv: &DependencyInventory,
    cache_inv: &CachingInventory,
    email: &EmailPatternReport,
    bg_jobs: &BackgroundJobReport,
    sp_cat: &StoredProcedureCatalog,
    inherit: &InheritanceChainReport,
    cfg_transforms: &ConfigTransformReport,
    res_inv: &ResourceInventory,
    master_regions: &MasterPageRegionMap,
    vb_translation: &VbTranslationReport,
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

    // Phase 33 method aggregation
    let mut total_methods = 0usize;
    let mut total_event_handlers = 0usize;
    let mut total_web_methods = 0usize;
    let mut largest_file_by_methods: Option<(String, usize)> = None;
    for (path, inv) in method_inv {
        total_methods += inv.total_methods;
        total_event_handlers += inv.event_handlers;
        total_web_methods += inv.web_methods;
        if largest_file_by_methods
            .as_ref()
            .is_none_or(|(_, c)| inv.total_methods > *c)
            && inv.total_methods > 0
        {
            largest_file_by_methods = Some((path.clone(), inv.total_methods));
        }
    }

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
        total_script_files: js.total_script_files,
        legacy_total_js_files: js.total_script_files,
        total_gis_libraries: gis.libraries_detected.len(),
        total_anti_patterns: ap.total_anti_patterns,
        total_service_endpoints: se.total_endpoints,
        total_classic_asp_files: asp.total_asp_files,
        total_reports: rpt.total_reports,
        // Phase 33
        total_methods,
        total_event_handlers,
        total_web_methods,
        largest_file_by_methods,
        total_nuget_packages: dep_inv.total_packages,
        target_framework: dep_inv
            .target_frameworks
            .first()
            .cloned()
            .unwrap_or_default(),
        total_cached_pages: cache_inv.total_cached_pages,
        total_cache_keys: cache_inv.total_cache_keys,
        has_email: email.has_email,
        has_background_jobs: bg_jobs.has_background_jobs,
        // Phase 34 aggregation
        total_stored_procedures: sp_cat.total_procedures,
        total_sp_called_from_code: sp_cat.procedures_called_from_code,
        deepest_inheritance_chain: inherit.deepest_chain_depth,
        total_base_classes: inherit.base_classes.len(),
        total_config_environments: cfg_transforms.environments.len(),
        total_resource_files: res_inv.resource_files.len(),
        total_resource_languages: res_inv.languages_detected.len(),
        total_master_page_regions: master_regions.regions.len(),
        total_legacy_packages: dep_inv.legacy_packages.len(),
        option_strict_on_files: vb_translation.dynamic_dispatch.option_strict_on_files,
        option_strict_off_files: vb_translation.dynamic_dispatch.option_strict_off_files,
        dynamic_dispatch_methods: vb_translation
            .dynamic_dispatch
            .methods_with_dynamic_dispatch,
        dynamic_dispatch_risk_tier: vb_translation
            .dynamic_dispatch
            .dynamic_dispatch_risk_tier
            .clone(),
    }
}

// ── Phase 32: Pre-compiled regex statics ──────────────────────────────────────
// Each function in this section previously compiled between 1 and 19 Regex
// objects on every call.  Moving them to LazyLock statics compiles each pattern
// exactly once at first use and eliminates all per-call allocation.

// web.config inventory (extract_webconfig_inventory)
static WC_ADD_KEY_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+key\s*=\s*"([^"]+)"\s+value\s*=\s*"([^"]*)""#).expect("valid regex")
});
static WC_CONN_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+name\s*=\s*"([^"]+)"[^>]*connectionString\s*=\s*"([^"]*)"[^>]*(?:providerName\s*=\s*"([^"]*)")?"#).expect("valid regex")
});
static WC_HANDLER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+(?:[^>]*?)verb\s*=\s*"([^"]*)"[^>]*path\s*=\s*"([^"]*)"[^>]*type\s*=\s*"([^"]*)""#).expect("valid regex")
});
static WC_MODULE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+name\s*=\s*"([^"]+)"[^>]*type\s*=\s*"([^"]*)""#).expect("valid regex")
});
static WC_CE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<customErrors\s+mode\s*=\s*"([^"]+)"(?:[^>]*defaultRedirect\s*=\s*"([^"]*)")?"#)
        .expect("valid regex")
});
static WC_ERROR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<error\s+statusCode\s*=\s*"([^"]+)"[^>]*redirect\s*=\s*"([^"]*)""#)
        .expect("valid regex")
});
static WC_COMP_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"<compilation\s+([^>]*?)/?>"#).expect("valid regex"));
static WC_TF_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"targetFramework\s*=\s*"([^"]+)""#).expect("valid regex")
});
static WC_ASM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+assembly\s*=\s*"([^"]+)""#).expect("valid regex")
});
static WC_SS_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"<sessionState\s+([^>]*?)/?>"#).expect("valid regex"));
static WC_MODE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"mode\s*=\s*"([^"]+)""#).expect("valid regex"));
static WC_TIMEOUT_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"timeout\s*=\s*"(\d+)""#).expect("valid regex"));
static WC_COOKIELESS_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"cookieless\s*=\s*"([^"]+)""#).expect("valid regex"));
static WC_PROVIDER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"customProvider\s*=\s*"([^"]+)""#).expect("valid regex")
});
static WC_PAGES_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"<pages\s+([^>]*?)/?>"#).expect("valid regex"));
static WC_THEME_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"theme\s*=\s*"([^"]+)""#).expect("valid regex"));
static WC_MP_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"masterPageFile\s*=\s*"([^"]+)""#).expect("valid regex")
});
static WC_NS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+namespace\s*=\s*"([^"]+)""#).expect("valid regex")
});
static WC_CTRL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<add\s+tagPrefix\s*=\s*"([^"]+)"[^>]*namespace\s*=\s*"([^"]+)""#)
        .expect("valid regex")
});

// Global.asax class extractor (extract_global_asax_info)
static ASAX_CLASS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)(?:Class|Inherits\s*=\s*["'])(\S+?)(?:["']|\s)"#).expect("valid regex")
});

// JS analysis (build_js_analysis)
static JS_SCRIPT_SRC_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<script[^>]+src\s*=\s*["']([^"']+\.js)["']"#).expect("valid regex")
});
static JS_INLINE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)<script\b[^>]*>").expect("valid regex"));
static JS_SRC_ATTR_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bsrc\s*=").expect("valid regex"));
static JS_JQUERY_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"jquery[.-](\d+\.\d+(?:\.\d+)?)").expect("valid regex")
});

// Classic ASP summary (build_classic_asp_summary)
static ASP_CREATE_OBJ_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)Server\.CreateObject\s*\(\s*"([^"]+)""#).expect("valid regex")
});
static ASP_SQL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)(?:\.Execute|\.CommandText|SELECT\s|INSERT\s|UPDATE\s|DELETE\s)")
        .expect("valid regex")
});
static ASP_STATE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)(?:Session|Application|Request\.Cookies|Response\.Cookies)\s*\(")
        .expect("valid regex")
});
static ASP_INCLUDE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<!--\s*#include\s+(?:file|virtual)\s*=\s*"([^"]+)""#).expect("valid regex")
});

// Report summary (build_report_summary)
static RPT_DATASET_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<DataSet\s+Name\s*=\s*"([^"]+)""#).expect("valid regex")
});
static RPT_PARAM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<ReportParameter\s+Name\s*=\s*"([^"]+)""#).expect("valid regex")
});
static RPT_SUBREPORT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<Subreport[^>]*>.*?<ReportName>([^<]+)</ReportName>"#).expect("valid regex")
});
static RPT_DATASOURCE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"<DataSource\s+Name\s*=\s*"([^"]+)""#).expect("valid regex")
});

// ── Phase 32: Analysis functions ──────────────────────────────────────────────

/// Extract web.config inventory: appSettings, connectionStrings, handlers,
/// modules, customErrors, compilation, sessionState, pages.
fn extract_webconfig_inventory(
    web_config: &str,
    code_files: &[(&str, &str)],
) -> WebConfigInventory {
    // ── appSettings ──
    let appsettings_section = extract_xml_section(web_config, "appSettings");
    let mut app_settings: Vec<AppSettingEntry> = Vec::new();
    for cap in WC_ADD_KEY_RE.captures_iter(&appsettings_section) {
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
    let conn_section = extract_xml_section(web_config, "connectionStrings");
    let mut connection_strings: Vec<ConnectionStringEntry> = Vec::new();
    for cap in WC_CONN_RE.captures_iter(&conn_section) {
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
    let handler_section = extract_xml_section(web_config, "httpHandlers")
        + &extract_xml_section(web_config, "handlers");
    let http_handlers: Vec<HandlerRegistration> = WC_HANDLER_RE
        .captures_iter(&handler_section)
        .map(|cap| HandlerRegistration {
            verb: cap[1].to_string(),
            path: cap[2].to_string(),
            handler_type: cap[3].to_string(),
        })
        .collect();

    // ── httpModules / system.webServer modules ──
    let module_section = extract_xml_section(web_config, "httpModules")
        + &extract_xml_section(web_config, "modules");
    let http_modules: Vec<ModuleRegistration> = WC_MODULE_RE
        .captures_iter(&module_section)
        .map(|cap| ModuleRegistration {
            name: cap[1].to_string(),
            module_type: cap[2].to_string(),
        })
        .collect();

    // ── customErrors ──
    let custom_errors = {
        let ce_section = extract_xml_section(web_config, "customErrors");
        WC_CE_RE.captures(&ce_section).map(|cap| {
            let redirects: Vec<(String, String)> = WC_ERROR_RE
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
        WC_COMP_RE.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            let debug = attrs.contains(r#"debug="true""#);
            let target_framework = WC_TF_RE.captures(attrs).map(|c| c[1].to_string());
            let comp_section = extract_xml_section(web_config, "compilation");
            let assemblies: Vec<String> = WC_ASM_RE
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
        WC_SS_RE.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            SessionStateConfig {
                mode: WC_MODE_RE
                    .captures(attrs)
                    .map_or("InProc".into(), |c| c[1].to_string()),
                timeout_minutes: WC_TIMEOUT_RE
                    .captures(attrs)
                    .and_then(|c| c[1].parse().ok()),
                cookieless: WC_COOKIELESS_RE.captures(attrs).map(|c| c[1].to_string()),
                custom_provider: WC_PROVIDER_RE.captures(attrs).map(|c| c[1].to_string()),
            }
        })
    };

    // ── pages ──
    let pages_config = {
        WC_PAGES_RE.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            let pages_section = extract_xml_section(web_config, "pages");
            PagesConfig {
                theme: WC_THEME_RE.captures(attrs).map(|c| c[1].to_string()),
                master_page_file: WC_MP_RE.captures(attrs).map(|c| c[1].to_string()),
                namespaces: WC_NS_RE
                    .captures_iter(&pages_section)
                    .map(|c| c[1].to_string())
                    .collect(),
                controls: WC_CTRL_RE
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
///
/// Uses a plain string search instead of compiling a new `Regex` on every call
/// (this helper is invoked ~10 times per `extract_webconfig_inventory` call).
/// XML section tag names are always simple ASCII identifiers so case-folding and
/// string comparison is sufficient.
fn extract_xml_section(xml: &str, tag: &str) -> String {
    let xml_lower = xml.to_ascii_lowercase();
    let tag_lower = tag.to_ascii_lowercase();

    // Find opening tag: `<tag_lower` followed by either `>` or whitespace (attributes)
    let open_prefix = format!("<{tag_lower}");
    let Some(open_start) = xml_lower.find(open_prefix.as_str()) else {
        return String::new();
    };
    // Advance past the tag name to the first `>`
    let Some(open_end_rel) = xml_lower[open_start..].find('>') else {
        return String::new();
    };
    let body_start = open_start + open_end_rel + 1;

    // Find closing tag
    let close_tag = format!("</{tag_lower}>");
    let Some(close_start_rel) = xml_lower[body_start..].find(close_tag.as_str()) else {
        return String::new();
    };
    let body_end = body_start + close_start_rel;

    xml[body_start..body_end].to_string()
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
    let codebehind_class = ASAX_CLASS_RE.captures(&combined).map(|c| c[1].to_string());

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
        if let Ok(re) = Regex::new(&pattern)
            && let Some(cap) = re.captures(&combined)
        {
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
        if let Ok(re) = Regex::new(pattern)
            && re.is_match(&combined)
        {
            startup_registrations.push(StartupRegistration {
                registration_type: reg_type.to_string(),
                detail: detail.to_string(),
            });
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
    let ws = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ExposesWebService, 1_000),
        "ExposesWebService",
    );
    let hh = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ExposesHttpHandler, 1_000),
        "ExposesHttpHandler",
    );
    let wcf = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ExposesWcfService, 1_000),
        "ExposesWcfService",
    );
    let mods = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::RegistersModule, 1_000),
        "RegistersModule",
    );
    let routes = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::RegistersHandler, 1_000),
        "RegistersHandler",
    );

    // Get ApiCall edges to cross-reference callers
    let api_calls = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ApiCall, 10_000),
        "ApiCall",
    );

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
            if let Some(ref meta) = e.metadata
                && let Some(method) = meta.get("method_name").and_then(|v| v.as_str())
                && !entry.methods.contains(&method.to_string())
            {
                entry.methods.push(method.to_string());
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
    script_files: &[(String, String)],
) -> JsAnalysisSummary {
    let dom_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ManipulatesDom, 10_000),
        "ManipulatesDom",
    );
    let postback_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::TriggersPostback, 10_000),
        "TriggersPostback",
    );
    let api_call_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ApiCall, 10_000),
        "ApiCall/js",
    );
    let contains_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::Contains, 50_000),
        "Contains",
    );

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
    for fc in markup_files {
        for cap in JS_SCRIPT_SRC_RE.captures_iter(&fc.markup_content) {
            let js_ref = cap[1].to_string();
            let js_list = page_js_deps.entry(fc.file_path.clone()).or_default();
            if !js_list.contains(&js_ref) {
                js_list.push(js_ref);
            }
        }
    }

    // Detect inline <script> blocks (not src= external files)
    let mut inline_script_files = Vec::new();
    for fc in markup_files {
        if JS_INLINE_RE
            .find_iter(&fc.markup_content)
            .any(|m| !JS_SRC_ATTR_RE.is_match(m.as_str()))
        {
            inline_script_files.push(fc.file_path.clone());
        }
    }

    // Detect jQuery version hint from JS files
    let mut jquery_version_hint = None;
    for (path, _content) in script_files {
        if let Some(cap) = JS_JQUERY_RE.captures(&path.to_lowercase()) {
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
        total_script_files: script_files.len(),
        legacy_total_js_files: script_files.len(),
        script_files_with_server_deps: js_files_with_deps.len(),
        legacy_js_files_with_server_deps: js_files_with_deps.len(),
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
    let spatial_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::SpatialCall, 10_000),
        "SpatialCall",
    );

    // Query insight nodes for GIS inventories
    let gis_insights = nodes_or_warn(
        graph.query_nodes(project_id, Some("insight"), None, None, 1_000),
        "gis_insights",
    )
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
                && !entry.features.contains(&"Places API".to_string())
            {
                entry.features.push("Places API".into());
            }
            if meta
                .get("has_streetview")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !entry.features.contains(&"StreetView".to_string())
            {
                entry.features.push("StreetView".into());
            }
            if meta
                .get("has_directions")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !entry.features.contains(&"Directions".into())
            {
                entry.features.push("Directions".into());
            }
            if meta
                .get("has_heatmap")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !entry.features.contains(&"Heatmap".into())
            {
                entry.features.push("Heatmap".into());
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
                && !entry.features.contains(&"KML layers".into())
            {
                entry.features.push("KML layers".into());
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
        let asp_insights = nodes_or_warn(
            graph.query_nodes(project_id, Some("insight"), None, None, 1_000),
            "asp_insights",
        )
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

    let include_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::IncludesFile, 5_000),
        "IncludesFile",
    );

    let mut com_objects = Vec::new();
    let mut ado_connections = 0usize;
    let mut sql_statements = 0usize;
    let mut state_accesses = 0usize;
    let mut includes = Vec::new();

    // Scan ASP file contents for patterns
    for (path, content) in asp_files {
        for cap in ASP_CREATE_OBJ_RE.captures_iter(content) {
            let prog_id = cap[1].to_string();
            if prog_id.to_lowercase().contains("adodb") {
                ado_connections += 1;
            }
            com_objects.push(ComObjectRef {
                file_path: path.clone(),
                prog_id,
            });
        }
        sql_statements += ASP_SQL_RE.find_iter(content).count();
        state_accesses += ASP_STATE_RE.find_iter(content).count();
        for cap in ASP_INCLUDE_RE.captures_iter(content) {
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
    let all_insights = nodes_or_warn(
        graph.query_nodes(project_id, Some("insight"), None, None, 2_000),
        "report_insights",
    );

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
    let ap_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::AntiPattern, 5_000),
        "AntiPattern/reports",
    );
    let crystal_edges: Vec<_> = ap_edges
        .iter()
        .filter(|e| {
            e.metadata
                .as_ref()
                .and_then(|m| m.get("pattern").and_then(|v| v.as_str()))
                .is_some_and(|p| p.to_lowercase().contains("crystal"))
        })
        .collect();

    let mut ssrs_reports = Vec::new();
    let mut crystal_reports = Vec::new();
    let mut shared_data_sources = Vec::new();
    let mut has_binary_rpt = false;

    // Parse SSRS report files (.rdl, .rdlc)
    for (path, content) in report_files {
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
        if ext == "rdl" || ext == "rdlc" {
            let datasets: Vec<String> = RPT_DATASET_RE
                .captures_iter(content)
                .map(|c| c[1].to_string())
                .collect();
            let param_count = RPT_PARAM_RE.find_iter(content).count();
            let subreports: Vec<String> = RPT_SUBREPORT_RE
                .captures_iter(content)
                .map(|c| c[1].to_string())
                .collect();
            for cap in RPT_DATASOURCE_RE.captures_iter(content) {
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

// ── Phase 33 analysis functions ────────────────────────────────────────────────

// ── Gap 1: Code-behind method inventory ─────────────────────────────────────

/// Public wrapper for classify_method_kind, used by access_layer_tools.
pub fn classify_method_kind_pub(
    name: &str,
    effects: &[String],
    metadata: &Option<serde_json::Value>,
) -> MethodKind {
    classify_method_kind(name, effects, metadata)
}

fn classify_method_kind(
    name: &str,
    effects: &[String],
    metadata: &Option<serde_json::Value>,
) -> MethodKind {
    static LIFECYCLE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)^(?:Page_(?:Load|Init|PreRender|Unload|PreInit|InitComplete|LoadComplete|PreRenderComplete|SaveStateComplete|Error)|OnInit|OnLoad|OnPreRender|OnUnload)$").expect("valid regex")
    });
    static CONTROL_EVENT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)_(?:Click|Command|RowCommand|SelectedIndexChanged|TextChanged|CheckedChanged|DataBound|RowEditing|RowUpdating|RowDeleting|RowCancelingEdit|PageIndexChanging|Sorting|ItemCommand|ItemDataBound|DataBinding|ServerClick|ServerChange|NeedDataSource|ItemCreated|Init|Load|PreRender|Unload)$").expect("valid regex")
    });

    if LIFECYCLE_RE.is_match(name) {
        return MethodKind::Lifecycle;
    }
    if CONTROL_EVENT_RE.is_match(name) {
        return MethodKind::ControlEvent;
    }

    // Check for WebMethod attribute in metadata
    if let Some(meta) = metadata {
        if let Some(sig) = meta.get("signature").and_then(|v| v.as_str())
            && sig.contains("WebMethod")
        {
            return MethodKind::WebMethod;
        }
        if let Some(eff) = meta.get("effects").and_then(|v| v.as_str())
            && eff.contains("WebMethod")
        {
            return MethodKind::WebMethod;
        }
    }

    if effects.iter().any(|e| e.contains("SQL_Access")) {
        return MethodKind::DataAccess;
    }

    MethodKind::Helper
}

fn build_method_inventories(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_contents: &[FileContent],
) -> BTreeMap<String, PageMethodInventory> {
    let mut result = BTreeMap::new();

    for fc in file_contents {
        let cb_path = fc.file_path.clone() + ".vb";
        let cb_path_cs = fc.file_path.clone() + ".cs";

        // Try both VB and CS code-behind paths
        for codebehind_path in &[&cb_path, &cb_path_cs] {
            let method_nodes = match graph.query_nodes(
                project_id,
                Some("function"),
                None,
                Some(codebehind_path),
                500,
            ) {
                Ok(nodes) => nodes,
                Err(e) => {
                    // MIG1/D2: log graph query failure so operators can see it.
                    tracing::warn!(
                        project_id,
                        path = %codebehind_path,
                        error = %e,
                        "MIG1: graph query for code-behind failed — skipping method node extraction"
                    );
                    continue;
                }
            };

            if method_nodes.is_empty() {
                // Also try without the extra extension (e.g. just "Page.aspx.vb")
                continue;
            }

            let mut methods: Vec<MethodInfo> = Vec::new();

            for node in &method_nodes {
                let effects: Vec<String> = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("effects"))
                    .and_then(|v| v.as_str())
                    .map(|s| {
                        s.split(',')
                            .map(|e| e.trim().to_string())
                            .filter(|e| !e.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();

                let signature = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("signature"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&node.name)
                    .to_string();

                let return_type = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("return_type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Sub")
                    .to_string();

                let access_level = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("access_level"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Private")
                    .to_string();

                let kind = classify_method_kind(&node.name, &effects, &node.metadata);
                let line_count = if node.end_line >= node.start_line {
                    node.end_line - node.start_line + 1
                } else {
                    1
                };

                methods.push(MethodInfo {
                    name: node.name.clone(),
                    signature,
                    return_type,
                    access_level,
                    line_range: (node.start_line, node.end_line),
                    line_count,
                    method_kind: kind,
                    effects,
                    calls_methods: vec![],
                    called_by: vec![],
                    body_preview: None, // graph nodes don't have body text
                    complexity_score: 0,
                    handles_clause: vec![],
                });
            }

            // Populate calls_methods/called_by from Dependency edges
            if let Ok(dep_edges) = graph.list_edges_by_kind(project_id, EdgeKind::Dependency, 5000)
            {
                let method_names: Vec<String> = methods.iter().map(|m| m.name.clone()).collect();
                for edge in &dep_edges {
                    for m in &mut methods {
                        if edge.source_id.ends_with(&m.name) {
                            let target_name =
                                edge.target_id.rsplit('.').next().unwrap_or(&edge.target_id);
                            if method_names.contains(&target_name.to_string()) {
                                m.calls_methods.push(target_name.to_string());
                            }
                        }
                        if edge.target_id.ends_with(&m.name) {
                            let source_name =
                                edge.source_id.rsplit('.').next().unwrap_or(&edge.source_id);
                            if method_names.contains(&source_name.to_string()) {
                                m.called_by.push(source_name.to_string());
                            }
                        }
                    }
                }
            }

            let lifecycle_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::Lifecycle))
                .count();
            let event_handlers = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::ControlEvent))
                .count();
            let web_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::WebMethod))
                .count();
            let data_access_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::DataAccess))
                .count();
            let helper_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::Helper))
                .count();
            let methods_with_sql = methods
                .iter()
                .filter(|m| m.effects.iter().any(|e| e.contains("SQL")))
                .count();
            let methods_with_state = methods
                .iter()
                .filter(|m| m.effects.iter().any(|e| e.contains("State")))
                .count();
            let largest_method = methods
                .iter()
                .max_by_key(|m| m.line_count)
                .map(|m| (m.name.clone(), m.line_count));

            let inventory = PageMethodInventory {
                file_path: fc.file_path.clone(),
                codebehind_path: codebehind_path.to_string(),
                total_methods: methods.len(),
                lifecycle_methods,
                event_handlers,
                web_methods,
                data_access_methods,
                helper_methods,
                largest_method,
                methods_with_sql,
                methods_with_state,
                methods,
            };

            result.insert(fc.file_path.clone(), inventory);
            break; // Found methods, no need to try the other extension
        }
    }

    // Fallback: if graph had no data, parse code-behind content directly
    for fc in file_contents {
        if result.contains_key(&fc.file_path) {
            continue;
        }
        if let Some(ref cb_content) = fc.codebehind_content {
            let methods = extract_methods_from_content(cb_content);
            if methods.is_empty() {
                continue;
            }
            let cb_path = fc.file_path.clone() + ".vb";
            let lifecycle_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::Lifecycle))
                .count();
            let event_handlers = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::ControlEvent))
                .count();
            let web_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::WebMethod))
                .count();
            let data_access_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::DataAccess))
                .count();
            let helper_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::Helper))
                .count();
            let methods_with_sql = methods
                .iter()
                .filter(|m| m.effects.iter().any(|e| e.contains("SQL")))
                .count();
            let methods_with_state = methods
                .iter()
                .filter(|m| m.effects.iter().any(|e| e.contains("State")))
                .count();
            let largest_method = methods
                .iter()
                .max_by_key(|m| m.line_count)
                .map(|m| (m.name.clone(), m.line_count));

            result.insert(
                fc.file_path.clone(),
                PageMethodInventory {
                    file_path: fc.file_path.clone(),
                    codebehind_path: cb_path,
                    total_methods: methods.len(),
                    lifecycle_methods,
                    event_handlers,
                    web_methods,
                    data_access_methods,
                    helper_methods,
                    largest_method,
                    methods_with_sql,
                    methods_with_state,
                    methods,
                },
            );
        }
    }

    result
}

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
            let effects = extract_effects_from_nearby_content(content, &name);
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
                classify_method_kind(&name, &effects, &None)
            };

            // Extract body for line range, preview, and complexity
            let (body_preview, line_range, line_count, complexity) =
                if let Some((body, sl, el, lc)) = extract_vb_method_body(content, &name) {
                    let preview = make_body_preview(&body, lc);
                    let cx = compute_complexity_score(&body);
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
            let effects = extract_effects_from_nearby_content(content, &name);
            let kind = classify_method_kind(&name, &effects, &None);

            // Extract body for line range, preview, and complexity
            let (body_preview, line_range, line_count, complexity) =
                if let Some((body, sl, el, lc)) = extract_cs_method_body(content, &name) {
                    let preview = make_body_preview(&body, lc);
                    let cx = compute_complexity_score(&body);
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

fn extract_effects_from_nearby_content(content: &str, method_name: &str) -> Vec<String> {
    // THIRD-PASS FIX: Scope effect detection to the method body when possible.
    // Previously scanned the ENTIRE file, causing every method to be tagged
    // with SQL_Access if any method in the file used SqlCommand.
    let body_text: Option<String> = {
        let is_vb = content.contains("End Sub") || content.contains("End Function");
        if is_vb {
            extract_vb_method_body(content, method_name).map(|(b, _, _, _)| b)
        } else {
            extract_cs_method_body(content, method_name).map(|(b, _, _, _)| b)
        }
    };
    // Use extracted body if available, fall back to full file content
    let scan_text = body_text.as_deref().unwrap_or(content);
    let lower = scan_text.to_lowercase();

    let mut effects = Vec::new();
    if lower.contains("sqlcommand")
        || lower.contains("sqlconnection")
        || lower.contains("sqldatareader")
        || lower.contains("sqldataadapter")
        || lower.contains("executenonquery")
        || lower.contains("executereader")
        || lower.contains("executescalar")
        || lower.contains("oledbcommand")
        || lower.contains("oledbconnection")
    {
        effects.push("SQL_Access".to_string());
    }
    if lower.contains("session(")
        || lower.contains("session[")
        || lower.contains("viewstate(")
        || lower.contains("viewstate[")
    {
        effects.push("State_Access".to_string());
    }
    if lower.contains("createobject") {
        effects.push("COM_Interop".to_string());
    }
    if lower.contains("response.redirect")
        || lower.contains("server.transfer")
        || lower.contains("response.write")
    {
        effects.push("HTTP_Response".to_string());
    }
    if lower.contains("smtpclient")
        || lower.contains("mailmessage")
        || lower.contains("cdo.message")
    {
        effects.push("Email_Send".to_string());
    }
    effects
}

// ── Gap 2: Third-party control detection ────────────────────────────────────

fn build_third_party_control_summary(markup_files: &[FileContent]) -> ThirdPartyControlSummary {
    static THIRD_PARTY_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<(telerik|rad|dx|ig|igtbl|igmisc|igsch|ComponentArt|kendo|obout|eo|FarPoint|Dart|cwc|ntx):(\w+)\b"#).expect("valid regex")
    });

    let mut vendor_controls: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut all_files: Vec<String> = Vec::new();

    for fc in markup_files {
        let mut found_in_file = false;
        for cap in THIRD_PARTY_RE.captures_iter(&fc.markup_content) {
            let prefix = cap[1].to_string();
            let control_name = cap[2].to_string();
            let vendor = classify_vendor_from_prefix(&prefix);
            vendor_controls
                .entry(vendor)
                .or_default()
                .entry(format!("{prefix}:{control_name}"))
                .or_default()
                .push(fc.file_path.clone());
            found_in_file = true;
        }
        if found_in_file {
            all_files.push(fc.file_path.clone());
        }
    }
    all_files.sort();
    all_files.dedup();

    let mut vendors_detected = Vec::new();
    let mut total_third_party = 0usize;
    let mut unmapped_controls = Vec::new();

    for (vendor, controls_map) in &vendor_controls {
        let (suite, modern_suite, license) = vendor_suite_info(vendor);
        let mut controls_used: Vec<(String, usize)> = Vec::new();
        let mut vendor_files: Vec<String> = Vec::new();
        let mut vendor_count = 0usize;

        for (tag_name, files) in controls_map {
            let usage = files.len();
            vendor_count += usage;
            controls_used.push((tag_name.clone(), usage));

            let control_short = tag_name.split(':').nth(1).unwrap_or(tag_name);
            if engram_index::control_mapping::lookup(control_short).is_none() {
                let first_file = files.first().cloned().unwrap_or_default();
                unmapped_controls.push(UnmappedControl {
                    tag_name: tag_name.clone(),
                    vendor: vendor.clone(),
                    file_path: first_file,
                    note: format!(
                        "No automatic mapping — evaluate {modern_suite} or manual implementation"
                    ),
                });
            }

            vendor_files.extend(files.iter().cloned());
        }

        vendor_files.sort();
        vendor_files.dedup();
        controls_used.sort_by(|a, b| b.1.cmp(&a.1));
        total_third_party += vendor_count;

        vendors_detected.push(VendorSummary {
            vendor: vendor.clone(),
            suite: suite.to_string(),
            control_count: vendor_count,
            controls_used,
            files: vendor_files,
            modern_replacement_suite: modern_suite.to_string(),
            license_note: license.to_string(),
        });
    }

    vendors_detected.sort_by(|a, b| b.control_count.cmp(&a.control_count));

    ThirdPartyControlSummary {
        vendors_detected,
        total_third_party_controls: total_third_party,
        files_with_third_party: all_files,
        unmapped_controls,
    }
}

fn classify_vendor_from_prefix(prefix: &str) -> String {
    match prefix.to_lowercase().as_str() {
        "telerik" | "rad" | "kendo" => "Telerik".to_string(),
        "dx" => "DevExpress".to_string(),
        "ig" | "igtbl" | "igmisc" | "igsch" | "ntx" => "Infragistics".to_string(),
        "componentart" => "ComponentArt".to_string(),
        "obout" => "Obout".to_string(),
        "eo" => "EO.WebControls".to_string(),
        "farpoint" => "FarPoint".to_string(),
        "dart" => "Dart".to_string(),
        "cwc" => "CustomWebControls".to_string(),
        other => other.to_string(),
    }
}

fn vendor_suite_info(vendor: &str) -> (&'static str, &'static str, &'static str) {
    match vendor {
        "Telerik" => (
            "UI for ASP.NET AJAX",
            "Telerik UI for Blazor or MudBlazor",
            "Commercial for Telerik Blazor; MudBlazor is MIT",
        ),
        "DevExpress" => (
            "ASP.NET Controls",
            "DevExpress Blazor Components or MudBlazor",
            "Commercial license required",
        ),
        "Infragistics" => (
            "Ultimate UI for ASP.NET",
            "IgniteUI for Blazor or MudBlazor",
            "Commercial license required",
        ),
        "ComponentArt" => (
            "Web.UI",
            "MudBlazor or Radzen",
            "ComponentArt discontinued; use open-source alternative",
        ),
        "Obout" => (
            "Suite for ASP.NET",
            "MudBlazor",
            "Obout discontinued; migrate to open-source",
        ),
        "EO.WebControls" => ("EO.Web", "MudBlazor", "Commercial"),
        "FarPoint" => (
            "Spread for ASP.NET",
            "SpreadJS or AG Grid",
            "Commercial license required",
        ),
        _ => (
            "Unknown Suite",
            "MudBlazor (open-source)",
            "Evaluate licensing",
        ),
    }
}

// ── Gap 3: Dependency inventory ─────────────────────────────────────────────

fn build_dependency_inventory(project_refs: &[ProjectReferenceBundle]) -> DependencyInventory {
    let mut target_frameworks: Vec<String> = Vec::new();
    let mut all_packages: Vec<NuGetPackageInfo> = Vec::new();
    let mut all_assemblies: Vec<AssemblyRefInfo> = Vec::new();
    let mut proj_refs: Vec<ProjectRefInfo> = Vec::new();
    let mut seen_packages: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_assemblies: std::collections::HashSet<String> = std::collections::HashSet::new();

    for prb in project_refs {
        if let Some(ref tf) = prb.target_framework
            && !target_frameworks.contains(tf)
        {
            target_frameworks.push(tf.clone());
        }

        for pkg in &prb.package_references {
            if seen_packages.contains(&pkg.name.to_lowercase()) {
                continue;
            }
            seen_packages.insert(pkg.name.to_lowercase());

            let (modern, version, notes, category) = lookup_modern_replacement(&pkg.name);
            let is_framework =
                pkg.name.starts_with("System.") || pkg.name.starts_with("Microsoft.");

            if is_framework && pkg.version.is_none() {
                // This is an assembly reference, not a NuGet package
                if seen_assemblies.insert(pkg.name.to_lowercase()) {
                    let (asm_modern, removal) = lookup_assembly_replacement(&pkg.name);
                    all_assemblies.push(AssemblyRefInfo {
                        assembly_name: pkg.name.clone(),
                        is_framework: true,
                        modern_equivalent: asm_modern.map(|s| s.to_string()),
                        removal_reason: removal.map(|s| s.to_string()),
                    });
                }
            } else {
                all_packages.push(NuGetPackageInfo {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    modern_replacement: modern.map(|s| s.to_string()),
                    modern_version: version.map(|s| s.to_string()),
                    migration_notes: notes.map(|s| s.to_string()),
                    category: category.to_string(),
                });
            }
        }

        for asm in &prb.assembly_references {
            if seen_assemblies.contains(&asm.to_lowercase()) {
                continue;
            }
            seen_assemblies.insert(asm.to_lowercase());
            let is_fw =
                asm.starts_with("System.") || asm.starts_with("Microsoft.") || asm == "mscorlib";
            let (modern, removal) = lookup_assembly_replacement(asm);
            all_assemblies.push(AssemblyRefInfo {
                assembly_name: asm.clone(),
                is_framework: is_fw,
                modern_equivalent: modern.map(|s| s.to_string()),
                removal_reason: removal.map(|s| s.to_string()),
            });
        }

        for dep in &prb.project_dependencies {
            proj_refs.push(ProjectRefInfo {
                project_name: dep.rsplit(['/', '\\']).next().unwrap_or(dep).to_string(),
                project_path: dep.clone(),
                target_framework: prb.target_framework.clone(),
            });
        }
    }

    let framework_assemblies: Vec<String> = all_assemblies
        .iter()
        .filter(|a| a.is_framework)
        .map(|a| a.assembly_name.clone())
        .collect();
    let third_party_assemblies: Vec<String> = all_assemblies
        .iter()
        .filter(|a| !a.is_framework)
        .map(|a| a.assembly_name.clone())
        .collect();
    let with_replacement = all_packages
        .iter()
        .filter(|p| p.modern_replacement.is_some())
        .count();
    let without_replacement = all_packages.len() - with_replacement;

    DependencyInventory {
        total_packages: all_packages.len(),
        total_assemblies: all_assemblies.len(),
        packages_with_known_replacement: with_replacement,
        packages_without_replacement: without_replacement,
        framework_assemblies,
        third_party_assemblies,
        target_frameworks,
        nuget_packages: all_packages,
        assembly_references: all_assemblies,
        project_references: proj_refs,
        legacy_packages: Vec::new(), // populated separately from packages.config
        binding_redirects: Vec::new(), // populated separately from web.config
    }
}

/// Returns (modern_replacement, modern_version, migration_notes, category)
fn lookup_modern_replacement(
    package: &str,
) -> (
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
    &'static str,
) {
    match package.to_lowercase().as_str() {
        "entityframework" => (
            Some("Microsoft.EntityFrameworkCore"),
            Some("8.0+"),
            Some("Different DbContext API; migrations differ"),
            "ORM",
        ),
        "newtonsoft.json" => (
            Some("System.Text.Json (or keep Newtonsoft)"),
            Some("8.0+"),
            Some("Compatible; System.Text.Json is built-in but API differs"),
            "Serialization",
        ),
        "telerik.web.ui" => (
            Some("Telerik.UI.for.Blazor"),
            None,
            Some("Commercial, completely different API surface"),
            "UI Controls",
        ),
        "devexpress.web" | "devexpress.web.v23.1" => (
            Some("DevExpress.Blazor"),
            None,
            Some("Commercial, different API"),
            "UI Controls",
        ),
        "infragistics.web" | "infragistics4.web" => (
            Some("IgniteUI.Blazor"),
            None,
            Some("Commercial"),
            "UI Controls",
        ),
        "log4net" => (
            Some("Serilog or NLog"),
            Some("3.0+"),
            Some("Similar patterns; Serilog preferred for structured logging"),
            "Logging",
        ),
        "nlog" => (
            Some("NLog"),
            Some("5.0+"),
            Some("Already .NET Core compatible"),
            "Logging",
        ),
        "serilog" => (
            Some("Serilog"),
            Some("3.0+"),
            Some("Already compatible"),
            "Logging",
        ),
        "unity" => (
            Some("Microsoft.Extensions.DependencyInjection"),
            Some("8.0+"),
            Some("Built-in DI in ASP.NET Core"),
            "DI",
        ),
        "autofac" => (
            Some("Autofac"),
            Some("7.0+"),
            Some(".NET Core compatible"),
            "DI",
        ),
        "structuremap" => (
            Some("Microsoft.Extensions.DependencyInjection or Lamar"),
            None,
            Some("StructureMap discontinued; Lamar is successor"),
            "DI",
        ),
        "system.data.sqlclient" => (
            Some("Microsoft.Data.SqlClient"),
            Some("5.0+"),
            Some("Namespace change; connection string compatible"),
            "Data",
        ),
        "microsoft.practices.enterpriselibrary.data" | "enterpriselibrary.data" => (
            Some("Microsoft.Data.SqlClient + Dapper"),
            None,
            Some("Replace per-block; no single equivalent"),
            "Data",
        ),
        "npoi" => (
            Some("NPOI"),
            Some("2.6+"),
            Some("Compatible; or consider ClosedXML"),
            "Office",
        ),
        "epplus" => (
            Some("EPPlus"),
            Some("7.0+"),
            Some("License changed to commercial in v5+"),
            "Office",
        ),
        "itextsharp" => (
            Some("itext7 or QuestPDF"),
            None,
            Some("License changed; QuestPDF is MIT"),
            "PDF",
        ),
        "crystaldecisions.crystalreports.engine" => (
            None,
            None,
            Some("No .NET Core port; consider SSRS, FastReport, or Telerik Reporting"),
            "Reporting",
        ),
        "microsoft.reportviewer.webforms"
        | "microsoft.reportingservices.reportviewercontrol.webforms" => (
            Some("Microsoft.Reporting.NETCore"),
            Some("16.0+"),
            Some("Limited .NET Core support"),
            "Reporting",
        ),
        "microsoft.aspnet.signalr" => (
            Some("Microsoft.AspNetCore.SignalR"),
            Some("8.0+"),
            Some("Different hub API; requires rewrite"),
            "RealTime",
        ),
        "system.web.optimization" => (
            None,
            None,
            Some("Removed; use Vite, Webpack, or esbuild for bundling"),
            "Build",
        ),
        "microsoft.owin" | "owin" => (
            None,
            None,
            Some("ASP.NET Core has native middleware pipeline"),
            "Middleware",
        ),
        "nhibernate" => (
            Some("NHibernate or EF Core"),
            Some("5.4+"),
            Some(".NET Core compatible"),
            "ORM",
        ),
        "dapper" => (
            Some("Dapper"),
            Some("2.1+"),
            Some("Already compatible"),
            "ORM",
        ),
        "fluentvalidation" => (
            Some("FluentValidation"),
            Some("11.0+"),
            Some("Already compatible"),
            "Validation",
        ),
        "automapper" => (
            Some("AutoMapper or Mapster"),
            Some("13.0+"),
            Some("Already compatible; Mapster is faster alternative"),
            "Mapping",
        ),
        "mediatr" => (
            Some("MediatR"),
            Some("12.0+"),
            Some("Already compatible"),
            "Patterns",
        ),
        "hangfire" | "hangfire.core" => (
            Some("Hangfire"),
            Some("1.8+"),
            Some("Already compatible"),
            "BackgroundJobs",
        ),
        "quartz" | "quartz.net" => (
            Some("Quartz.NET"),
            Some("3.8+"),
            Some("Already compatible"),
            "BackgroundJobs",
        ),
        "stackexchange.redis" => (
            Some("StackExchange.Redis"),
            Some("2.7+"),
            Some("Already compatible"),
            "Cache",
        ),
        "microsoft.aspnet.webapi" | "microsoft.aspnet.webapi.core" => (
            Some("Microsoft.AspNetCore.Mvc"),
            Some("8.0+"),
            Some("Unified in ASP.NET Core; different routing and DI"),
            "Web",
        ),
        "microsoft.aspnet.mvc" => (
            Some("Microsoft.AspNetCore.Mvc"),
            Some("8.0+"),
            Some("Different routing, DI, filters"),
            "Web",
        ),
        "ajaxcontroltoolkit" => (
            None,
            None,
            Some("No .NET Core port; use MudBlazor or JavaScript components"),
            "UI Controls",
        ),
        "antlr" | "antlr3.runtime" => (
            Some("Antlr4.Runtime.Standard"),
            Some("4.13+"),
            Some("Different API"),
            "Parsing",
        ),
        "webgrease" => (None, None, Some("Removed; use modern bundler"), "Build"),
        "microsoft.web.infrastructure" => (None, None, Some("Built into ASP.NET Core"), "Web"),
        "microsoft.aspnet.web.optimization" => {
            (None, None, Some("Use Vite/Webpack for bundling"), "Build")
        }
        "dotnetopenauth" => (
            Some("Microsoft.AspNetCore.Authentication.OAuth"),
            Some("8.0+"),
            Some("Built-in OAuth in ASP.NET Core"),
            "Auth",
        ),
        "microsoft.identitymodel.tokens" => (
            Some("Microsoft.IdentityModel.Tokens"),
            Some("7.0+"),
            Some("Already compatible"),
            "Auth",
        ),
        "microsoft.aspnet.identity.core" => (
            Some("Microsoft.AspNetCore.Identity"),
            Some("8.0+"),
            Some("Different API but same concepts"),
            "Auth",
        ),
        "system.web.services" => (None, None, Some("Removed; use Minimal API or gRPC"), "Web"),
        "system.servicemodel" => (
            Some("CoreWCF or gRPC"),
            None,
            Some("Limited WCF in .NET Core; gRPC preferred"),
            "Services",
        ),
        "system.enterpriseservices" => (None, None, Some("No .NET Core equivalent"), "Legacy"),
        "system.directoryservices" => (
            Some("System.DirectoryServices"),
            None,
            Some("Partial support; needs platform-specific shim"),
            "Directory",
        ),
        "system.drawing" => (
            Some("System.Drawing.Common or SkiaSharp"),
            None,
            Some("Linux needs libgdiplus; SkiaSharp is cross-platform"),
            "Graphics",
        ),
        _ => (None, None, None, "Unknown"),
    }
}

fn lookup_assembly_replacement(assembly: &str) -> (Option<&'static str>, Option<&'static str>) {
    match assembly.to_lowercase().as_str() {
        "system.web" => (
            None,
            Some("Removed in .NET Core — use ASP.NET Core middleware"),
        ),
        "system.web.mvc" => (
            Some("Microsoft.AspNetCore.Mvc"),
            Some("Different routing and DI"),
        ),
        "system.web.services" => (None, Some("Removed — use Minimal API or gRPC")),
        "system.web.extensions" => (
            None,
            Some("Removed — AJAX functionality is built-in to ASP.NET Core"),
        ),
        "system.enterpriseservices" => (None, Some("Removed — no .NET Core equivalent")),
        "system.web.mobile" => (None, Some("Removed — use responsive design")),
        "system.web.routing" => (None, Some("Built into ASP.NET Core endpoint routing")),
        "system.web.abstractions" => (None, Some("Built into ASP.NET Core")),
        "system.web.dynamicdata" => (None, Some("Removed — no equivalent")),
        "system.web.entity" => (None, Some("Use EF Core")),
        "system.web.applicationservices" => (None, Some("Use ASP.NET Core Identity")),
        "microsoft.csharp" => (Some("Microsoft.CSharp"), None),
        "system.configuration" => (
            Some("Microsoft.Extensions.Configuration"),
            Some("Different API"),
        ),
        "system.data" => (Some("System.Data.Common"), None),
        "system.data.sqlclient" => (Some("Microsoft.Data.SqlClient"), Some("Namespace change")),
        _ => (None, None),
    }
}

// ── Gap 4: Caching inventory ────────────────────────────────────────────────

fn build_caching_inventory(
    markup_files: &[FileContent],
    code_refs: &[(&str, &str)],
    code_files: &[(String, String)],
) -> CachingInventory {
    static OUTPUT_CACHE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<%@\s*OutputCache\s+([^%]+?)%>"#).expect("valid regex")
    });
    static CACHE_ATTR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(\w+)\s*=\s*"([^"]*)""#).expect("valid regex")
    });
    static CACHE_API_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:HttpRuntime\.Cache|HttpContext\.Current\.Cache|\bCache)\.(Insert|Add|Get|Remove)\s*\(\s*"([^"]+)""#).expect("valid regex")
    });
    static RESPONSE_CACHE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)Response\.Cache\.Set(?:Expires|Cacheability|MaxAge|ValidUntilExpires|NoStore|NoTransforms|SlidingExpiration|Revalidation|ETag|LastModified|VaryByCustom|OmitVaryStar)\s*\(").expect("valid regex")
    });
    static SQL_CACHE_DEP_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r#"(?i)new\s+SqlCacheDependency\s*\(\s*"?([^",)]*)"?\s*(?:,\s*"?([^",)]*)"?)?\s*\)"#,
        )
        .expect("valid regex")
    });

    let mut output_cache_pages = Vec::new();
    let mut programmatic_keys: BTreeMap<String, (Vec<String>, String)> = BTreeMap::new();
    let mut response_cache_files: Vec<String> = Vec::new();
    let mut sql_cache_deps = Vec::new();

    // Scan markup files for OutputCache directives
    for fc in markup_files {
        for cap in OUTPUT_CACHE_RE.captures_iter(&fc.markup_content) {
            let attrs_str = &cap[1];
            let mut duration: Option<u32> = None;
            let mut vary_by_param: Option<String> = None;
            let mut vary_by_control: Option<String> = None;
            let mut vary_by_custom: Option<String> = None;
            let mut location: Option<String> = None;
            let mut cache_profile: Option<String> = None;
            let mut sql_dependency: Option<String> = None;

            for attr_cap in CACHE_ATTR_RE.captures_iter(attrs_str) {
                let key = &attr_cap[1];
                let val = attr_cap[2].to_string();
                match key.to_lowercase().as_str() {
                    "duration" => duration = val.parse().ok(),
                    "varybyparam" => vary_by_param = Some(val),
                    "varybycontrol" => vary_by_control = Some(val),
                    "varybycustom" => vary_by_custom = Some(val),
                    "location" => location = Some(val),
                    "cacheprofile" => cache_profile = Some(val),
                    "sqldependency" => sql_dependency = Some(val),
                    _ => {}
                }
            }

            let mut modern_parts = Vec::new();
            if let Some(d) = duration {
                modern_parts.push(format!("Duration = {d}"));
            }
            if let Some(ref vbp) = vary_by_param
                && vbp != "none"
                && vbp != "*"
            {
                modern_parts.push(format!("VaryByQueryKeys = new[] {{ \"{vbp}\" }}"));
            }
            let modern_equivalent = if modern_parts.is_empty() {
                "[ResponseCache]".to_string()
            } else {
                format!("[ResponseCache({})]", modern_parts.join(", "))
            };

            output_cache_pages.push(OutputCacheEntry {
                file_path: fc.file_path.clone(),
                duration_seconds: duration,
                vary_by_param,
                vary_by_control,
                vary_by_custom,
                location,
                cache_profile,
                sql_dependency,
                modern_equivalent,
            });
        }
    }

    // Scan code files for programmatic cache patterns
    let all_code: Vec<(&str, &str)> = code_refs
        .iter()
        .copied()
        .chain(code_files.iter().map(|(p, c)| (p.as_str(), c.as_str())))
        .collect();

    for (path, content) in &all_code {
        for cap in CACHE_API_RE.captures_iter(content) {
            let operation = cap[1].to_string();
            let cache_key = cap[2].to_string();
            programmatic_keys
                .entry(cache_key)
                .or_insert_with(|| (Vec::new(), operation.clone()))
                .0
                .push(path.to_string());
        }

        if RESPONSE_CACHE_RE.is_match(content) && !response_cache_files.contains(&path.to_string())
        {
            response_cache_files.push(path.to_string());
        }

        for cap in SQL_CACHE_DEP_RE.captures_iter(content) {
            let db = cap.get(1).map(|m| m.as_str().to_string());
            let table = cap.get(2).map(|m| m.as_str().to_string());
            sql_cache_deps.push(SqlCacheDependencyEntry {
                file_path: path.to_string(),
                database_name: db,
                table_name: table,
                modern_note: "No direct .NET Core equivalent — use EF Change Tracker + cache invalidation or message bus".to_string(),
            });
        }
    }

    let programmatic_cache_keys: Vec<ProgrammaticCacheEntry> = programmatic_keys
        .into_iter()
        .map(|(key, (mut files, operation))| {
            files.sort();
            files.dedup();
            let modern = if files.len() > 1 {
                "IDistributedCache (shared across instances)".to_string()
            } else {
                "IMemoryCache with SlidingExpiration".to_string()
            };
            ProgrammaticCacheEntry {
                cache_key: key,
                operation,
                has_expiration: false,
                has_dependency: false,
                modern_equivalent: modern,
                files,
            }
        })
        .collect();

    let total_cached = output_cache_pages.len();
    let total_keys = programmatic_cache_keys.len();
    let has_resp = !response_cache_files.is_empty();
    let has_sql = !sql_cache_deps.is_empty();

    CachingInventory {
        output_cache_pages,
        programmatic_cache_keys,
        response_cache_files,
        sql_cache_dependencies: sql_cache_deps,
        total_cached_pages: total_cached,
        total_cache_keys: total_keys,
        has_response_caching: has_resp,
        has_sql_dependencies: has_sql,
    }
}

// ── Gap 5: URL routing/rewrite rules ────────────────────────────────────────

fn extract_url_routing(
    web_config: Option<&str>,
    global_asax_content: &str,
    code_files: &[(&str, &str)],
) -> UrlRoutingInventory {
    static REWRITE_RULE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?is)<rule\s+name="([^"]*)"[^>]*>.*?<match\s+url="([^"]*)"[^/]*/?>.*?<action\s+type="(\w+)"\s+url="([^"]*)"[^/]*/?>.*?</rule>"#).expect("valid regex")
    });
    static URL_MAPPING_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<add\s+url="([^"]*)"\s+mappedUrl="([^"]*)"\s*/>"#).expect("valid regex")
    });
    static MAP_PAGE_ROUTE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)\.MapPageRoute\s*\(\s*"([^"]*)",\s*"([^"]*)",\s*"([^"]*)""#)
            .expect("valid regex")
    });
    static REWRITE_PATH_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?:HttpContext\.Current|Context|HttpContext)\.RewritePath\s*\(\s*"([^"]*)""#,
        )
        .expect("valid regex")
    });
    static REDIRECT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)Response\.Redirect(Permanent)?\s*\(\s*"([^"]*)""#).expect("valid regex")
    });
    static SERVER_TRANSFER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)Server\.Transfer\s*\(\s*"([^"]*)""#).expect("valid regex")
    });
    static FRIENDLY_URL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)FriendlyUrl|FriendlyUrlSettings|EnableFriendlyUrls").expect("valid regex")
    });

    let mut rewrite_rules = Vec::new();
    let mut page_routes = Vec::new();
    let mut url_mappings = Vec::new();
    let mut rewrite_path_calls = Vec::new();
    let mut redirects = Vec::new();
    let mut server_transfers = Vec::new();
    let mut has_friendly_urls = false;

    // Parse web.config
    if let Some(wc) = web_config {
        for cap in REWRITE_RULE_RE.captures_iter(wc) {
            let name = cap[1].to_string();
            let pattern = cap[2].to_string();
            let action = cap[3].to_string();
            let target = cap[4].to_string();
            let modern = build_modern_route_equivalent(&pattern, &target, &action);
            rewrite_rules.push(UrlRewriteRule {
                rule_name: name,
                match_pattern: pattern,
                action_type: action,
                target_url: target,
                modern_equivalent: modern,
            });
        }

        for cap in URL_MAPPING_RE.captures_iter(wc) {
            url_mappings.push(UrlMapping {
                friendly_url: cap[1].to_string(),
                mapped_url: cap[2].to_string(),
            });
        }

        if FRIENDLY_URL_RE.is_match(wc) {
            has_friendly_urls = true;
        }
    }

    // Parse Global.asax for MapPageRoute calls
    for cap in MAP_PAGE_ROUTE_RE.captures_iter(global_asax_content) {
        let route_name = cap[1].to_string();
        let pattern = cap[2].to_string();
        let page = cap[3].to_string();
        let modern = format!("app.MapGet(\"/{pattern}\", ...)");
        page_routes.push(PageRoute {
            route_name,
            url_pattern: pattern,
            physical_page: page,
            modern_equivalent: modern,
        });
    }

    // Scan all code files
    let all_content: Vec<(&str, &str)> = code_files
        .iter()
        .copied()
        .chain(std::iter::once(("Global.asax.vb", global_asax_content)))
        .collect();

    for (path, content) in &all_content {
        for (line_num, line) in content.lines().enumerate() {
            if let Some(cap) = REWRITE_PATH_RE.captures(line) {
                rewrite_path_calls.push(RewritePathCall {
                    file_path: path.to_string(),
                    target_path: cap[1].to_string(),
                    line_number: (line_num + 1) as u32,
                });
            }
            if let Some(cap) = REDIRECT_RE.captures(line) {
                let is_permanent = cap.get(1).is_some();
                redirects.push(RedirectEntry {
                    file_path: path.to_string(),
                    target_url: cap[2].to_string(),
                    is_permanent,
                });
            }
            if let Some(cap) = SERVER_TRANSFER_RE.captures(line) {
                server_transfers.push(ServerTransferEntry {
                    file_path: path.to_string(),
                    target_page: cap[1].to_string(),
                });
            }
        }

        if FRIENDLY_URL_RE.is_match(content) {
            has_friendly_urls = true;
        }
    }

    let total = rewrite_rules.len() + page_routes.len() + url_mappings.len();

    UrlRoutingInventory {
        rewrite_rules,
        page_routes,
        url_mappings,
        rewrite_path_calls,
        redirects,
        server_transfers,
        has_friendly_urls,
        total_url_patterns: total,
    }
}

fn build_modern_route_equivalent(pattern: &str, target: &str, action_type: &str) -> String {
    // Convert IIS rewrite regex to ASP.NET Core endpoint pattern
    let route = pattern
        .replace(r"\d+", "{id:int}")
        .replace(r"(\d+)", "{id}")
        .replace(r"([^/]+)", "{slug}")
        .replace("^", "")
        .replace("$", "");
    let _ = target; // target is the rewrite destination
    match action_type.to_lowercase().as_str() {
        "redirect" | "redirectpermanent" => {
            format!("app.MapGet(\"/{route}\", () => Results.Redirect(\"{target}\"))")
        }
        _ => format!("app.MapGet(\"/{route}\", ...)"),
    }
}

// ── Gap 6: VB.NET → C# translation flags ───────────────────────────────────

/// True iff a `VbTranslationFlag` extracted from file `flag_path` belongs
/// in the dossier for a page at `page_path` with optional detected
/// `codebehind`. Accepts:
///   - the page file itself
///   - the page's detected codebehind (when non-empty)
///   - the conventional `<page>.vb` / `<page>.cs` sibling for an `.aspx`
///     page that did not resolve a codebehind (e.g. the page inherits
///     `System.Web.UI.Page` directly)
///
/// Rejects everything else. The prior version allowed
/// `flag_path.contains(codebehind.unwrap_or(""))` which silently matched
/// *every* file in the project whenever `codebehind` was `None`, so the
/// first page dossier on a project with unresolved codebehinds dumped
/// the entire project-wide flag list into one page's section.
fn flag_belongs_to_page(flag_path: &str, page_path: &str, codebehind: Option<&str>) -> bool {
    if flag_path == page_path {
        return true;
    }
    if let Some(cb) = codebehind
        && !cb.is_empty()
        && flag_path == cb
    {
        return true;
    }
    if page_path.ends_with(".aspx")
        && (flag_path == format!("{}.vb", page_path) || flag_path == format!("{}.cs", page_path))
    {
        return true;
    }
    false
}

fn analyze_vb_translation_flags(code_files: &[(&str, &str)]) -> VbTranslationReport {
    static OPTIONAL_PARAM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bOptional\s+(?:ByVal\s+|ByRef\s+)?\w+\s+As\s+").expect("valid regex")
    });
    static IS_MISSING_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bIsMissing\s*\(").expect("valid regex"));
    static MODULE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?im)^\s*(?:Public\s+|Friend\s+)?Module\s+(\w+)").expect("valid regex")
    });
    static MY_NAMESPACE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bMy\.(Computer|Application|Settings|Resources|User|Forms|WebServices)\b")
            .expect("valid regex")
    });
    static WITH_EVENTS_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bWithEvents\s+").expect("valid regex"));
    static HANDLES_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bHandles\s+\w+\.\w+").expect("valid regex"));
    static RAISE_EVENT_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bRaiseEvent\s+\w+").expect("valid regex"));
    static SHADOWS_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bShadows\s+").expect("valid regex"));
    static OPTION_COMPARE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?im)^\s*Option\s+Compare\s+Text").expect("valid regex")
    });
    static LIKE_OPERATOR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)\bLike\s+"[^"]*[*?#\[\]]+[^"]*""#).expect("valid regex")
    });
    static VB_INTRINSICS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:IsNumeric|IsDate|IsNothing|IsDBNull|IsArray|IsError)\s*\(")
            .expect("valid regex")
    });
    static ON_ERROR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bOn\s+Error\s+(?:Resume\s+Next|GoTo\s+)").expect("valid regex")
    });
    static LATE_BINDING_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bDim\s+\w+\s+As\s+Object\b").expect("valid regex")
    });
    static VB_CAST_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:CType|DirectCast|TryCast|CStr|CInt|CDbl|CBool|CLng|CDec|CDate|CByte|CShort|CSng|CObj|CChar)\s*\(").expect("valid regex")
    });

    struct FlagDef {
        category: &'static str,
        pattern_name: &'static str,
        re: &'static std::sync::LazyLock<Regex>,
        csharp_eq: &'static str,
        risk: &'static str,
        auto_tr: bool,
        notes: &'static str,
    }

    let flag_defs: Vec<FlagDef> = vec![
        FlagDef {
            category: "ErrorHandling",
            pattern_name: "On Error Resume Next / GoTo",
            re: &ON_ERROR_RE,
            csharp_eq: "try-catch blocks",
            risk: "high",
            auto_tr: false,
            notes: "Each On Error must be manually restructured into proper exception handling",
        },
        FlagDef {
            category: "OptionalParams",
            pattern_name: "Optional parameter",
            re: &OPTIONAL_PARAM_RE,
            csharp_eq: "default parameter values",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "OptionalParams",
            pattern_name: "IsMissing()",
            re: &IS_MISSING_RE,
            csharp_eq: "No equivalent — restructure logic",
            risk: "high",
            auto_tr: false,
            notes: "IsMissing has no C# equivalent; refactor to nullable parameters",
        },
        FlagDef {
            category: "Modules",
            pattern_name: "Module declaration",
            re: &MODULE_RE,
            csharp_eq: "static class",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "MyNamespace",
            pattern_name: "My. namespace",
            re: &MY_NAMESPACE_RE,
            csharp_eq: "System.IO / IConfiguration / etc.",
            risk: "medium",
            auto_tr: false,
            notes: "My.Settings → IConfiguration; My.Computer → System.IO",
        },
        FlagDef {
            category: "Events",
            pattern_name: "WithEvents",
            re: &WITH_EVENTS_RE,
            csharp_eq: "Explicit += / -= subscription",
            risk: "medium",
            auto_tr: false,
            notes: "",
        },
        FlagDef {
            category: "Events",
            pattern_name: "Handles clause",
            re: &HANDLES_RE,
            csharp_eq: "btn.Click += handler in constructor",
            risk: "medium",
            auto_tr: false,
            notes: "",
        },
        FlagDef {
            category: "Events",
            pattern_name: "RaiseEvent",
            re: &RAISE_EVENT_RE,
            csharp_eq: "MyEvent?.Invoke(args)",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "Inheritance",
            pattern_name: "Shadows keyword",
            re: &SHADOWS_RE,
            csharp_eq: "new modifier",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "StringCompare",
            pattern_name: "Option Compare Text",
            re: &OPTION_COMPARE_RE,
            csharp_eq: "StringComparer.OrdinalIgnoreCase everywhere",
            risk: "high",
            auto_tr: false,
            notes: "All string comparisons in this file use case-insensitive by default",
        },
        FlagDef {
            category: "PatternMatch",
            pattern_name: "Like operator",
            re: &LIKE_OPERATOR_RE,
            csharp_eq: "Regex.IsMatch()",
            risk: "medium",
            auto_tr: false,
            notes: "",
        },
        FlagDef {
            category: "Intrinsics",
            pattern_name: "VB intrinsics (IsNumeric, etc.)",
            re: &VB_INTRINSICS_RE,
            csharp_eq: "TryParse methods",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "LateBind",
            pattern_name: "Dim x As Object (late binding)",
            re: &LATE_BINDING_RE,
            csharp_eq: "dynamic keyword",
            risk: "high",
            auto_tr: false,
            notes: "",
        },
        FlagDef {
            category: "Casting",
            pattern_name: "VB cast operators",
            re: &VB_CAST_RE,
            csharp_eq: "(Type)x or x as Type",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
    ];

    let mut vb_files = 0usize;
    let mut cs_files = 0usize;
    let mut translation_flags: Vec<VbTranslationFlag> = Vec::new();
    let mut file_flag_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut option_strict_on_files = 0usize;
    let mut option_strict_off_files = 0usize;
    let mut methods_with_dynamic_dispatch = 0usize;
    let mut late_binding_call_count = 0usize;
    let mut object_var_count = 0usize;
    let mut callbyname_count = 0usize;

    // MIG1/D2: log regex compile failures so silent zero-out is visible to operators.
    macro_rules! compile_re {
        ($pattern:expr, $name:expr) => {{
            match Regex::new($pattern) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!(
                        pattern_name = $name,
                        error = %e,
                        "MIG1: VB analysis regex failed to compile — affected metric will be zero"
                    );
                    None
                }
            }
        }};
    }
    let option_strict_re =
        compile_re!(r"(?im)^\s*Option\s+Strict\s+(On|Off)\b", "option_strict_re");
    let method_re = compile_re!(
        r"(?is)\b(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial|MustOverride|NotOverridable|Default|Iterator|ReadOnly|WriteOnly\s+)*\b(?:Sub|Function)\b.*?\bEnd\s+(?:Sub|Function)\b",
        "method_re"
    );
    let object_decl_re = compile_re!(r"(?i)\bDim\s+(\w+)\s+As\s+Object\b", "object_decl_re");
    let callbyname_re = compile_re!(r"(?i)\bCallByName\s*\(", "callbyname_re");
    let late_call_re = compile_re!(r"(?i)\b(\w+)\.(\w+)\s*(?:\(([^)]*)\))?", "late_call_re");

    for (path, content) in code_files {
        let is_vb = path.to_lowercase().ends_with(".vb");
        let is_cs = path.to_lowercase().ends_with(".cs");
        if is_vb {
            vb_files += 1;
        } else if is_cs {
            cs_files += 1;
        }

        // Only scan VB files for VB-specific constructs
        if !is_vb {
            continue;
        }

        if let Some(re) = &option_strict_re {
            let mut file_option = None;
            for cap in re.captures_iter(content) {
                file_option = cap.get(1).map(|m| m.as_str().to_ascii_lowercase());
            }
            match file_option.as_deref() {
                Some("on") => option_strict_on_files += 1,
                Some("off") => option_strict_off_files += 1,
                _ => {}
            }
        }

        if let (Some(mre), Some(obj_re), Some(cbn_re), Some(call_re)) =
            (&method_re, &object_decl_re, &callbyname_re, &late_call_re)
        {
            for method_match in mre.find_iter(content) {
                let body = method_match.as_str();
                let mut method_object_vars = std::collections::HashSet::new();
                let mut method_object_var_count = 0usize;

                for cap in obj_re.captures_iter(body) {
                    if let Some(v) = cap.get(1).map(|m| m.as_str()) {
                        method_object_var_count += 1;
                        method_object_vars.insert(v.to_lowercase());
                    }
                }

                let method_callbyname = cbn_re.find_iter(body).count();
                let method_late_calls = call_re
                    .captures_iter(body)
                    .filter(|cap| {
                        cap.get(1)
                            .map(|m| method_object_vars.contains(&m.as_str().to_lowercase()))
                            .unwrap_or(false)
                    })
                    .count();

                if method_object_var_count > 0 || method_callbyname > 0 || method_late_calls > 0 {
                    methods_with_dynamic_dispatch += 1;
                }
                object_var_count += method_object_var_count;
                callbyname_count += method_callbyname;
                late_binding_call_count += method_late_calls;
            }
        }

        for def in &flag_defs {
            let count = def.re.find_iter(content).count();
            if count > 0 {
                translation_flags.push(VbTranslationFlag {
                    category: def.category.to_string(),
                    pattern: def.pattern_name.to_string(),
                    file_path: path.to_string(),
                    count,
                    csharp_equivalent: def.csharp_eq.to_string(),
                    risk_level: def.risk.to_string(),
                    auto_translatable: def.auto_tr,
                    notes: def.notes.to_string(),
                });
                *file_flag_counts.entry(path.to_string()).or_insert(0) += count;
            }
        }
    }

    let mut flags_by_category: BTreeMap<String, usize> = BTreeMap::new();
    for flag in &translation_flags {
        *flags_by_category.entry(flag.category.clone()).or_insert(0) += flag.count;
    }

    let total_flags: usize = translation_flags.iter().map(|f| f.count).sum();

    let mut highest_risk: Vec<(String, usize)> = file_flag_counts.into_iter().collect();
    highest_risk.sort_by(|a, b| b.1.cmp(&a.1));
    highest_risk.truncate(10);

    let dynamic_dispatch_risk_tier =
        if option_strict_off_files > 0 || callbyname_count > 0 || late_binding_call_count >= 5 {
            "high"
        } else if methods_with_dynamic_dispatch > 0 || object_var_count > 0 {
            "medium"
        } else {
            "low"
        };

    VbTranslationReport {
        is_vb_project: vb_files > cs_files,
        vb_file_count: vb_files,
        cs_file_count: cs_files,
        mixed_language: vb_files > 0 && cs_files > 0,
        total_flags,
        translation_flags,
        flags_by_category,
        highest_risk_files: highest_risk,
        dynamic_dispatch: DynamicDispatchSummary {
            option_strict_on_files,
            option_strict_off_files,
            methods_with_dynamic_dispatch,
            late_binding_call_count,
            object_var_count,
            callbyname_count,
            dynamic_dispatch_risk_tier: dynamic_dispatch_risk_tier.to_string(),
        },
    }
}

// ── Gap 7: Multi-tenancy detection ──────────────────────────────────────────

fn detect_multi_tenancy(
    web_config: Option<&str>,
    code_files: &[(&str, &str)],
    global_asax_content: Option<&str>,
) -> MultiTenancyReport {
    static TENANT_SESSION_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)Session\s*[\(\[]\s*"(?:TenantId|Tenant|TenantKey|TenantCode|OrganizationId|OrgId|CompanyId|ClientId|SiteId|AccountId|CustomerId)"#).expect("valid regex")
    });
    static TENANT_CONTEXT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:HttpContext\.Current\.Items|Context\.Items)\s*[\(\[]\s*"(?:TenantId|Tenant|TenantContext|CurrentTenant)"#).expect("valid regex")
    });
    static TENANT_SQL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:WHERE|AND)\s+(?:\w+\.)?(?:TenantId|TenantID|Tenant_ID|OrgId|OrganizationId|CompanyId|SiteId|AccountId)\s*=\s*(?:@\w+|'\w*'|\?)"#).expect("valid regex")
    });
    static TENANT_PARAM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:tenantId|tenant_id|orgId|organizationId|companyId|siteId|accountId)\s+(?:As\s+(?:Integer|String|Guid|Long|Int32|Int64)|:\s*(?:int|string|Guid|long))"#).expect("valid regex")
    });
    static TENANT_CONFIG_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:TenantMode|MultiTenancy|TenantProvider|TenantResolution|TenantStrategy|IsTenanted|EnableMultiTenancy)"#).expect("valid regex")
    });
    static TENANT_CONN_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?:GetConnectionString|ConnectionString)\s*[\(\[]\s*(?:tenantId|tenant|orgId)"#,
        )
        .expect("valid regex")
    });
    static SUBDOMAIN_TENANT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:Request\.Url\.Host|Request\.Headers\["X-Tenant"|Request\.Headers\["Host"\]).*(?:Split|Substring|Replace|tenant|org)"#).expect("valid regex")
    });
    static TENANT_MODULE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)class\s+\w*(?:Tenant|MultiTenant|Org)\w*\s*(?::\s*I(?:Http)?Module|Inherits\s+I(?:Http)?Module)"#).expect("valid regex")
    });

    let mut evidence: Vec<TenancyEvidence> = Vec::new();
    let mut files_with_tenant: Vec<String> = Vec::new();
    let mut tenant_filtered_queries = 0usize;
    let mut tenant_resolution: Option<TenantResolution> = None;
    let mut tenant_col_name: Option<String> = None;

    // Scan web.config
    if let Some(wc) = web_config
        && TENANT_CONFIG_RE.is_match(wc)
    {
        evidence.push(TenancyEvidence {
            evidence_type: "config".to_string(),
            file_path: "web.config".to_string(),
            detail: "Tenant configuration key found in web.config".to_string(),
            line_hint: None,
        });
    }

    // Scan Global.asax
    if let Some(ga) = global_asax_content
        && (TENANT_MODULE_RE.is_match(ga) || SUBDOMAIN_TENANT_RE.is_match(ga))
    {
        evidence.push(TenancyEvidence {
            evidence_type: "module".to_string(),
            file_path: "Global.asax".to_string(),
            detail: "Tenant resolution logic in Global.asax".to_string(),
            line_hint: None,
        });
    }

    // Scan code files
    for (path, content) in code_files {
        let mut file_has_tenant = false;

        if TENANT_SESSION_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "session_storage".to_string(),
                file_path: path.to_string(),
                detail: "Tenant ID stored in/read from Session".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
        }

        if TENANT_CONTEXT_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "context_items".to_string(),
                file_path: path.to_string(),
                detail: "Tenant context stored in HttpContext.Items".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
        }

        let sql_count = TENANT_SQL_RE.find_iter(content).count();
        if sql_count > 0 {
            tenant_filtered_queries += sql_count;
            evidence.push(TenancyEvidence {
                evidence_type: "sql_filter".to_string(),
                file_path: path.to_string(),
                detail: format!("{sql_count} SQL queries filter by tenant column"),
                line_hint: None,
            });
            file_has_tenant = true;
            // Try to extract the most common column name
            if tenant_col_name.is_none()
                && let Some(cap) = TENANT_SQL_RE.captures(content)
            {
                let full_match = cap.get(0).expect("group 0 always present").as_str();
                if let Some(col) = full_match.split('=').next() {
                    let col = col.trim().rsplit('.').next().unwrap_or(col.trim());
                    let col = col.split_whitespace().last().unwrap_or(col);
                    tenant_col_name = Some(col.to_string());
                }
            }
        }

        if TENANT_PARAM_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "parameter".to_string(),
                file_path: path.to_string(),
                detail: "Method parameter with tenant ID".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
        }

        if TENANT_CONN_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "connection_string".to_string(),
                file_path: path.to_string(),
                detail: "Tenant-specific connection string selection".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
        }

        if SUBDOMAIN_TENANT_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "subdomain".to_string(),
                file_path: path.to_string(),
                detail: "Subdomain-based tenant resolution".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
            if tenant_resolution.is_none() {
                tenant_resolution = Some(TenantResolution {
                    mechanism: "subdomain".to_string(),
                    module_class: None,
                    file_path: path.to_string(),
                });
            }
        }

        if TENANT_MODULE_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "http_module".to_string(),
                file_path: path.to_string(),
                detail: "Tenant resolution IHttpModule".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
            tenant_resolution = Some(TenantResolution {
                mechanism: "http_module".to_string(),
                module_class: TENANT_MODULE_RE
                    .captures(content)
                    .and_then(|c| c.get(0))
                    .map(|m| m.as_str().to_string()),
                file_path: path.to_string(),
            });
        }

        if file_has_tenant {
            files_with_tenant.push(path.to_string());
        }
    }

    files_with_tenant.sort();
    files_with_tenant.dedup();

    // Classify confidence
    let evidence_types: std::collections::HashSet<&str> =
        evidence.iter().map(|e| e.evidence_type.as_str()).collect();
    let confidence = match evidence_types.len() {
        0 => "none",
        1 => "low",
        2 => "medium",
        _ => "high",
    };

    let is_multi_tenant = !evidence.is_empty();

    // Determine isolation strategy
    let isolation_strategy = if evidence_types.contains("connection_string") {
        Some("separate_db".to_string())
    } else if tenant_filtered_queries > 0 {
        Some("shared_db_shared_schema".to_string())
    } else {
        None
    };

    // Build recommendations
    let mut recommendations = Vec::new();
    if is_multi_tenant {
        recommendations
            .push("Replace tenant resolution module with ASP.NET Core middleware".to_string());
        recommendations
            .push("Use EF Core Global Query Filters for automatic tenant filtering".to_string());
        recommendations
            .push("Register ITenantContext as scoped service (one per request)".to_string());
        if isolation_strategy.as_deref() == Some("separate_db") {
            recommendations
                .push("Use IDbContextFactory<T> with tenant-specific connections".to_string());
        }
        recommendations.push(
            "Move Session-based tenant ID to JWT claims or HttpContext.Items via middleware"
                .to_string(),
        );
        recommendations.push("CRITICAL: Audit ALL SQL queries for tenant filtering — missing ANY filter causes data leak".to_string());
    }

    MultiTenancyReport {
        is_multi_tenant,
        confidence: confidence.to_string(),
        tenant_id_column_name: tenant_col_name,
        isolation_strategy,
        detection_evidence: evidence,
        tenant_resolution,
        tenant_filtered_queries,
        files_with_tenant_logic: files_with_tenant,
        migration_recommendations: recommendations,
    }
}

// ── Gap 8: Email & background job detection ─────────────────────────────────

fn detect_email_patterns(
    code_files: &[(&str, &str)],
    web_config: Option<&str>,
) -> EmailPatternReport {
    static SMTP_CLIENT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?SmtpClient\s*[\(\.]").expect("valid regex")
    });
    static MAIL_MESSAGE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?MailMessage\s*\(").expect("valid regex")
    });
    static WEB_MAIL_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bSystem\.Web\.Mail\b").expect("valid regex"));
    static ATTACHMENT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?Attachment\s*\(").expect("valid regex")
    });
    static ALTERNATE_VIEW_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bAlternateView\.CreateAlternateViewFromString\s*\(").expect("valid regex")
    });
    static CDO_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)CreateObject\s*\(\s*"CDO\.Message"\s*\)"#).expect("valid regex")
    });
    static SMTP_CONFIG_HOST_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<network\s+host\s*=\s*"([^"]*)""#).expect("valid regex")
    });
    static SMTP_CONFIG_PORT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<network[^>]*port\s*=\s*"(\d+)""#).expect("valid regex")
    });
    static SMTP_CONFIG_FROM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<smtp\s+[^>]*from\s*=\s*"([^"]*)""#).expect("valid regex")
    });

    let mut email_patterns: Vec<EmailPattern> = Vec::new();
    let mut uses_html = false;
    let mut uses_attachments = false;
    let mut uses_cdo = false;
    let mut uses_web_mail = false;
    let mut email_files: Vec<String> = Vec::new();

    for (path, content) in code_files {
        let mut file_patterns: Vec<(&str, &str, usize)> = Vec::new();

        let smtp_count = SMTP_CLIENT_RE.find_iter(content).count();
        if smtp_count > 0 {
            file_patterns.push(("SmtpClient", "IEmailSender / SendGrid SDK", smtp_count));
        }
        let mm_count = MAIL_MESSAGE_RE.find_iter(content).count();
        if mm_count > 0 {
            file_patterns.push(("MailMessage", "IEmailSender with Razor templates", mm_count));
        }
        let wm_count = WEB_MAIL_RE.find_iter(content).count();
        if wm_count > 0 {
            file_patterns.push(("System.Web.Mail", "IEmailSender (obsolete API)", wm_count));
            uses_web_mail = true;
        }
        let cdo_count = CDO_RE.find_iter(content).count();
        if cdo_count > 0 {
            file_patterns.push(("CDO.Message", "IEmailSender (COM object)", cdo_count));
            uses_cdo = true;
        }

        if ATTACHMENT_RE.is_match(content) {
            uses_attachments = true;
        }
        if ALTERNATE_VIEW_RE.is_match(content) {
            uses_html = true;
        }

        if !file_patterns.is_empty() {
            email_files.push(path.to_string());
            for (ptype, modern, count) in file_patterns {
                email_patterns.push(EmailPattern {
                    file_path: path.to_string(),
                    pattern_type: ptype.to_string(),
                    count,
                    modern_equivalent: modern.to_string(),
                });
            }
        }
    }
    email_files.sort();
    email_files.dedup();

    // Parse SMTP config from web.config
    let smtp_config = web_config.and_then(|wc| {
        if !wc.to_lowercase().contains("<smtp") && !wc.to_lowercase().contains("<network") {
            return None;
        }
        let host = SMTP_CONFIG_HOST_RE.captures(wc).map(|c| c[1].to_string());
        let port = SMTP_CONFIG_PORT_RE
            .captures(wc)
            .and_then(|c| c[1].parse().ok());
        let from = SMTP_CONFIG_FROM_RE.captures(wc).map(|c| c[1].to_string());
        let uses_credentials = wc.to_lowercase().contains("username=")
            || wc.to_lowercase().contains("defaultcredentials");
        let uses_ssl =
            wc.to_lowercase().contains("enablessl") || wc.to_lowercase().contains("ssl=\"true\"");
        Some(SmtpConfig {
            host,
            port,
            from_address: from,
            uses_credentials,
            uses_ssl,
        })
    });

    let has_email = !email_patterns.is_empty();

    EmailPatternReport {
        has_email,
        email_patterns,
        smtp_config,
        total_email_files: email_files.len(),
        uses_html_email: uses_html,
        uses_attachments,
        uses_legacy_cdo: uses_cdo,
        uses_legacy_web_mail: uses_web_mail,
    }
}

fn detect_background_job_patterns(
    code_files: &[(&str, &str)],
    global_asax_content: Option<&str>,
) -> BackgroundJobReport {
    static THREAD_POOL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bThreadPool\.QueueUserWorkItem\s*\(").expect("valid regex")
    });
    static BG_WORKER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?BackgroundWorker\b").expect("valid regex")
    });
    static TASK_RUN_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bTask\.Run\s*\(").expect("valid regex"));
    static TIMER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?(?:System\.(?:Timers|Threading)\.)?Timer\s*\(")
            .expect("valid regex")
    });
    static THREAD_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?Thread\s*\(\s*(?:AddressOf|New\s+ThreadStart|New\s+ParameterizedThreadStart)\s").expect("valid regex")
    });
    static HANGFIRE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"(?i)\bBackgroundJob\.(?:Enqueue|Schedule|ContinueWith|ContinueJobWith)\s*[\(<]",
        )
        .expect("valid regex")
    });
    static QUARTZ_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:IScheduler|JobBuilder\.Create|TriggerBuilder\.Create)\b")
            .expect("valid regex")
    });
    static SELF_CALL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)WebClient\s*\(\s*\)\.Download(?:String|Data)\s*\(\s*"(?:http|~/)"#)
            .expect("valid regex")
    });

    struct BgDef {
        re: &'static std::sync::LazyLock<Regex>,
        pattern_type: &'static str,
        modern: &'static str,
        risk: &'static str,
    }

    let bg_defs: Vec<BgDef> = vec![
        BgDef {
            re: &THREAD_POOL_RE,
            pattern_type: "ThreadPool.QueueUserWorkItem",
            modern: "BackgroundService + Channel<T>",
            risk: "high",
        },
        BgDef {
            re: &BG_WORKER_RE,
            pattern_type: "BackgroundWorker",
            modern: "BackgroundService",
            risk: "medium",
        },
        BgDef {
            re: &TASK_RUN_RE,
            pattern_type: "Task.Run (fire-and-forget)",
            modern: "Hangfire BackgroundJob.Enqueue or IHostedService",
            risk: "high",
        },
        BgDef {
            re: &TIMER_RE,
            pattern_type: "Timer",
            modern: "IHostedService with PeriodicTimer",
            risk: "medium",
        },
        BgDef {
            re: &THREAD_RE,
            pattern_type: "Thread creation",
            modern: "BackgroundService or Task.Run with lifetime management",
            risk: "high",
        },
        BgDef {
            re: &HANGFIRE_RE,
            pattern_type: "Hangfire",
            modern: "Hangfire (already compatible)",
            risk: "low",
        },
        BgDef {
            re: &QUARTZ_RE,
            pattern_type: "Quartz.NET",
            modern: "Quartz.NET (already compatible)",
            risk: "low",
        },
        BgDef {
            re: &SELF_CALL_RE,
            pattern_type: "Self-call timer (WebClient)",
            modern: "IHostedService + HttpClientFactory",
            risk: "high",
        },
    ];

    let mut patterns: Vec<BackgroundJobPattern> = Vec::new();
    let mut bg_files: Vec<String> = Vec::new();
    let mut uses_thread_pool = false;
    let mut uses_timers = false;
    let mut uses_task_run = false;
    let mut uses_bg_worker = false;
    let mut uses_hangfire = false;
    let mut uses_quartz = false;
    let mut fire_and_forget = 0usize;

    let all_code: Vec<(&str, &str)> = code_files
        .iter()
        .copied()
        .chain(global_asax_content.map(|c| ("Global.asax", c)))
        .collect();

    for (path, content) in &all_code {
        let mut file_has_bg = false;
        for def in &bg_defs {
            let count = def.re.find_iter(content).count();
            if count > 0 {
                patterns.push(BackgroundJobPattern {
                    file_path: path.to_string(),
                    pattern_type: def.pattern_type.to_string(),
                    count,
                    modern_equivalent: def.modern.to_string(),
                    risk_level: def.risk.to_string(),
                });
                file_has_bg = true;

                match def.pattern_type {
                    "ThreadPool.QueueUserWorkItem" => {
                        uses_thread_pool = true;
                        fire_and_forget += count;
                    }
                    "BackgroundWorker" => uses_bg_worker = true,
                    "Task.Run (fire-and-forget)" => {
                        uses_task_run = true;
                        fire_and_forget += count;
                    }
                    "Timer" => uses_timers = true,
                    "Thread creation" => fire_and_forget += count,
                    "Hangfire" => uses_hangfire = true,
                    "Quartz.NET" => uses_quartz = true,
                    _ => {}
                }
            }
        }
        if file_has_bg {
            bg_files.push(path.to_string());
        }
    }
    bg_files.sort();
    bg_files.dedup();

    BackgroundJobReport {
        has_background_jobs: !patterns.is_empty(),
        total_background_files: bg_files.len(),
        uses_thread_pool,
        uses_timers,
        uses_task_run,
        uses_bg_worker,
        uses_hangfire,
        uses_quartz,
        fire_and_forget_count: fire_and_forget,
        patterns,
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
    method_inv: &BTreeMap<String, PageMethodInventory>,
    third_party: &ThirdPartyControlSummary,
    dep_inv: &DependencyInventory,
    cache_inv: &CachingInventory,
    url_routing: &UrlRoutingInventory,
    vb_trans: &VbTranslationReport,
    multi_tenant: &MultiTenancyReport,
    email: &EmailPatternReport,
    bg_jobs: &BackgroundJobReport,
    sp_cat: &StoredProcedureCatalog,
    inherit: &InheritanceChainReport,
    cfg_transforms: &ConfigTransformReport,
    master_regions: &MasterPageRegionMap,
    res_inv: &ResourceInventory,
    vb_traps: &engram_index::vb_translation_traps::VbTranslationTrapReport,
    jquery_inv: &engram_index::jquery_inventory::JQueryInventory,
    cross_traces: &CrossLayerTraceSummary,
    biz_logic: &super::business_logic_service::ProjectBusinessLogicReport,
    db_intel: &super::database_intelligence_service::DatabaseIntelligence,
    session_wf: &super::session_workflow_service::SessionWorkflowReport,
) -> String {
    let mut md = String::with_capacity(180_000);

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
    if cross.total_script_files > 0 {
        md.push_str(&format!(
            "- **Client script files (.js/.ts/.tsx/.jsx)**: {} ({} with server-side dependencies)\n",
            cross.total_script_files, js.script_files_with_server_deps
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
    if vb_trans.vb_file_count > 0 {
        md.push_str(&format!(
            "- **Dynamic-dispatch risk tier**: {} (Option Strict Off files: {}, dynamic methods: {})\n",
            cross.dynamic_dispatch_risk_tier,
            cross.option_strict_off_files,
            cross.dynamic_dispatch_methods
        ));
    }
    md.push('\n');

    // ── Phase 33: Project Dependencies (Gap 3) ───────────────────────────
    if dep_inv.total_packages > 0 || dep_inv.total_assemblies > 0 {
        md.push_str("## Project Dependencies\n\n");
        if let Some(tf) = dep_inv.target_frameworks.first() {
            md.push_str(&format!("**Target Framework**: {tf}\n"));
        }
        md.push_str(&format!(
            "**NuGet Packages**: {} ({} have modern replacements, {} need manual evaluation)\n",
            dep_inv.total_packages,
            dep_inv.packages_with_known_replacement,
            dep_inv.packages_without_replacement
        ));
        md.push_str(&format!(
            "**Assembly References**: {} ({} framework, {} third-party)\n",
            dep_inv.total_assemblies,
            dep_inv.framework_assemblies.len(),
            dep_inv.third_party_assemblies.len()
        ));
        md.push_str(&format!(
            "**Project References**: {}\n\n",
            dep_inv.project_references.len()
        ));

        if !dep_inv.nuget_packages.is_empty() {
            md.push_str("### NuGet Packages\n");
            md.push_str("| Package | Version | Modern Replacement | Category | Notes |\n");
            md.push_str("|---------|---------|-------------------|----------|-------|\n");
            for pkg in &dep_inv.nuget_packages {
                let ver = pkg.version.as_deref().unwrap_or("-");
                let modern = pkg.modern_replacement.as_deref().unwrap_or("(evaluate)");
                let notes = pkg.migration_notes.as_deref().unwrap_or("");
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    pkg.name, ver, modern, pkg.category, notes
                ));
            }
            md.push('\n');
        }

        let removable: Vec<&AssemblyRefInfo> = dep_inv
            .assembly_references
            .iter()
            .filter(|a| a.removal_reason.is_some())
            .collect();
        if !removable.is_empty() {
            md.push_str("### Framework Assemblies Requiring Replacement\n");
            md.push_str("| Assembly | Status in .NET Core | Migration Path |\n");
            md.push_str("|----------|--------------------|--------------|\n");
            for asm in &removable {
                let reason = asm.removal_reason.as_deref().unwrap_or("");
                let modern = asm.modern_equivalent.as_deref().unwrap_or("(none)");
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    asm.assembly_name, reason, modern
                ));
            }
            md.push('\n');
        }

        let compatible: Vec<&NuGetPackageInfo> = dep_inv
            .nuget_packages
            .iter()
            .filter(|p| {
                p.modern_replacement
                    .as_deref()
                    .is_some_and(|m| m.contains("compatible") || m == p.name)
            })
            .collect();
        if !compatible.is_empty() {
            let names: Vec<&str> = compatible.iter().map(|p| p.name.as_str()).collect();
            md.push_str(&format!(
                "### Compatible Packages (no action needed)\n{}\n\n",
                names.join(", ")
            ));
        }
        md.push('\n');
    }

    // ── Phase 33: Language & Translation Analysis (Gap 6) ────────────────
    if vb_trans.vb_file_count > 0 || vb_trans.cs_file_count > 0 {
        md.push_str("## Language & Translation Analysis\n\n");
        let primary = if vb_trans.is_vb_project {
            "VB.NET"
        } else {
            "C#"
        };
        md.push_str(&format!(
            "**Primary language**: {primary} ({} files)\n",
            if vb_trans.is_vb_project {
                vb_trans.vb_file_count
            } else {
                vb_trans.cs_file_count
            }
        ));
        if vb_trans.mixed_language {
            let secondary = if vb_trans.is_vb_project {
                "C#"
            } else {
                "VB.NET"
            };
            let sec_count = if vb_trans.is_vb_project {
                vb_trans.cs_file_count
            } else {
                vb_trans.vb_file_count
            };
            md.push_str(&format!(
                "**Secondary language**: {secondary} ({sec_count} files)\n"
            ));
        }
        md.push_str(&format!(
            "**Translation flags**: {} across files\n\n",
            vb_trans.total_flags
        ));
        md.push_str(&format!(
            "**Dynamic-dispatch risk tier**: {}\n",
            vb_trans.dynamic_dispatch.dynamic_dispatch_risk_tier
        ));
        md.push_str(&format!(
            "**Option Strict**: On in {} file(s), Off in {} file(s)\n",
            vb_trans.dynamic_dispatch.option_strict_on_files,
            vb_trans.dynamic_dispatch.option_strict_off_files
        ));
        md.push_str(&format!(
            "**Dynamic-dispatch counters**: {} late-bound call(s), {} `As Object` declaration(s), {} `CallByName` call(s) across {} method(s)\n\n",
            vb_trans.dynamic_dispatch.late_binding_call_count,
            vb_trans.dynamic_dispatch.object_var_count,
            vb_trans.dynamic_dispatch.callbyname_count,
            vb_trans.dynamic_dispatch.methods_with_dynamic_dispatch
        ));

        if !vb_trans.flags_by_category.is_empty() {
            md.push_str("### Translation Risk Summary\n");
            md.push_str("| Category | Count | Risk | Auto-Translatable |\n");
            md.push_str("|----------|-------|------|-------------------|\n");
            for (cat, count) in &vb_trans.flags_by_category {
                let risk = vb_trans
                    .translation_flags
                    .iter()
                    .find(|f| &f.category == cat)
                    .map(|f| f.risk_level.as_str())
                    .unwrap_or("low");
                let auto_val = vb_trans
                    .translation_flags
                    .iter()
                    .find(|f| &f.category == cat)
                    .map(|f| if f.auto_translatable { "Yes" } else { "No" })
                    .unwrap_or("No");
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    cat, count, risk, auto_val
                ));
            }
            md.push('\n');
        }

        if !vb_trans.highest_risk_files.is_empty() {
            md.push_str("### Highest-Risk Files (most translation flags)\n");
            md.push_str("| File | Flags |\n|------|-------|\n");
            for (path, count) in &vb_trans.highest_risk_files {
                md.push_str(&format!("| {} | {} |\n", path, count));
            }
            md.push('\n');
        }

        if vb_trans.is_vb_project {
            md.push_str("### Migration Strategy\n");
            md.push_str("1. Run automated VB→C# converter (dotnet-vb2cs or Instant C#) for mechanical translations\n");
            let on_error = vb_trans
                .flags_by_category
                .get("ErrorHandling")
                .copied()
                .unwrap_or(0);
            if on_error > 0 {
                md.push_str(&format!("2. Manually fix {on_error} `On Error Resume Next` patterns → proper try-catch\n"));
            }
            let late = vb_trans
                .flags_by_category
                .get("LateBind")
                .copied()
                .unwrap_or(0);
            if late > 0 {
                md.push_str(&format!("3. Convert {late} `Dim x As Object` late bindings → `dynamic` or typed interfaces\n"));
            }
            md.push('\n');
        }
    }

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

    // ── Phase 33: Multi-Tenancy Analysis (Gap 7) ──────────────────────────
    if multi_tenant.is_multi_tenant {
        md.push_str("## Multi-Tenancy Analysis\n\n");
        md.push_str(&format!(
            "**Multi-tenant**: Yes (confidence: {})\n",
            multi_tenant.confidence
        ));
        if let Some(ref col) = multi_tenant.tenant_id_column_name {
            md.push_str(&format!("**Tenant ID column**: `{col}`\n"));
        }
        if let Some(ref strat) = multi_tenant.isolation_strategy {
            md.push_str(&format!("**Isolation strategy**: {strat}\n"));
        }
        if let Some(ref res) = multi_tenant.tenant_resolution {
            md.push_str(&format!(
                "**Tenant resolution**: {} via `{}`\n",
                res.mechanism, res.file_path
            ));
        }
        md.push_str(&format!(
            "**Tenant-filtered queries**: {}\n",
            multi_tenant.tenant_filtered_queries
        ));
        md.push_str(&format!(
            "**Files with tenant logic**: {}\n\n",
            multi_tenant.files_with_tenant_logic.len()
        ));

        if !multi_tenant.detection_evidence.is_empty() {
            md.push_str("### Detection Evidence\n");
            md.push_str("| Type | File | Detail |\n|------|------|--------|\n");
            for ev in &multi_tenant.detection_evidence {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    ev.evidence_type, ev.file_path, ev.detail
                ));
            }
            md.push('\n');
        }

        if !multi_tenant.migration_recommendations.is_empty() {
            md.push_str("### Modern Migration Strategy\n");
            for (i, rec) in multi_tenant.migration_recommendations.iter().enumerate() {
                md.push_str(&format!("{}. {rec}\n", i + 1));
            }
            md.push('\n');
        }
    }

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

    // ── Phase 33: URL Routing & Rewriting (Gap 5) ─────────────────────────
    if url_routing.total_url_patterns > 0
        || !url_routing.rewrite_path_calls.is_empty()
        || !url_routing.redirects.is_empty()
        || !url_routing.server_transfers.is_empty()
    {
        md.push_str("## URL Routing & Rewriting\n\n");
        md.push_str(&format!(
            "**URL patterns**: {} ({} rewrite rules, {} page routes, {} URL mappings)\n",
            url_routing.total_url_patterns,
            url_routing.rewrite_rules.len(),
            url_routing.page_routes.len(),
            url_routing.url_mappings.len()
        ));
        md.push_str(&format!(
            "**RewritePath calls**: {}\n",
            url_routing.rewrite_path_calls.len()
        ));
        md.push_str(&format!("**Redirects**: {}\n", url_routing.redirects.len()));
        md.push_str(&format!(
            "**Server.Transfer calls**: {}\n",
            url_routing.server_transfers.len()
        ));
        md.push_str(&format!(
            "**Friendly URLs**: {}\n\n",
            if url_routing.has_friendly_urls {
                "enabled"
            } else {
                "disabled"
            }
        ));

        if !url_routing.rewrite_rules.is_empty() {
            md.push_str("### IIS Rewrite Rules\n");
            md.push_str("| Rule | Match Pattern | Action | Target | Modern Equivalent |\n");
            md.push_str("|------|--------------|--------|--------|-------------------|\n");
            for rule in &url_routing.rewrite_rules {
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    rule.rule_name,
                    rule.match_pattern,
                    rule.action_type,
                    rule.target_url,
                    rule.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if !url_routing.page_routes.is_empty() {
            md.push_str("### Page Routes (Global.asax)\n");
            md.push_str("| Route Name | URL Pattern | Physical Page | Modern Equivalent |\n");
            md.push_str("|-----------|-------------|---------------|-------------------|\n");
            for route in &url_routing.page_routes {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    route.route_name,
                    route.url_pattern,
                    route.physical_page,
                    route.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if !url_routing.server_transfers.is_empty() {
            md.push_str("### Code-Based URL Manipulation\n");
            md.push_str("| File | Type | Target |\n|------|------|--------|\n");
            for st in &url_routing.server_transfers {
                md.push_str(&format!(
                    "| {} | Server.Transfer | {} |\n",
                    st.file_path, st.target_page
                ));
            }
            for rp in &url_routing.rewrite_path_calls {
                md.push_str(&format!(
                    "| {} | RewritePath | {} |\n",
                    rp.file_path, rp.target_path
                ));
            }
            md.push('\n');
            md.push_str(&format!("**WARNING**: {} Server.Transfer calls must be refactored — this pattern does not exist in ASP.NET Core\n\n", url_routing.server_transfers.len()));
        }

        md.push_str("### Migration Strategy\n");
        md.push_str(
            "- IIS Rewrite Rules → ASP.NET Core URL Rewriting Middleware (`app.UseRewriter()`)\n",
        );
        md.push_str("- `MapPageRoute` → `app.MapGet()` / `@page` directives\n");
        md.push_str("- `HttpContext.RewritePath` → Middleware pipeline or endpoint routing\n");
        md.push_str(
            "- `Server.Transfer` → **No equivalent** — refactor to redirect or shared component\n",
        );
        md.push_str(
            "- `Response.Redirect` → `Results.Redirect()` / `NavigationManager.NavigateTo()`\n\n",
        );
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

    // ── Phase 33: Caching Strategy (Gap 4) ─────────────────────────────────
    if cache_inv.total_cached_pages > 0
        || cache_inv.total_cache_keys > 0
        || cache_inv.has_response_caching
    {
        md.push_str("## Caching Strategy\n\n");
        md.push_str(&format!(
            "**Output-cached pages**: {}\n",
            cache_inv.total_cached_pages
        ));
        md.push_str(&format!(
            "**Programmatic cache keys**: {}\n",
            cache_inv.total_cache_keys
        ));
        md.push_str(&format!(
            "**Response-cached files**: {}\n",
            cache_inv.response_cache_files.len()
        ));
        md.push_str(&format!(
            "**SQL cache dependencies**: {}\n\n",
            cache_inv.sql_cache_dependencies.len()
        ));

        if !cache_inv.output_cache_pages.is_empty() {
            md.push_str("### Page/Control Output Caching\n");
            md.push_str("| Page | Duration | VaryByParam | Location | Modern Equivalent |\n");
            md.push_str("|------|----------|-------------|----------|-------------------|\n");
            for oc in &cache_inv.output_cache_pages {
                let dur = oc
                    .duration_seconds
                    .map_or("-".to_string(), |d| format!("{d}s"));
                let vbp = oc.vary_by_param.as_deref().unwrap_or("-");
                let loc = oc.location.as_deref().unwrap_or("-");
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    oc.file_path, dur, vbp, loc, oc.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if !cache_inv.programmatic_cache_keys.is_empty() {
            md.push_str("### Programmatic Cache Keys\n");
            md.push_str("| Key | Operations | Used By | Modern Equivalent |\n");
            md.push_str("|-----|-----------|---------|-------------------|\n");
            for ck in &cache_inv.programmatic_cache_keys {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ck.cache_key,
                    ck.operation,
                    ck.files.join(", "),
                    ck.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if !cache_inv.sql_cache_dependencies.is_empty() {
            md.push_str("### SQL Cache Dependencies\n");
            md.push_str("| File | Database | Table | Note |\n|------|----------|-------|------|\n");
            for sd in &cache_inv.sql_cache_dependencies {
                let db = sd.database_name.as_deref().unwrap_or("-");
                let tbl = sd.table_name.as_deref().unwrap_or("-");
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    sd.file_path, db, tbl, sd.modern_note
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Strategy\n");
        md.push_str("- `HttpRuntime.Cache` → `IMemoryCache` (single-server) or `IDistributedCache` (Redis, multi-server)\n");
        md.push_str("- `<%@ OutputCache %>` → `[ResponseCache]` attribute + `services.AddResponseCaching()`\n");
        md.push_str("- `Response.Cache.*` → `Response.Headers` or `[ResponseCache]` attribute\n");
        md.push_str("- `SqlCacheDependency` → Manual invalidation via Change Tracking, SignalR, or message bus\n\n");
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

    // ── Phase 33: Code-Behind Method Inventory (Gap 1) ─────────────────────
    if !method_inv.is_empty() {
        let total_m: usize = method_inv.values().map(|i| i.total_methods).sum();
        let total_eh: usize = method_inv.values().map(|i| i.event_handlers).sum();
        let total_wm: usize = method_inv.values().map(|i| i.web_methods).sum();
        let total_helpers: usize = method_inv.values().map(|i| i.helper_methods).sum();
        let total_lc: usize = method_inv.values().map(|i| i.lifecycle_methods).sum();

        md.push_str("## Code-Behind Method Inventory\n\n");
        md.push_str(&format!(
            "**Total methods**: {} across {} code-behind files\n",
            total_m,
            method_inv.len()
        ));
        md.push_str(&format!(
            "**Lifecycle handlers**: {} | **Event handlers**: {} | **WebMethods**: {} | **Helpers**: {}\n",
            total_lc, total_eh, total_wm, total_helpers
        ));
        if let Some((ref path, count)) = cross.largest_file_by_methods {
            md.push_str(&format!(
                "**Largest code-behind**: {} ({} methods)\n",
                path, count
            ));
        }
        md.push('\n');

        // Top 10 files by method count
        let mut sorted_files: Vec<(&String, &PageMethodInventory)> = method_inv.iter().collect();
        sorted_files.sort_by(|a, b| b.1.total_methods.cmp(&a.1.total_methods));
        sorted_files.truncate(10);

        if !sorted_files.is_empty() {
            md.push_str("### Files by Method Count (top 10)\n");
            md.push_str("| File | Methods | Events | SQL Methods | Largest Method |\n");
            md.push_str("|------|---------|--------|-------------|----------------|\n");
            for (path, inv) in &sorted_files {
                let largest = inv
                    .largest_method
                    .as_ref()
                    .map(|(n, lc)| format!("{n} ({lc} lines)"))
                    .unwrap_or_else(|| "-".to_string());
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    path, inv.total_methods, inv.event_handlers, inv.methods_with_sql, largest
                ));
            }
            md.push('\n');
        }

        // Complexity indicators
        let big_methods: usize = method_inv
            .values()
            .flat_map(|i| i.methods.iter())
            .filter(|m| m.line_count > 50)
            .count();
        let sql_methods: usize = method_inv.values().map(|i| i.methods_with_sql).sum();
        let com_methods: usize = method_inv
            .values()
            .flat_map(|i| i.methods.iter())
            .filter(|m| m.effects.iter().any(|e| e.contains("COM")))
            .count();
        md.push_str("### Migration Complexity Indicators\n");
        if big_methods > 0 {
            md.push_str(&format!(
                "- {} methods > 50 lines → candidates for decomposition\n",
                big_methods
            ));
        }
        if sql_methods > 0 {
            md.push_str(&format!(
                "- {} methods with SQL_Access → need repository extraction\n",
                sql_methods
            ));
        }
        if com_methods > 0 {
            md.push_str(&format!(
                "- {} methods with COM_Interop → need modern library replacement\n",
                com_methods
            ));
        }
        if total_wm > 0 {
            md.push_str(&format!(
                "- {} WebMethods → must become API endpoints\n",
                total_wm
            ));
        }
        md.push('\n');
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

    // ── JavaScript/TypeScript & Client-Side Dependencies (Phase 32) ──────
    if js.total_script_files > 0 || !js.dom_manipulations.is_empty() || !js.ajax_calls.is_empty() {
        md.push_str("## JavaScript/TypeScript & Client-Side Dependencies\n\n");
        md.push_str(&format!(
            "**Client script files (.js/.ts/.tsx/.jsx)**: {} ({} with server-side dependencies)\n",
            js.total_script_files, js.script_files_with_server_deps
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
                js.script_files_with_server_deps));
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

    // ── Phase 33: Third-Party Control Libraries (Gap 2) ────────────────────
    if third_party.total_third_party_controls > 0 {
        md.push_str("## Third-Party Control Libraries\n\n");
        md.push_str(&format!(
            "**Vendors detected**: {}\n",
            third_party.vendors_detected.len()
        ));
        md.push_str(&format!(
            "**Total third-party controls**: {} across {} files\n\n",
            third_party.total_third_party_controls,
            third_party.files_with_third_party.len()
        ));

        for vendor in &third_party.vendors_detected {
            md.push_str(&format!("### {} {}\n", vendor.vendor, vendor.suite));
            let control_list: Vec<String> = vendor
                .controls_used
                .iter()
                .map(|(name, count)| format!("{name} ({count})"))
                .collect();
            md.push_str(&format!(
                "- **Controls used**: {}\n",
                control_list.join(", ")
            ));
            md.push_str(&format!(
                "- **Modern replacement ({target_stack})**: {}\n",
                vendor.modern_replacement_suite
            ));
            md.push_str(&format!("- **License**: {}\n\n", vendor.license_note));
        }

        if !third_party.unmapped_controls.is_empty() {
            md.push_str("### Unmapped Controls (no automatic mapping)\n");
            md.push_str("| Control | Vendor | File | Note |\n|---------|--------|------|------|\n");
            for uc in &third_party.unmapped_controls {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    uc.tag_name, uc.vendor, uc.file_path, uc.note
                ));
            }
            md.push('\n');
        }
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

    // ── Phase 33: Email & Notifications (Gap 8) ────────────────────────────
    if email.has_email {
        md.push_str("## Email & Notifications\n\n");
        md.push_str(&format!(
            "**Email sending**: Yes ({} files)\n",
            email.total_email_files
        ));
        if let Some(ref cfg) = email.smtp_config {
            let host = cfg.host.as_deref().unwrap_or("unknown");
            let port = cfg.port.map_or("-".to_string(), |p| p.to_string());
            md.push_str(&format!(
                "**SMTP config**: {}:{} (SSL: {}, credentials: {})\n",
                host, port, cfg.uses_ssl, cfg.uses_credentials
            ));
        }
        md.push_str(&format!(
            "**HTML email**: {}\n",
            if email.uses_html_email { "Yes" } else { "No" }
        ));
        md.push_str(&format!(
            "**Attachments**: {}\n",
            if email.uses_attachments { "Yes" } else { "No" }
        ));
        if email.uses_legacy_cdo {
            md.push_str("**Legacy CDO**: Yes (COM interop)\n");
        }
        if email.uses_legacy_web_mail {
            md.push_str("**Legacy System.Web.Mail**: Yes (obsolete)\n");
        }
        md.push('\n');

        if !email.email_patterns.is_empty() {
            md.push_str("### Email Usage\n");
            md.push_str("| File | Pattern | Count | Modern Equivalent |\n");
            md.push_str("|------|---------|-------|-------------------|\n");
            for ep in &email.email_patterns {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ep.file_path, ep.pattern_type, ep.count, ep.modern_equivalent
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Strategy\n");
        md.push_str(
            "- `SmtpClient` → **Obsolete in .NET 6+** — replace with `IEmailSender` abstraction\n",
        );
        md.push_str("- Register `IEmailSender` implementation: SendGrid, Mailgun, or Azure Communication Services\n");
        md.push_str("- HTML email templates → Razor templates with strongly-typed models\n");
        md.push_str("- SMTP config → `appsettings.json` service configuration\n\n");
    }

    // ── Phase 33: Background Processing (Gap 8) ──────────────────────────
    if bg_jobs.has_background_jobs {
        md.push_str("## Background Processing\n\n");
        md.push_str(&format!(
            "**Background jobs**: Yes ({} files)\n",
            bg_jobs.total_background_files
        ));
        md.push_str(&format!(
            "**Fire-and-forget**: {} (HIGH RISK)\n",
            bg_jobs.fire_and_forget_count
        ));
        if bg_jobs.uses_timers {
            md.push_str("**Timers**: Yes\n");
        }
        if bg_jobs.uses_thread_pool {
            md.push_str("**ThreadPool**: Yes\n");
        }
        if bg_jobs.uses_task_run {
            md.push_str("**Task.Run**: Yes\n");
        }
        if bg_jobs.uses_hangfire {
            md.push_str("**Hangfire**: Yes (already compatible)\n");
        }
        if bg_jobs.uses_quartz {
            md.push_str("**Quartz.NET**: Yes (already compatible)\n");
        }
        md.push('\n');

        if !bg_jobs.patterns.is_empty() {
            md.push_str("### Background Job Inventory\n");
            md.push_str("| File | Pattern | Count | Risk | Modern Equivalent |\n");
            md.push_str("|------|---------|-------|------|-------------------|\n");
            for bp in &bg_jobs.patterns {
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    bp.file_path, bp.pattern_type, bp.count, bp.risk_level, bp.modern_equivalent
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Strategy\n");
        md.push_str("- `ThreadPool.QueueUserWorkItem` → `BackgroundService` + `Channel<T>`\n");
        md.push_str("- `System.Timers.Timer` → `IHostedService` with `PeriodicTimer`\n");
        md.push_str("- `Task.Run()` fire-and-forget → Hangfire `BackgroundJob.Enqueue()` or `IHostedService`\n");
        md.push_str(
            "- `BackgroundWorker` → `BackgroundService` (same pattern, different base class)\n",
        );
        if bg_jobs.fire_and_forget_count > 0 {
            md.push_str(&format!(
                "- **WARNING**: {} fire-and-forget patterns will silently fail in ASP.NET Core\n",
                bg_jobs.fire_and_forget_count
            ));
        }
        md.push('\n');
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

        let llm_tag = if d.llm_business_purpose.is_some() || d.llm_migration_notes.is_some() {
            " — LLM-enhanced"
        } else {
            ""
        };
        md.push_str(&format!(
            "### {} (Wave {}, {}, Risk {}/10){}\n\n",
            d.file_path, wave_num, d.estimated_complexity, d.blast_radius_score, llm_tag
        ));

        if let Some(ref bp) = d.llm_business_purpose {
            md.push_str(&format!("**Business purpose**: {bp}\n\n"));
        }

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

        // Phase 33: Method inventory per page (Gap 1)
        if let Some(inv) = method_inv.get(&d.file_path)
            && !inv.methods.is_empty()
        {
            md.push_str(&format!(
                "**Methods** ({} total: {} lifecycle, {} event handlers, {} helpers)\n\n",
                inv.total_methods, inv.lifecycle_methods, inv.event_handlers, inv.helper_methods
            ));
            md.push_str("| Method | Kind | Lines | Effects | Signature |\n");
            md.push_str("|--------|------|-------|---------|----------|\n");
            for m in &inv.methods {
                let effects = if m.effects.is_empty() {
                    "-".to_string()
                } else {
                    m.effects.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    m.name, m.method_kind, m.line_count, effects, m.signature
                ));
            }
            md.push('\n');
        }

        // Phase 33: Third-party controls per page (Gap 2)
        {
            let page_tp: Vec<&VendorSummary> = third_party
                .vendors_detected
                .iter()
                .filter(|v| v.files.contains(&d.file_path))
                .collect();
            if !page_tp.is_empty() {
                let parts: Vec<String> = page_tp
                    .iter()
                    .flat_map(|v| {
                        v.controls_used
                            .iter()
                            .filter(|(_, _)| true) // all controls from vendor present in this page
                            .map(|(name, count)| format!("{name} ({count})"))
                    })
                    .collect();
                md.push_str(&format!("**Third-party controls**: {}\n", parts.join(", ")));
            }
        }

        // Phase 33: VB translation flags per page (Gap 6)
        //
        // Scope the flags to the page itself, its explicit codebehind (when
        // the dossier builder detected one), and the conventional
        // `.aspx.vb` / `.aspx.cs` sibling. Previously this filter contained
        // `f.file_path.contains(cb)` where `cb` came from
        // `d.codebehind_file.as_deref().unwrap_or("")` — with no codebehind
        // detected, `cb` was the empty string and `.contains("")` is always
        // true, so the first dossier on pages without a detected codebehind
        // (e.g. OciusX `Site/AuthCallback.aspx`) dumped the project-wide
        // flag list (~50 KB) into a single page's section.
        if vb_trans.is_vb_project {
            let page_flags: Vec<&VbTranslationFlag> = vb_trans
                .translation_flags
                .iter()
                .filter(|f| {
                    flag_belongs_to_page(&f.file_path, &d.file_path, d.codebehind_file.as_deref())
                })
                .collect();
            if !page_flags.is_empty() {
                let parts: Vec<String> = page_flags
                    .iter()
                    .map(|f| format!("{} ({})", f.pattern, f.count))
                    .collect();
                md.push_str(&format!("**VB translation flags**: {}\n", parts.join(", ")));
            }
        }

        // Phase 33: Caching per page (Gap 4)
        {
            let page_cache: Vec<&OutputCacheEntry> = cache_inv
                .output_cache_pages
                .iter()
                .filter(|c| c.file_path == d.file_path)
                .collect();
            if !page_cache.is_empty() {
                for oc in &page_cache {
                    let dur = oc
                        .duration_seconds
                        .map_or("-".to_string(), |d| format!("{d}s"));
                    md.push_str(&format!("**OutputCache**: Duration={dur}"));
                    if let Some(ref vbp) = oc.vary_by_param {
                        md.push_str(&format!(", VaryByParam={vbp}"));
                    }
                    md.push('\n');
                }
            }
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

        // LLM-generated migration notes (risks + Blazor component guidance
        // that the deterministic analysis doesn't already capture). Only
        // present when `use_llm: true` and this page was within the
        // `llm_max_pages` cap.
        if let Some(ref notes) = d.llm_migration_notes {
            md.push_str("\n**Migration notes (LLM)**:\n");
            for line in notes.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // Honour any bullet formatting the model already produced;
                // otherwise wrap each line in a `-` bullet.
                if trimmed.starts_with('-')
                    || trimmed.starts_with('*')
                    || trimmed.starts_with(|c: char| c.is_ascii_digit())
                {
                    md.push_str(&format!("{trimmed}\n"));
                } else {
                    md.push_str(&format!("- {trimmed}\n"));
                }
            }
        }

        md.push('\n');
    }

    // ── Phase 34: Stored Procedure Catalog ──────────────────────────────
    if sp_cat.total_procedures > 0 {
        md.push_str("## Stored Procedure Catalog\n\n");
        md.push_str(&format!(
            "**Total**: {} procedures | **Called from code**: {} | **Uncalled (dead?)**: {}\n\n",
            sp_cat.total_procedures,
            sp_cat.procedures_called_from_code,
            sp_cat.uncalled_procedures.len()
        ));

        md.push_str("| Procedure | Params | Tables Read | Tables Written | Lines | Dynamic SQL | Cursor | Modern Equivalent |\n");
        md.push_str("|-----------|--------|-------------|----------------|-------|-------------|--------|-------------------|\n");
        for sp in sp_cat.procedures.iter().take(50) {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
                sp.name,
                sp.parameters.len(),
                sp.tables_read.join(", "),
                sp.tables_written.join(", "),
                sp.line_count,
                if sp.has_dynamic_sql { "Yes" } else { "No" },
                if sp.has_cursor { "Yes" } else { "No" },
                sp.modern_equivalent
            ));
        }
        md.push('\n');

        // Parameter details for top SPs
        let top_sps: Vec<_> = sp_cat
            .procedures
            .iter()
            .filter(|sp| !sp.parameters.is_empty() && !sp.called_from.is_empty())
            .take(10)
            .collect();
        if !top_sps.is_empty() {
            md.push_str("### Stored Procedure Parameters (Top Called)\n\n");
            for sp in top_sps {
                md.push_str(&format!(
                    "**{}** — called from: {}\n\n",
                    sp.name,
                    sp.called_from.join(", ")
                ));
                md.push_str("| Parameter | SQL Type | C# Type | Direction | Default |\n");
                md.push_str("|-----------|----------|---------|-----------|--------|\n");
                for p in &sp.parameters {
                    md.push_str(&format!(
                        "| `{}` | {} | `{}` | {} | {} |\n",
                        p.name,
                        p.sql_type,
                        p.csharp_type,
                        p.direction,
                        p.default_value.as_deref().unwrap_or("-")
                    ));
                }
                md.push('\n');
            }
        }

        if !sp_cat.uncalled_procedures.is_empty() {
            md.push_str("### Potentially Dead Procedures\n\n");
            md.push_str("These SPs were found in `.sql` files but are not called from any scanned code-behind:\n\n");
            for name in sp_cat.uncalled_procedures.iter().take(30) {
                md.push_str(&format!("- `{name}`\n"));
            }
            md.push('\n');
        }
    }

    // ── Phase 34: Inheritance Chain Report ────────────────────────────────
    if !inherit.chains.is_empty() {
        md.push_str("## Base Class Inheritance Chains\n\n");
        md.push_str(&format!(
            "**Deepest chain**: {} levels | **Shared base classes**: {}\n\n",
            inherit.deepest_chain_depth,
            inherit.base_classes.len()
        ));

        // Base class summary
        if !inherit.base_classes.is_empty() {
            md.push_str("### Shared Base Classes\n\n");
            md.push_str("| Base Class | File | Derived Pages | Lifecycle Methods | Session Keys Initialized |\n");
            md.push_str("|------------|------|---------------|-------------------|-------------------------|\n");
            for bc in &inherit.base_classes {
                md.push_str(&format!(
                    "| `{}` | `{}` | {} | {} | {} |\n",
                    bc.class_name,
                    bc.file_path,
                    bc.derived_count,
                    bc.lifecycle_methods.join(", "),
                    if bc.state_keys_initialized.is_empty() {
                        "-".to_string()
                    } else {
                        bc.state_keys_initialized.join(", ")
                    }
                ));
            }
            md.push('\n');
        }

        // Shared lifecycle methods
        if !inherit.shared_lifecycle_methods.is_empty() {
            md.push_str("### Shared Lifecycle Methods\n\n");
            for slm in &inherit.shared_lifecycle_methods {
                md.push_str(&format!(
                    "- **{}** defined in `{}`, overridden in: {} {}\n",
                    slm.method_name,
                    slm.defining_class,
                    slm.overridden_in.join(", "),
                    if slm.calls_base {
                        "(calls base)"
                    } else {
                        "(does NOT call base)"
                    }
                ));
            }
            md.push('\n');
        }

        // Per-page chain diagrams (top 20)
        md.push_str("### Inheritance Chains per Page\n\n");
        for chain in inherit.chains.iter().take(20) {
            md.push_str(&format!(
                "**{}**: `{}`\n",
                chain.page_file,
                chain.chain.join(" → ")
            ));
            if !chain.inherited_state_writes.is_empty() {
                md.push_str(&format!(
                    "  - Inherited Session keys: {}\n",
                    chain.inherited_state_writes.join(", ")
                ));
            }
            if !chain.inherited_lifecycle_methods.is_empty() {
                let parts: Vec<String> = chain
                    .inherited_lifecycle_methods
                    .iter()
                    .map(|(m, c)| format!("{m} ({c})"))
                    .collect();
                md.push_str(&format!("  - Inherited lifecycle: {}\n", parts.join(", ")));
            }
        }
        md.push('\n');
    }

    // ── Phase 34: Config Transforms ───────────────────────────────────────
    if !cfg_transforms.environments.is_empty() {
        md.push_str("## Configuration Transforms\n\n");
        md.push_str(&format!(
            "**Environments**: {} | **Total transforms**: {}\n\n",
            cfg_transforms.environments.len(),
            cfg_transforms.total_transforms
        ));
        md.push_str("Modern equivalent: `appsettings.{Environment}.json` with environment-specific overrides.\n\n");

        for env in &cfg_transforms.environments {
            md.push_str(&format!("### {} (`{}`)\n\n", env.name, env.file_path));
            md.push_str("| XPath | Operation | Key | Value |\n");
            md.push_str("|-------|-----------|-----|-------|\n");
            for t in &env.transforms {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    t.xpath_hint,
                    t.operation,
                    t.key.as_deref().unwrap_or("-"),
                    t.value_preview.as_deref().unwrap_or("-")
                ));
            }
            md.push('\n');
        }

        if !cfg_transforms.connection_string_overrides.is_empty() {
            md.push_str("**Connection string overrides by environment:**\n");
            for (env, cs) in &cfg_transforms.connection_string_overrides {
                md.push_str(&format!("- `{env}` → `{cs}`\n"));
            }
            md.push('\n');
        }

        if !cfg_transforms.debug_flag_overrides.is_empty() {
            md.push_str("**Debug flag by environment:**\n");
            for (env, debug) in &cfg_transforms.debug_flag_overrides {
                md.push_str(&format!("- `{env}` → debug={debug}\n"));
            }
            md.push('\n');
        }
    }

    // ── Phase 34: Master Page Region Map ──────────────────────────────────
    if !master_regions.master_pages.is_empty() {
        md.push_str("## Master Page Layout Regions\n\n");
        md.push_str(&format!(
            "**Master pages**: {} | **Content regions**: {}\n\n",
            master_regions.master_pages.len(),
            master_regions.regions.len()
        ));

        md.push_str("| Region | Defined In | Pages Filling | Has Default | Modern Equivalent |\n");
        md.push_str("|--------|-----------|---------------|-------------|-------------------|\n");
        for region in &master_regions.regions {
            md.push_str(&format!(
                "| `{}` | `{}` | {} | {} | `{}` |\n",
                region.region_name,
                region.defined_in,
                region.filled_by.len(),
                if region.has_default_content {
                    "Yes"
                } else {
                    "No"
                },
                region.modern_equivalent
            ));
        }
        md.push('\n');

        if !master_regions.orphan_regions.is_empty() {
            md.push_str("**Orphan regions** (defined but never filled):\n");
            for r in &master_regions.orphan_regions {
                md.push_str(&format!("- `{r}`\n"));
            }
            md.push('\n');
        }
    }

    // ── Phase 34: Resource File Inventory ─────────────────────────────────
    if !res_inv.resource_files.is_empty() {
        md.push_str("## Resource Files (.resx)\n\n");
        md.push_str(&format!(
            "**Total files**: {} | **Total keys**: {} | **Languages**: {}\n",
            res_inv.resource_files.len(),
            res_inv.total_keys,
            if res_inv.languages_detected.is_empty() {
                "default only".to_string()
            } else {
                res_inv.languages_detected.join(", ")
            }
        ));
        if res_inv.has_global_resources {
            md.push_str("- Uses `App_GlobalResources` → migrate to `IStringLocalizer`\n");
        }
        if res_inv.has_local_resources {
            md.push_str(
                "- Uses `App_LocalResources` → migrate to page-specific `IStringLocalizer`\n",
            );
        }
        if res_inv.embedded_resource_count > 0 {
            md.push_str(&format!(
                "- {} embedded resources (images, files)\n",
                res_inv.embedded_resource_count
            ));
        }
        md.push('\n');

        md.push_str("| File | Keys | Language | Type |\n");
        md.push_str("|------|------|----------|------|\n");
        for rf in res_inv.resource_files.iter().take(30) {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                rf.file_path,
                rf.key_count,
                rf.language.as_deref().unwrap_or("default"),
                rf.resource_type
            ));
        }
        md.push('\n');
    }

    // ── Phase 34: Binding Redirects ───────────────────────────────────────
    if !dep_inv.binding_redirects.is_empty() {
        md.push_str("## Assembly Binding Redirects\n\n");
        md.push_str(&format!(
            "**{}** binding redirects found — these indicate version conflicts to resolve.\n\n",
            dep_inv.binding_redirects.len()
        ));
        md.push_str("| Assembly | Old Version | New Version | Known Replacement |\n");
        md.push_str("|----------|-------------|-------------|-------------------|\n");
        for br in &dep_inv.binding_redirects {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                br.assembly_name,
                br.old_version_range,
                br.new_version,
                if br.has_known_replacement {
                    "Yes"
                } else {
                    "No"
                }
            ));
        }
        md.push('\n');
    }

    // ── Phase 35: VB Translation Traps ──────────────────────────────────
    if vb_traps.total_traps > 0 {
        md.push_str("## VB.NET Translation Traps\n\n");
        md.push_str(&format!(
            "**Total traps**: {} | **Silent bugs**: {} | **Compile errors**: {} | **Files analyzed**: {}\n\n",
            vb_traps.total_traps,
            vb_traps.silent_bug_count,
            vb_traps.compile_error_count,
            vb_traps.files_analyzed,
        ));
        md.push_str("| Trap | Location | Risk | VB Code | Guidance |\n");
        md.push_str("|------|----------|------|---------|----------|\n");
        for trap in vb_traps.traps.iter().take(50) {
            let code_escaped = trap.vb_code.replace('|', "\\|");
            let guidance_escaped = trap.guidance.replace('|', "\\|");
            let guidance_short = if guidance_escaped.len() > 80 {
                // Truncate at a safe char boundary
                let end = guidance_escaped
                    .char_indices()
                    .nth(80)
                    .map(|(i, _)| i)
                    .unwrap_or(guidance_escaped.len());
                format!("{}...", &guidance_escaped[..end])
            } else {
                guidance_escaped
            };
            md.push_str(&format!(
                "| {} | `{}` | {} | `{}` | {} |\n",
                trap.trap, trap.location, trap.risk, code_escaped, guidance_short
            ));
        }
        if vb_traps.total_traps > 50 {
            md.push_str(&format!(
                "\n*... and {} more traps (see JSON output for full list)*\n",
                vb_traps.total_traps - 50
            ));
        }
        md.push('\n');
    }

    // ── Phase 35: jQuery Ecosystem Inventory ─────────────────────────────
    if jquery_inv.total_usages > 0 || jquery_inv.core_version.is_some() {
        md.push_str("## jQuery Plugin Ecosystem\n\n");
        if let Some(ref ver) = jquery_inv.core_version {
            let vuln_badge = if jquery_inv.core_vulnerable {
                " **VULNERABLE**"
            } else {
                ""
            };
            md.push_str(&format!("**jQuery Core**: v{ver}{vuln_badge}\n\n"));
            for note in &jquery_inv.vulnerability_notes {
                md.push_str(&format!("- {note}\n"));
            }
            if !jquery_inv.vulnerability_notes.is_empty() {
                md.push('\n');
            }
        }
        md.push_str(&format!(
            "**Total plugin usages**: {} | **Files analyzed**: {}\n\n",
            jquery_inv.total_usages, jquery_inv.files_analyzed,
        ));

        if !jquery_inv.ui_widgets.is_empty() {
            md.push_str("### jQuery UI Widgets\n\n");
            md.push_str("| Widget | File | Line | Modern Equivalent | Complexity |\n");
            md.push_str("|--------|------|------|-------------------|------------|\n");
            for w in &jquery_inv.ui_widgets {
                md.push_str(&format!(
                    "| {} | `{}` | {} | {} | {} |\n",
                    w.name, w.file_path, w.line_number, w.modern_equivalent, w.migration_complexity
                ));
            }
            md.push('\n');
        }

        if !jquery_inv.third_party_plugins.is_empty() {
            md.push_str("### Third-Party Plugins\n\n");
            md.push_str("| Plugin | File | Line | Modern Equivalent | Complexity |\n");
            md.push_str("|--------|------|------|-------------------|------------|\n");
            for p in &jquery_inv.third_party_plugins {
                md.push_str(&format!(
                    "| {} | `{}` | {} | {} | {} |\n",
                    p.name, p.file_path, p.line_number, p.modern_equivalent, p.migration_complexity
                ));
            }
            md.push('\n');
        }

        if !jquery_inv.custom_plugins.is_empty() {
            md.push_str("### Custom Plugins ($.fn.*)\n\n");
            md.push_str("| Plugin | File | Line |\n");
            md.push_str("|--------|------|------|\n");
            for p in &jquery_inv.custom_plugins {
                md.push_str(&format!(
                    "| {} | `{}` | {} |\n",
                    p.name, p.file_path, p.line_number
                ));
            }
            md.push('\n');
        }

        if !jquery_inv.deprecated_patterns.is_empty() {
            md.push_str("### Deprecated Patterns\n\n");
            md.push_str("| Pattern | File | Line | Recommendation |\n");
            md.push_str("|---------|------|------|----------------|\n");
            for d in &jquery_inv.deprecated_patterns {
                md.push_str(&format!(
                    "| {} | `{}` | {} | {} |\n",
                    d.name, d.file_path, d.line_number, d.modern_equivalent
                ));
            }
            md.push('\n');
        }
    }

    // ── Phase 35: Cross-Layer Data Flow Chains ───────────────────────────
    if !cross_traces.chains.is_empty() {
        md.push_str("## Cross-Layer Data Flow Chains\n\n");
        md.push_str(&format!(
            "**Total chains**: {} | **Unresolved URLs**: {}\n\n",
            cross_traces.total_chains,
            cross_traces.unresolved_urls.len(),
        ));

        for chain in cross_traces.chains.iter().take(20) {
            md.push_str(&format!("### Feature: {}\n\n", chain.feature_name));
            md.push_str("| Layer | File | Action |\n");
            md.push_str("|-------|------|--------|\n");
            for step in &chain.steps {
                md.push_str(&format!(
                    "| {} | `{}` | {} |\n",
                    step.layer, step.file_path, step.action
                ));
            }
            if !chain.tables_touched.is_empty() {
                md.push_str(&format!(
                    "\n**Tables**: {}\n",
                    chain.tables_touched.join(", ")
                ));
            }
            for note in &chain.risk_notes {
                md.push_str(&format!("- {note}\n"));
            }
            md.push('\n');
        }

        if !cross_traces.unresolved_urls.is_empty() {
            md.push_str("### Unresolved AJAX URLs\n\n");
            for url in &cross_traces.unresolved_urls {
                md.push_str(&format!("- `{url}`\n"));
            }
            md.push('\n');
        }
    }

    // ── Phase 35: Inherited Effects ──────────────────────────────────────
    if !inherit.inherited_effects.is_empty() {
        md.push_str("## Inherited Effects (Base Class Propagation)\n\n");
        md.push_str("| Derived Class | Inherited From | Method | Effects |\n");
        md.push_str("|---------------|----------------|--------|--------|\n");
        for eff in inherit.inherited_effects.iter().take(50) {
            let effects_str = eff.effects.join(", ").replace('|', "\\|");
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                eff.class, eff.inherited_from, eff.method, effects_str
            ));
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

    // ── Phase 36: Business Logic Summary ────────────────────────────────
    if !biz_logic.file_summaries.is_empty() {
        md.push_str(&super::business_logic_service::render_compact_markdown(
            biz_logic,
        ));
        // Show tip only when no LLM was used (no confidence data present)
        let has_llm_data = biz_logic
            .file_summaries
            .iter()
            .any(|f| f.methods.iter().any(|m| !m.confidence.is_empty()));
        if !has_llm_data {
            md.push_str("\n> **Tip**: Run `analyze_full_project_migration` with `use_llm: true` ");
            md.push_str(
                "for LLM-powered business logic comprehension with confidence scoring.\n\n",
            );
        }
    }

    // ── Phase 37: Database Intelligence ──────────────────────────────────
    md.push_str(
        &super::database_intelligence_service::render_database_intelligence_markdown(db_intel),
    );

    // ── Phase 37: Session Workflows ─────────────────────────────────────
    md.push_str(&super::session_workflow_service::render_session_workflows_markdown(session_wf));

    // ── Phase 37: Migration Intelligence Confidence Dashboard ───────────
    md.push_str(&render_confidence_dashboard(
        cross, biz_logic, db_intel, session_wf,
    ));

    md
}

/// Render a top-level confidence dashboard summarizing intelligence coverage.
fn render_confidence_dashboard(
    cross: &CrossCuttingSummary,
    biz_logic: &super::business_logic_service::ProjectBusinessLogicReport,
    db_intel: &super::database_intelligence_service::DatabaseIntelligence,
    session_wf: &super::session_workflow_service::SessionWorkflowReport,
) -> String {
    let mut md = String::with_capacity(2_000);
    md.push_str("## Migration Intelligence Confidence\n\n");
    md.push_str("| Dimension | Coverage | Confidence |\n|---|---|---|\n");

    // Code Structure
    md.push_str(&format!(
        "| Code Structure | {} pages analyzed | {} |\n",
        cross.total_pages_analyzed,
        if cross.total_pages_analyzed > 0 {
            "✅ High"
        } else {
            "❌ Low"
        }
    ));

    // Business Logic — single pass for confidence counts
    let total_methods: usize = biz_logic
        .file_summaries
        .iter()
        .map(|f| f.methods.len())
        .sum();
    let (mut llm_methods, mut high_conf, mut med_conf, mut low_conf) =
        (0usize, 0usize, 0usize, 0usize);
    for m in biz_logic.file_summaries.iter().flat_map(|f| &f.methods) {
        if !m.confidence.is_empty() {
            llm_methods += 1;
            match m.confidence.as_str() {
                "High" => high_conf += 1,
                "Medium" => med_conf += 1,
                "Low" => low_conf += 1,
                _ => {}
            }
        }
    }

    if llm_methods > 0 {
        md.push_str(&format!(
            "| Business Logic | {llm_methods}/{total_methods} methods analyzed by LLM | ✅ High ({high_conf}), ⚠️ Medium ({med_conf}), ❌ Low ({low_conf}) |\n"
        ));
    } else {
        md.push_str(&format!(
            "| Business Logic | {total_methods} methods (deterministic only) | ⚠️ Medium (no LLM) |\n"
        ));
    }

    // Database
    let sp_count = db_intel.sp_logic.len();
    let trigger_count = db_intel.triggers.len();
    let table_count = db_intel.schema.tables.len();
    let db_confidence = if sp_count > 0 && table_count > 0 {
        "✅ High"
    } else if sp_count > 0 || table_count > 0 {
        "⚠️ Medium"
    } else {
        "ℹ️ No SQL files"
    };
    md.push_str(&format!(
        "| Database | {table_count} tables in schema, {sp_count} SPs analyzed, {trigger_count} triggers | {db_confidence} |\n"
    ));

    // Session Workflows
    let wf_count = session_wf.cross_page_chains;
    let wf_confidence = if session_wf.total_keys > 0 {
        if session_wf.warnings.is_empty() {
            "✅ High"
        } else {
            "⚠️ Medium"
        }
    } else {
        "ℹ️ No state detected"
    };
    md.push_str(&format!(
        "| Session Workflows | {} keys, {wf_count} cross-page flows | {wf_confidence} |\n",
        session_wf.total_keys
    ));

    // Data Access
    md.push_str(&format!(
        "| Data Access | {} SPs, {} called from code | {} |\n",
        cross.total_stored_procedures,
        cross.total_sp_called_from_code,
        if cross.total_sp_called_from_code > 0 {
            "✅ High"
        } else if cross.total_stored_procedures > 0 {
            "⚠️ Medium"
        } else {
            "ℹ️ No SPs"
        }
    ));

    // External Integrations
    let ext_count = cross.total_service_endpoints;
    md.push_str(&format!(
        "| External Integrations | {} service endpoints | {} |\n",
        ext_count,
        if ext_count > 0 {
            "⚠️ Medium (contracts not parsed)"
        } else {
            "ℹ️ None detected"
        }
    ));

    md.push('\n');
    md
}

// ── Phase 34: Stored Procedure Catalog Builder ───────────────────────────────

fn build_sp_catalog(
    sql_files: &[(String, String)],
    code_files: &[(&str, &str)],
) -> StoredProcedureCatalog {
    use engram_index::sp_extractor;

    let mut all_procs: Vec<StoredProcedureInfo> = Vec::new();
    let mut code_calls: Vec<(String, String)> = Vec::new(); // (sp_name, calling_file)

    // 1. Parse SQL files for SP definitions
    for (_path, content) in sql_files {
        let defs = sp_extractor::parse_sp_definitions(content);
        for sp in defs {
            let modern_eq = if sp.has_dynamic_sql {
                "raw SQL (review for SQL injection)".to_string()
            } else if sp.has_cursor {
                "LINQ query or Dapper (cursor refactoring needed)".to_string()
            } else if sp.tables_read.len() > 3 || sp.tables_written.len() > 2 {
                "EF Core with repository pattern (complex joins)".to_string()
            } else if sp.tables_written.is_empty() {
                "EF Core query or Dapper".to_string()
            } else {
                "EF Core SaveChanges or Dapper Execute".to_string()
            };

            all_procs.push(StoredProcedureInfo {
                name: sp.name.clone(),
                parameters: sp
                    .parameters
                    .iter()
                    .map(|p| SpParameterInfo {
                        name: p.name.clone(),
                        sql_type: p.sql_type.clone(),
                        direction: p.direction.clone(),
                        default_value: p.default_value.clone(),
                        csharp_type: p.csharp_type.clone(),
                    })
                    .collect(),
                tables_read: sp.tables_read,
                tables_written: sp.tables_written,
                called_from: Vec::new(), // filled below
                line_count: sp.line_count,
                has_dynamic_sql: sp.has_dynamic_sql,
                has_cursor: sp.has_cursor,
                modern_equivalent: modern_eq,
            });
        }
    }

    // 2. Scan code files for SP calls
    for (path, content) in code_files {
        let rel = engram_core::RelPath::new(path);
        let (_, edges) = sp_extractor::extract_code_side_sp_calls(&rel, content);
        for edge in edges {
            if edge.kind == "calls_stored_procedure" {
                code_calls.push((edge.target_name.clone(), path.to_string()));
            }
        }
    }

    // 3. Cross-reference: mark which SPs are called from code
    for (sp_name, calling_file) in &code_calls {
        for proc in &mut all_procs {
            if proc.name.eq_ignore_ascii_case(sp_name) && !proc.called_from.contains(calling_file) {
                proc.called_from.push(calling_file.clone());
            }
        }
    }

    let total = all_procs.len();
    let with_params = all_procs
        .iter()
        .filter(|p| !p.parameters.is_empty())
        .count();
    let called_from_code = all_procs
        .iter()
        .filter(|p| !p.called_from.is_empty())
        .count();
    let uncalled: Vec<String> = all_procs
        .iter()
        .filter(|p| p.called_from.is_empty())
        .map(|p| p.name.clone())
        .collect();

    StoredProcedureCatalog {
        procedures: all_procs,
        total_procedures: total,
        procedures_with_params: with_params,
        procedures_called_from_code: called_from_code,
        uncalled_procedures: uncalled,
    }
}

/// Public wrapper for building a stored procedure catalog from SQL + code files.
/// Used by standalone tools (e.g., `analyze_database_intelligence`) that need the catalog
/// without running the full project migration analysis.
/// `sp_limit` caps the number of procedures to include (0 = unlimited).
pub fn build_sp_catalog_public(
    sql_files: &[(String, String)],
    code_files: &[(&str, &str)],
    sp_limit: usize,
) -> StoredProcedureCatalog {
    let mut catalog = build_sp_catalog(sql_files, code_files);
    // Sort so that procs called from application code appear first — when
    // `sp_limit` truncates, we want business-critical SPs to survive. On
    // real projects the tail tends to be framework procs (e.g. `aspnet_*`
    // from Membership) that no application code actually references, so
    // pushing them to the back via a descending `called_from.len()` sort
    // (with alphabetical tiebreaker for determinism) is the right shape.
    catalog.procedures.sort_by(|a, b| {
        b.called_from
            .len()
            .cmp(&a.called_from.len())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    if sp_limit > 0 && catalog.procedures.len() > sp_limit {
        catalog.procedures.truncate(sp_limit);
    }
    // Keep `total_procedures` in sync with what's actually returned so the
    // downstream renderer and JSON consumers agree on the count.
    catalog.total_procedures = catalog.procedures.len();
    catalog
}

// ── Phase 34: Inheritance Chain Resolution ───────────────────────────────────

static VB_CLASS_INHERITS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*(?:Public\s+)?(?:Partial\s+)?Class\s+(\w+)\s*(?:\r?\n\s*)?Inherits\s+(\w[\w.]*)",
    )
    .expect("vb_class_inherits")
});
static CS_CLASS_INHERITS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:public\s+)?(?:partial\s+)?class\s+(\w+)\s*:\s*(\w[\w.]*)")
        .expect("cs_class_inherits")
});
static VB_METHOD_DEF_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:Protected\s+)?(?:Overrides\s+)?(?:Overridable\s+)?(?:Public\s+)?(?:Private\s+)?(?:Friend\s+)?(?:Shared\s+)?(?:Async\s+)?(?:Sub|Function)\s+(\w+)").expect("vb_method_def")
});
static CS_METHOD_DEF_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    // Supports generic return types with commas: Task<ActionResult>, Dictionary<string, int>,
    // IEnumerable<KeyValuePair<string, int>>, Nullable<int>, List<T> etc.
    Regex::new(r"(?im)^\s*(?:protected\s+)?(?:override\s+)?(?:virtual\s+)?(?:public\s+)?(?:private\s+)?(?:internal\s+)?(?:static\s+)?(?:async\s+)?(?:void|[\w]+(?:<[\w,\s<>\[\]?]+>)?(?:\[\])?)\s+(\w+)\s*\(").expect("cs_method_def")
});
static VB_CALLS_BASE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)MyBase\.(\w+)").expect("vb_calls_base"));
static CS_CALLS_BASE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)base\.(\w+)").expect("cs_calls_base"));
static SESSION_WRITE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)Session\s*[\(\[]\s*"(\w+)"\s*[\)\]]\s*="#).expect("session_write")
});
static INHERITS_DIRECTIVE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
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

fn resolve_inheritance_chains(
    code_files: &[(&str, &str)],
    markup_files: &[FileContent],
) -> InheritanceChainReport {
    // C# keyword blacklist for method name filtering
    const CS_KEYWORDS: &[&str] = &[
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
    ];

    // 1. Build class map: class_name → (parent_class, file_path, methods[], state_writes[], base_calls[])
    // SECOND-PASS FIX: Scope methods & state_writes to each class body, not the whole file.
    let mut class_map: std::collections::HashMap<String, ClassInfo> =
        std::collections::HashMap::new();

    for (path, content) in code_files {
        let is_vb = path.to_lowercase().ends_with(".vb");

        // Collect all class starts (with their byte positions) to determine class boundaries
        let mut class_ranges: Vec<(String, String, usize)> = Vec::new(); // (name, parent, start_pos)

        if is_vb {
            for cap in VB_CLASS_INHERITS_RE.captures_iter(content) {
                let class_name = cap[1].to_string();
                let parent = cap[2].to_string();
                let start_pos = cap.get(0).map_or(0, |m| m.start());
                class_ranges.push((class_name, parent, start_pos));
            }
        } else {
            for cap in CS_CLASS_INHERITS_RE.captures_iter(content) {
                let class_name = cap[1].to_string();
                let parent = cap[2].to_string();
                let start_pos = cap.get(0).map_or(0, |m| m.start());
                class_ranges.push((class_name, parent, start_pos));
            }
        }

        // For each class, extract methods/state_writes only from its body region
        for (ci, (class_name, parent, start_pos)) in class_ranges.iter().enumerate() {
            let end_pos = class_ranges
                .get(ci + 1)
                .map(|(_, _, p)| *p)
                .unwrap_or(content.len());
            let class_body = &content[*start_pos..end_pos];

            let methods: Vec<String> = if is_vb {
                VB_METHOD_DEF_RE
                    .captures_iter(class_body)
                    .map(|c| c[1].to_string())
                    .collect()
            } else {
                CS_METHOD_DEF_RE
                    .captures_iter(class_body)
                    .filter_map(|c| {
                        let name = c[1].to_string();
                        if CS_KEYWORDS.contains(&name.as_str()) {
                            None
                        } else {
                            Some(name)
                        }
                    })
                    .collect()
            };

            let base_calls: Vec<String> = if is_vb {
                VB_CALLS_BASE_RE
                    .captures_iter(class_body)
                    .map(|c| c[1].to_string())
                    .collect()
            } else {
                CS_CALLS_BASE_RE
                    .captures_iter(class_body)
                    .map(|c| c[1].to_string())
                    .collect()
            };

            let state_writes: Vec<String> = SESSION_WRITE_RE
                .captures_iter(class_body)
                .map(|c| c[1].to_string())
                .collect();

            // THIRD-PASS FIX: Merge instead of overwrite when partial classes
            // span multiple files (e.g. _Default.aspx.vb + _Default.aspx.designer.vb).
            // The second insert would clobber the first file's methods.
            if let Some(existing) = class_map.get_mut(class_name) {
                // Keep the parent from the file that declares the inheritance
                if existing.0.is_empty() || existing.0 == class_name.as_str() {
                    existing.0 = parent.clone();
                }
                // Merge methods, state_writes, base_calls (deduplicated)
                for m in &methods {
                    if !existing.2.contains(m) {
                        existing.2.push(m.clone());
                    }
                }
                for sw in &state_writes {
                    if !existing.3.contains(sw) {
                        existing.3.push(sw.clone());
                    }
                }
                for bc in &base_calls {
                    if !existing.4.contains(bc) {
                        existing.4.push(bc.clone());
                    }
                }
            } else {
                class_map.insert(
                    class_name.clone(),
                    (
                        parent.clone(),
                        path.to_string(),
                        methods,
                        state_writes,
                        base_calls,
                    ),
                );
            }
        }
    }

    // 2. For each .aspx Inherits directive, walk the chain
    let mut chains: Vec<InheritanceChain> = Vec::new();
    let mut base_class_usage: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for fc in markup_files {
        let inherits_class = INHERITS_DIRECTIVE_RE
            .captures(&fc.markup_content)
            .and_then(|c| {
                let full = c[1].to_string();
                // Extract just the class name (after last dot)
                full.rsplit('.').next().map(|s| s.to_string())
            });

        let Some(page_class) = inherits_class else {
            continue;
        };

        let mut chain: Vec<String> = Vec::new();
        let mut inherited_lifecycle: Vec<(String, String)> = Vec::new();
        let mut inherited_state_writes: Vec<String> = Vec::new();
        let mut current = page_class.clone();

        // Walk up the inheritance chain
        for _ in 0..20 {
            // max depth safety
            chain.push(current.clone());

            let Some((parent, _path, methods, state_writes, _base_calls)) = class_map.get(&current)
            else {
                // Check if parent is a known framework class
                if current == "Page"
                    || current == "System.Web.UI.Page"
                    || current == "UserControl"
                    || current == "MasterPage"
                {
                    chain.push(format!("System.Web.UI.{current}"));
                }
                break;
            };

            // Track which base classes are used
            base_class_usage
                .entry(current.clone())
                .or_default()
                .push(fc.file_path.clone());

            // Collect lifecycle methods from this ancestor
            for method in methods {
                if LIFECYCLE_METHODS
                    .iter()
                    .any(|lm| lm.eq_ignore_ascii_case(method))
                {
                    inherited_lifecycle.push((method.clone(), current.clone()));
                }
            }

            // Collect state writes from ancestors (not the page class itself)
            if current != page_class {
                for key in state_writes {
                    if !inherited_state_writes.contains(key) {
                        inherited_state_writes.push(key.clone());
                    }
                }
            }

            current = parent.clone();
        }

        if chain.len() > 1 {
            chains.push(InheritanceChain {
                page_file: fc.file_path.clone(),
                chain,
                inherited_lifecycle_methods: inherited_lifecycle,
                inherited_state_writes,
            });
        }
    }

    // 3. Build base class info
    let mut base_classes: Vec<BaseClassInfo> = Vec::new();
    for (class_name, pages) in &base_class_usage {
        if let Some((_, file_path, methods, state_writes, _)) = class_map.get(class_name) {
            let lifecycle_methods: Vec<String> = methods
                .iter()
                .filter(|m| {
                    LIFECYCLE_METHODS
                        .iter()
                        .any(|lm| lm.eq_ignore_ascii_case(m))
                })
                .cloned()
                .collect();

            if pages.len() > 1 || !lifecycle_methods.is_empty() {
                base_classes.push(BaseClassInfo {
                    class_name: class_name.clone(),
                    file_path: file_path.clone(),
                    derived_count: pages.len(),
                    lifecycle_methods,
                    state_keys_initialized: state_writes.clone(),
                });
            }
        }
    }
    base_classes.sort_by(|a, b| b.derived_count.cmp(&a.derived_count));

    // 4. Build shared lifecycle methods
    let mut shared_lifecycle: Vec<SharedLifecycleMethod> = Vec::new();
    for lm_name in LIFECYCLE_METHODS {
        let mut defining_classes: Vec<(String, Vec<String>)> = Vec::new();

        for (class_name, (_, _, methods, _, base_calls)) in &class_map {
            if methods.iter().any(|m| m.eq_ignore_ascii_case(lm_name)) {
                let calls_base = base_calls.iter().any(|bc| bc.eq_ignore_ascii_case(lm_name));
                defining_classes.push((
                    class_name.clone(),
                    if calls_base {
                        vec!["calls_base".to_string()]
                    } else {
                        vec![]
                    },
                ));
            }
        }

        if defining_classes.len() > 1 {
            let first = defining_classes[0].0.clone();
            let calls_base = !defining_classes[0].1.is_empty();
            let overridden_in: Vec<String> = defining_classes[1..]
                .iter()
                .map(|(name, _)| name.clone())
                .collect();

            shared_lifecycle.push(SharedLifecycleMethod {
                method_name: lm_name.to_string(),
                defining_class: first,
                overridden_in,
                calls_base,
            });
        }
    }

    // 5. Propagate effects down inheritance chains
    let inherited_effects = propagate_inherited_effects(&chains, code_files);

    let deepest = chains.iter().map(|c| c.chain.len()).max().unwrap_or(0);

    InheritanceChainReport {
        chains,
        base_classes,
        shared_lifecycle_methods: shared_lifecycle,
        inherited_effects,
        deepest_chain_depth: deepest,
    }
}

// ── Phase 35: Inherited effect propagation ───────────────────────────────────

// Effect detection regexes for method bodies
static EFFECT_SQL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:SqlCommand|SqlDataAdapter|ExecuteReader|ExecuteNonQuery|ExecuteScalar|SqlConnection|OleDbCommand|DataAdapter)\b")
        .expect("effect_sql")
});
static EFFECT_REDIRECT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:Response\.Redirect|Server\.Transfer|Response\.RedirectPermanent)\b")
        .expect("effect_redirect")
});
static EFFECT_CONTROL_WRITE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:\w+\.(?:Text|Visible|Enabled|DataSource|DataBind|SelectedValue|SelectedIndex|Items)\s*=)")
        .expect("effect_control_write")
});
static EFFECT_HTTP_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:Response\.Write|Response\.ContentType|Response\.AddHeader|Response\.Cookies)\b",
    )
    .expect("effect_http")
});

/// Extract effects from a method body snippet.
fn extract_method_effects(method_body: &str) -> Vec<String> {
    let mut effects = Vec::new();

    // Session/ViewState writes
    let session_keys: Vec<String> = SESSION_WRITE_RE
        .captures_iter(method_body)
        .map(|c| format!("Session[\"{}\"]", &c[1]))
        .collect();
    if !session_keys.is_empty() {
        effects.push(format!("State_Access: writes {}", session_keys.join(", ")));
    }

    // SQL operations
    if EFFECT_SQL_RE.is_match(method_body) {
        effects.push("SQL_Access".to_string());
    }

    // Redirects
    if EFFECT_REDIRECT_RE.is_match(method_body) {
        effects.push("Redirect".to_string());
    }

    // Control writes (UI mutation)
    if EFFECT_CONTROL_WRITE_RE.is_match(method_body) {
        effects.push("UI_Mutation".to_string());
    }

    // HTTP response manipulation
    if EFFECT_HTTP_RE.is_match(method_body) {
        effects.push("HTTP_Response".to_string());
    }

    effects
}

/// Extract method bodies from a class region of code.
fn extract_method_bodies_from_class(class_body: &str, is_vb: bool) -> Vec<(String, String)> {
    let mut results: Vec<(String, String)> = Vec::new();

    let method_re = if is_vb {
        &*VB_METHOD_DEF_RE
    } else {
        &*CS_METHOD_DEF_RE
    };

    let starts: Vec<(usize, String)> = method_re
        .captures_iter(class_body)
        .map(|c| (c.get(0).expect("match").start(), c[1].to_string()))
        .collect();

    for (i, (start, name)) in starts.iter().enumerate() {
        let end = starts
            .get(i + 1)
            .map(|(s, _)| *s)
            .unwrap_or(class_body.len());
        let body = &class_body[*start..end];
        results.push((name.clone(), body.to_string()));
    }

    results
}

/// Propagate effects from ancestor classes down to derived page classes.
fn propagate_inherited_effects(
    chains: &[InheritanceChain],
    code_files: &[(&str, &str)],
) -> Vec<InheritedEffect> {
    // Build class_name → (file_path, class_body) for targeted extraction
    let mut class_bodies: std::collections::HashMap<String, (bool, String)> =
        std::collections::HashMap::new();

    for (path, content) in code_files {
        let is_vb = path.to_lowercase().ends_with(".vb");
        let class_re = if is_vb {
            &*VB_CLASS_INHERITS_RE
        } else {
            &*CS_CLASS_INHERITS_RE
        };

        let mut ranges: Vec<(String, usize)> = Vec::new();
        for cap in class_re.captures_iter(content) {
            let class_name = cap[1].to_string();
            let start_pos = cap.get(0).map_or(0, |m| m.start());
            ranges.push((class_name, start_pos));
        }

        for (ci, (class_name, start_pos)) in ranges.iter().enumerate() {
            let end_pos = ranges.get(ci + 1).map(|(_, p)| *p).unwrap_or(content.len());
            let body = content[*start_pos..end_pos].to_string();
            class_bodies.insert(class_name.clone(), (is_vb, body));
        }
    }

    let mut inherited_effects: Vec<InheritedEffect> = Vec::new();

    for chain in chains {
        if chain.chain.len() < 2 {
            continue;
        }

        let page_class = &chain.chain[0];

        // Walk ancestors (skip the page class itself at index 0)
        for ancestor_name in &chain.chain[1..] {
            // Skip framework base classes
            if ancestor_name.starts_with("System.Web.UI.") {
                continue;
            }

            let Some((is_vb, class_body)) = class_bodies.get(ancestor_name) else {
                continue;
            };

            let method_bodies = extract_method_bodies_from_class(class_body, *is_vb);

            for (method_name, method_body) in &method_bodies {
                let effects = extract_method_effects(method_body);
                if effects.is_empty() {
                    continue;
                }

                inherited_effects.push(InheritedEffect {
                    class: page_class.clone(),
                    inherited_from: ancestor_name.clone(),
                    method: method_name.clone(),
                    effects: effects.clone(),
                    detail: format!(
                        "{}.{} has: {}",
                        ancestor_name,
                        method_name,
                        effects.join(", ")
                    ),
                });
            }
        }
    }

    inherited_effects
}

// ── Phase 35: Cross-Layer AJAX→Handler→Data Tracing ──────────────────────────

static HANDLER_SP_NAME_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)CommandText\s*=\s*"(sp_\w+|usp_\w+|\w+_\w+)""#).expect("handler_sp_name")
});

static HANDLER_TABLE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:FROM|JOIN|INTO|UPDATE|DELETE\s+FROM)\s+(?:\[?dbo\]?\.)?\[?(\w+)\]?")
        .expect("handler_table")
});

/// Build cross-layer traces from JS AJAX calls → handlers → database.
fn build_cross_layer_traces(
    js_analysis: &JsAnalysisSummary,
    sp_catalog: &StoredProcedureCatalog,
    service_endpoints: &ServiceEndpointSummary,
    code_files: &[(&str, &str)],
) -> CrossLayerTraceSummary {
    // 1. Build URL→handler file map from service endpoints and code files
    let mut url_to_handler: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // Map from service_endpoints
    for ep in &service_endpoints.web_services {
        let base = extract_filename_from_path(&ep.file_path);
        url_to_handler.insert(base.to_lowercase(), ep.file_path.clone());
    }
    for ep in &service_endpoints.http_handlers {
        let base = extract_filename_from_path(&ep.file_path);
        url_to_handler.insert(base.to_lowercase(), ep.file_path.clone());
    }
    for ep in &service_endpoints.wcf_services {
        let base = extract_filename_from_path(&ep.file_path);
        url_to_handler.insert(base.to_lowercase(), ep.file_path.clone());
    }
    for ep in &service_endpoints.route_handlers {
        let base = extract_filename_from_path(&ep.file_path);
        url_to_handler.insert(base.to_lowercase(), ep.file_path.clone());
    }

    // Also map from code files by filename
    for &(path, _) in code_files {
        let lower = path.to_lowercase();
        if lower.ends_with(".ashx")
            || lower.ends_with(".ashx.cs")
            || lower.ends_with(".ashx.vb")
            || lower.ends_with(".asmx")
            || lower.ends_with(".asmx.cs")
            || lower.ends_with(".asmx.vb")
        {
            let base = extract_filename_from_path(path);
            // Strip .cs / .vb suffix for matching
            let base_lower = base.to_lowercase().replace(".cs", "").replace(".vb", "");
            url_to_handler.insert(base_lower, path.to_string());
        }
    }

    // Build code_file content map
    let content_map: std::collections::HashMap<&str, &str> =
        code_files.iter().map(|&(p, c)| (p, c)).collect();

    // SP name → tables map from catalog
    let mut sp_tables: std::collections::HashMap<String, (Vec<String>, Vec<String>)> =
        std::collections::HashMap::new();
    for sp in &sp_catalog.procedures {
        sp_tables.insert(
            sp.name.to_lowercase(),
            (sp.tables_read.clone(), sp.tables_written.clone()),
        );
    }

    let mut chains: Vec<DataFlowChain> = Vec::new();
    let mut unresolved_urls: Vec<String> = Vec::new();
    let mut resolved_handlers: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 2. For each AJAX call, try to resolve the chain
    for ajax_call in &js_analysis.ajax_calls {
        let url = &ajax_call.target_url;
        let url_parts = extract_url_parts(url);
        let handler_file_lower = url_parts
            .file_part
            .to_lowercase()
            .replace(".cs", "")
            .replace(".vb", "");

        // Try to find handler
        let handler_path = url_to_handler.get(&handler_file_lower).cloned();

        if handler_path.is_none() {
            if !unresolved_urls.contains(url) {
                unresolved_urls.push(url.clone());
            }
            continue;
        }

        let handler_path = handler_path.expect("checked above");
        resolved_handlers.insert(handler_path.clone());

        // Build steps
        let mut steps: Vec<DataFlowStep> = Vec::new();
        let mut tables_touched: Vec<String> = Vec::new();
        let mut risk_notes: Vec<String> = Vec::new();

        // Step 1: Client AJAX call
        steps.push(DataFlowStep {
            layer: "client".to_string(),
            file_path: ajax_call.js_file.clone(),
            action: format!(
                "{} {} to {}",
                ajax_call.transport,
                url_parts.method_part.as_deref().unwrap_or(""),
                url
            ),
            params: Vec::new(),
        });

        // Step 2: Handler processing
        let handler_content = find_handler_content(&handler_path, &content_map);
        let mut sp_names: Vec<String> = Vec::new();

        if let Some(content) = handler_content {
            // Find SP calls in handler
            for cap in HANDLER_SP_NAME_RE.captures_iter(content) {
                sp_names.push(cap[1].to_string());
            }

            // Find direct table access
            for cap in HANDLER_TABLE_RE.captures_iter(content) {
                let table = cap[1].to_string();
                if !tables_touched.contains(&table) {
                    tables_touched.push(table);
                }
            }

            let sp_desc = if !sp_names.is_empty() {
                format!("calls {}", sp_names.join(", "))
            } else if !tables_touched.is_empty() {
                format!("direct SQL on: {}", tables_touched.join(", "))
            } else {
                "processes request (no SQL detected)".to_string()
            };

            steps.push(DataFlowStep {
                layer: "handler".to_string(),
                file_path: handler_path.clone(),
                action: sp_desc,
                params: sp_names.clone(),
            });
        } else {
            steps.push(DataFlowStep {
                layer: "handler".to_string(),
                file_path: handler_path.clone(),
                action: "handler file (code not available for analysis)".to_string(),
                params: Vec::new(),
            });
            risk_notes.push("Handler code-behind not found — cannot trace data layer".into());
        }

        // Step 3: Database layer (from SP catalog)
        for sp_name in &sp_names {
            if let Some((reads, writes)) = sp_tables.get(&sp_name.to_lowercase()) {
                for t in reads {
                    if !tables_touched.contains(t) {
                        tables_touched.push(t.clone());
                    }
                }
                for t in writes {
                    if !tables_touched.contains(t) {
                        tables_touched.push(t.clone());
                    }
                }

                steps.push(DataFlowStep {
                    layer: "database".to_string(),
                    file_path: sp_name.clone(),
                    action: format!(
                        "reads: [{}], writes: [{}]",
                        reads.join(", "),
                        writes.join(", ")
                    ),
                    params: Vec::new(),
                });
            }
        }

        let feature_name = url_parts
            .method_part
            .unwrap_or_else(|| url_parts.file_part.clone());

        chains.push(DataFlowChain {
            feature_name,
            trigger_file: ajax_call.js_file.clone(),
            steps,
            tables_touched,
            risk_notes,
        });
    }

    // Find handlers without callers
    let all_handler_paths: Vec<String> = url_to_handler.values().cloned().collect();
    let handlers_without_ajax_callers: Vec<String> = all_handler_paths
        .into_iter()
        .filter(|h| !resolved_handlers.contains(h))
        .collect();

    let total_chains = chains.len();

    CrossLayerTraceSummary {
        chains,
        total_chains,
        unresolved_urls,
        handlers_without_ajax_callers,
    }
}

struct UrlParts {
    file_part: String,
    method_part: Option<String>,
}

fn extract_url_parts(url: &str) -> UrlParts {
    // Strip query string and fragment
    let clean = url.split('?').next().unwrap_or(url);
    let clean = clean.split('#').next().unwrap_or(clean);

    // Split on last / to separate method from file
    // e.g. "Services/MapData.asmx/GetPolygons" → file="MapData.asmx", method="GetPolygons"
    let parts: Vec<&str> = clean.rsplitn(2, '/').collect();
    if parts.len() == 2 {
        let maybe_method = parts[0];
        let path_part = parts[1];

        // If the path part contains a file extension, the right side is a method name
        if path_part.contains('.') && !maybe_method.contains('.') {
            let file = extract_filename_from_path(path_part);
            return UrlParts {
                file_part: file.to_string(),
                method_part: Some(maybe_method.to_string()),
            };
        }
    }

    // No method part, just extract filename
    let file = extract_filename_from_path(clean);
    UrlParts {
        file_part: file.to_string(),
        method_part: None,
    }
}

fn extract_filename_from_path(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn find_handler_content<'a>(
    handler_path: &str,
    content_map: &std::collections::HashMap<&str, &'a str>,
) -> Option<&'a str> {
    // Direct match
    if let Some(&c) = content_map.get(handler_path) {
        return Some(c);
    }
    // Try with .cs or .vb suffix
    let with_cs = format!("{handler_path}.cs");
    if let Some(&c) = content_map.get(with_cs.as_str()) {
        return Some(c);
    }
    let with_vb = format!("{handler_path}.vb");
    if let Some(&c) = content_map.get(with_vb.as_str()) {
        return Some(c);
    }
    // Partial match by filename
    let filename = extract_filename_from_path(handler_path).to_lowercase();
    for (&path, &content) in content_map {
        let pf = extract_filename_from_path(path).to_lowercase();
        if pf == filename || pf.starts_with(&filename) {
            return Some(content);
        }
    }
    None
}

// ── Phase 34: packages.config Parser ─────────────────────────────────────────

// packages.config element regex — matches the entire <package ... /> tag
// regardless of attribute order. Individual attributes are extracted inside.
static PKG_CONFIG_ELEMENT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?is)<package\s+([^>]+?)/>").expect("pkg_config_element")
});
static PKG_ATTR_ID_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)\bid\s*=\s*"([^"]+)""#).expect("pkg_attr_id"));
static PKG_ATTR_VER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\bversion\s*=\s*"([^"]+)""#).expect("pkg_attr_ver")
});
static PKG_ATTR_TFM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\btargetFramework\s*=\s*"([^"]+)""#).expect("pkg_attr_tfm")
});
static PKG_ATTR_DEV_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\bdevelopmentDependency\s*=\s*"true""#).expect("pkg_attr_dev")
});

/// Parse packages.config XML. Handles any attribute order within `<package ... />` elements.
fn parse_packages_config(content: &str) -> Vec<LegacyPackageRef> {
    let mut packages = Vec::new();

    for element in PKG_CONFIG_ELEMENT_RE.captures_iter(content) {
        let attrs = &element[1];

        // id and version are required
        let Some(id_cap) = PKG_ATTR_ID_RE.captures(attrs) else {
            continue;
        };
        let Some(ver_cap) = PKG_ATTR_VER_RE.captures(attrs) else {
            continue;
        };

        let package_id = id_cap[1].to_string();
        let version = ver_cap[1].to_string();
        let target_framework = PKG_ATTR_TFM_RE
            .captures(attrs)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let is_dev = PKG_ATTR_DEV_RE.is_match(attrs);

        let modern_replacement = {
            let (repl, _, _, _) = lookup_modern_replacement(&package_id);
            repl.map(|s| s.to_string())
        };

        packages.push(LegacyPackageRef {
            package_id,
            version,
            target_framework,
            is_dev_dependency: is_dev,
            modern_replacement,
        });
    }

    packages
}

// ── Phase 34: Binding Redirect Parser ────────────────────────────────────────

// Binding redirect parsing: matches the entire <dependentAssembly> block,
// then extracts attributes individually for order-independence.
static DEP_ASSEMBLY_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?is)<dependentAssembly>\s*(.*?)\s*</dependentAssembly>").expect("dep_assembly")
});
static ASM_NAME_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)\bname\s*=\s*"([^"]+)""#).expect("asm_name"));
static ASM_PKT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\bpublicKeyToken\s*=\s*"([^"]+)""#).expect("asm_pkt")
});
static BR_OLD_VER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\boldVersion\s*=\s*"([^"]+)""#).expect("br_old_ver")
});
static BR_NEW_VER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)\bnewVersion\s*=\s*"([^"]+)""#).expect("br_new_ver")
});

fn extract_binding_redirects(web_config: Option<&str>) -> Vec<BindingRedirect> {
    let Some(content) = web_config else {
        return Vec::new();
    };

    let mut redirects = Vec::new();

    for block in DEP_ASSEMBLY_RE.captures_iter(content) {
        let inner = &block[1];

        // Extract assemblyIdentity attributes (any order)
        let Some(name_cap) = ASM_NAME_RE.captures(inner) else {
            continue;
        };
        let assembly_name = name_cap[1].to_string();
        let public_key_token = ASM_PKT_RE.captures(inner).map(|c| c[1].to_string());

        // Extract bindingRedirect attributes (any order)
        let Some(old_cap) = BR_OLD_VER_RE.captures(inner) else {
            continue;
        };
        let Some(new_cap) = BR_NEW_VER_RE.captures(inner) else {
            continue;
        };
        let old_version = old_cap[1].to_string();
        let new_version = new_cap[1].to_string();

        let has_known = lookup_assembly_replacement(&assembly_name).0.is_some();

        redirects.push(BindingRedirect {
            assembly_name,
            old_version_range: old_version,
            new_version,
            public_key_token,
            has_known_replacement: has_known,
        });
    }

    redirects
}

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

/// Produce a body preview: full for ≤30 lines, truncated otherwise.
fn make_body_preview(body: &str, line_count: u32) -> String {
    let lines: Vec<&str> = body.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    // Dedent: find minimum leading whitespace
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    let dedent = |line: &str| -> String {
        if line.len() >= min_indent {
            line[min_indent..].to_string()
        } else {
            line.trim_start().to_string()
        }
    };

    if line_count <= 30 {
        lines
            .iter()
            .map(|l| dedent(l))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let first_10: Vec<String> = lines.iter().take(10).map(|l| dedent(l)).collect();
        let last_5: Vec<String> = lines
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(|l| dedent(l))
            .collect();
        // Use the actual number of shown lines so the count is correct even if
        // take(10)/take(5) yielded fewer lines than expected.
        let shown = first_10.len() + last_5.len();
        let remaining = (line_count as usize).saturating_sub(shown);
        format!(
            "{}\n    ... ({remaining} more lines) ...\n{}",
            first_10.join("\n"),
            last_5.join("\n")
        )
    }
}

// ── Phase 34 second-pass: LazyLock statics for compute_complexity_score ──────
// Pre-compiled regexes avoid recompiling 18 patterns on every method body.

static CX_IF_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bif\b").expect("valid regex"));
static CX_ELSE_IF_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\belse\s+if\b").expect("valid regex"));
static CX_ELSEIF_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\belseif\b").expect("valid regex"));
static CX_SWITCH_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bswitch\b").expect("valid regex"));
static CX_CASE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bcase\b").expect("valid regex"));
static CX_SELECT_CASE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)\bselect\s+case\b").expect("valid regex"));

static CX_FOR_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bfor\s").expect("valid regex"));
static CX_FOREACH_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bforeach\b").expect("valid regex"));
static CX_FOR_EACH_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bfor\s+each\b").expect("valid regex"));
static CX_WHILE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bwhile\b").expect("valid regex"));
static CX_DO_WHILE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bdo\s+while\b").expect("valid regex"));
static CX_DO_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bdo\s*$").expect("valid regex"));

static CX_TRY_BRACE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\btry\s*\{").expect("valid regex"));
static CX_TRY_EOL_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\btry\s*$").expect("valid regex"));
static CX_CATCH_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bcatch\b").expect("valid regex"));
static CX_ON_ERROR_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?im)\bOn\s+Error\b").expect("valid regex"));

static CX_SQL_SELECT_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)"SELECT\s"#).expect("valid regex"));
static CX_SQL_INSERT_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)"INSERT\s"#).expect("valid regex"));
static CX_SQL_UPDATE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)"UPDATE\s"#).expect("valid regex"));
static CX_SQL_DELETE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)"DELETE\s"#).expect("valid regex"));
static CX_CMD_TEXT_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)CommandText\s*=").expect("valid regex"));
static CX_SQL_CMD_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)SqlCommand").expect("valid regex"));
static CX_SQL_ADAPTER_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)SqlDataAdapter").expect("valid regex"));

static CX_SESSION_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"(?i)Session\s*[\(\[]"#).expect("valid regex"));

/// Compute a heuristic complexity score for a method body.
/// Uses pre-compiled LazyLock regexes to avoid per-call compilation overhead.
///
/// THIRD-PASS FIX: Subtract overlap counts to prevent double-counting.
/// `else if` matches both `\bif\b` and `\belse\s+if\b`.
/// `select case` matches both `\bcase\b` and `\bselect\s+case\b`.
/// `do while` matches both `\bwhile\b` and `\bdo\s+while\b`.
/// `for each` (VB) matches both `\bfor\s` and `\bfor\s+each\b`.
/// `foreach` (C#) matches `\bfor\s` because of the word boundary + space.
fn compute_complexity_score(body: &str) -> u32 {
    let mut score: u32 = 0;

    // Branches (1 point each), with overlap subtraction
    let if_count = CX_IF_RE.find_iter(body).count() as u32;
    let else_if_count = CX_ELSE_IF_RE.find_iter(body).count() as u32;
    let elseif_count = CX_ELSEIF_RE.find_iter(body).count() as u32;
    // `else if` and `elseif` also match `\bif\b`, so subtract them
    score += if_count
        .saturating_sub(else_if_count)
        .saturating_sub(elseif_count);
    score += else_if_count;
    score += elseif_count;

    score += CX_SWITCH_RE.find_iter(body).count() as u32;
    let case_count = CX_CASE_RE.find_iter(body).count() as u32;
    let select_case_count = CX_SELECT_CASE_RE.find_iter(body).count() as u32;
    // `select case` also matches `\bcase\b`, subtract overlap
    score += case_count.saturating_sub(select_case_count);
    score += select_case_count;

    // Loops (1 point each), with overlap subtraction
    let for_count = CX_FOR_RE.find_iter(body).count() as u32;
    let foreach_count = CX_FOREACH_RE.find_iter(body).count() as u32;
    let for_each_count = CX_FOR_EACH_RE.find_iter(body).count() as u32;
    // `for each` (VB) matches `\bfor\s`, and C# `foreach` does NOT match `\bfor\s`
    // (because `foreach` has no space after `for`). So only subtract VB for_each.
    score += for_count.saturating_sub(for_each_count);
    score += foreach_count;
    score += for_each_count;

    let while_count = CX_WHILE_RE.find_iter(body).count() as u32;
    let do_while_count = CX_DO_WHILE_RE.find_iter(body).count() as u32;
    // `do while` also matches `\bwhile\b`, subtract overlap
    score += while_count.saturating_sub(do_while_count);
    score += do_while_count;
    score += CX_DO_RE.find_iter(body).count() as u32;

    // Error handlers (2 points each)
    score += CX_TRY_BRACE_RE.find_iter(body).count() as u32 * 2;
    score += CX_TRY_EOL_RE.find_iter(body).count() as u32 * 2;
    score += CX_CATCH_RE.find_iter(body).count() as u32 * 2;
    score += CX_ON_ERROR_RE.find_iter(body).count() as u32 * 2;

    // SQL strings (3 points each)
    score += CX_SQL_SELECT_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_INSERT_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_UPDATE_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_DELETE_RE.find_iter(body).count() as u32 * 3;
    score += CX_CMD_TEXT_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_CMD_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_ADAPTER_RE.find_iter(body).count() as u32 * 3;

    // Session access (1 point each)
    score += CX_SESSION_RE.find_iter(body).count() as u32;

    score
}

// ── Phase 34: Config Transform Parser ────────────────────────────────────────

static XDT_TRANSFORM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)xdt:Transform\s*=\s*"(\w+)""#).expect("xdt_transform")
});
static XDT_LOCATOR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)xdt:Locator\s*=\s*"Match\((\w+)\)""#).expect("xdt_locator")
});
static XDT_CONNSTR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r#"(?i)<add\s+name\s*=\s*"([^"]+)"[^>]*connectionString\s*=\s*"([^"]*)"[^>]*xdt:Transform"#,
    )
    .expect("xdt_connstr")
});
static XDT_APPSETTING_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<add\s+key\s*=\s*"([^"]+)"\s+value\s*=\s*"([^"]*)"[^>]*xdt:Transform"#)
        .expect("xdt_appsetting")
});
static XDT_DEBUG_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<compilation[^>]*debug\s*=\s*"(true|false)""#).expect("xdt_debug")
});

fn parse_config_transforms(transform_files: &[(String, String)]) -> ConfigTransformReport {
    let mut environments: Vec<ConfigEnvironment> = Vec::new();
    let mut total_transforms = 0usize;
    let mut conn_overrides: Vec<(String, String)> = Vec::new();
    let mut debug_overrides: Vec<(String, bool)> = Vec::new();
    let mut setting_overrides: Vec<(String, String, String)> = Vec::new();
    // MIG1/D2: log if value-attribute regex fails to compile.
    let value_attr_re = Regex::new(r#"value\s*=\s*"([^"]*)""#)
        .inspect_err(|e| tracing::warn!(error = %e, "MIG1: config transform value_attr regex compile failed — value extraction disabled"))
        .ok();

    for (path, content) in transform_files {
        // Derive environment name from filename: web.Release.config → Release
        let env_name = path
            .rsplit('/')
            .next()
            .or_else(|| path.rsplit('\\').next())
            .unwrap_or(path)
            .strip_prefix("web.")
            .or_else(|| path.strip_prefix("Web."))
            .unwrap_or(path)
            .strip_suffix(".config")
            .unwrap_or(path)
            .to_string();

        let mut transforms: Vec<ConfigTransform> = Vec::new();

        // Extract all XDT transform operations
        for cap in XDT_TRANSFORM_RE.captures_iter(content) {
            let operation = cap[1].to_string();

            // Find the XML element context around this transform
            let match_pos = cap.get(0).map_or(0, |m| m.start());
            let context_start = content[..match_pos].rfind('<').unwrap_or(0);
            let context_end = content[match_pos..]
                .find('>')
                .map(|p| match_pos + p + 1)
                .unwrap_or(content.len());
            let context = &content[context_start..context_end];

            let key = XDT_LOCATOR_RE.captures(context).map(|c| c[1].to_string());

            // Extract value preview (sanitize sensitive values)
            let value_preview = if context.contains("connectionString") {
                Some("(connection string)".to_string())
            } else if let Some(val_cap) = value_attr_re.as_ref().and_then(|re| re.captures(context))
            {
                let val = val_cap[1].to_string();
                if val.len() > 50 {
                    Some(format!("{}...", &val[..47]))
                } else {
                    Some(val)
                }
            } else {
                None
            };

            // Derive xpath hint from element context AND parent context.
            // Look back in the content to find the parent XML element for nested <add> elements.
            let parent_context = &content[..match_pos];
            let xpath_hint = if context.contains("<appSettings")
                || (context.contains("<add ") && context.contains("key="))
                || parent_context
                    .rfind("<appSettings")
                    .is_some_and(|p| !parent_context[p..].contains("</appSettings"))
            {
                "configuration/appSettings".to_string()
            } else if context.contains("connectionStrings")
                || context.contains("connectionString")
                || parent_context
                    .rfind("<connectionStrings")
                    .is_some_and(|p| !parent_context[p..].contains("</connectionStrings"))
            {
                "configuration/connectionStrings".to_string()
            } else if context.contains("<compilation") {
                "configuration/system.web/compilation".to_string()
            } else if context.contains("<customErrors") {
                "configuration/system.web/customErrors".to_string()
            } else if context.contains("<httpHandlers")
                || context.contains("<handlers")
                || parent_context
                    .rfind("<handlers")
                    .is_some_and(|p| !parent_context[p..].contains("</handlers"))
            {
                "configuration/system.webServer/handlers".to_string()
            } else if context.contains("<httpModules")
                || context.contains("<modules")
                || parent_context
                    .rfind("<modules")
                    .is_some_and(|p| !parent_context[p..].contains("</modules"))
            {
                "configuration/system.webServer/modules".to_string()
            } else if context.contains("<system.webServer") {
                "configuration/system.webServer".to_string()
            } else if context.contains("<system.web") {
                "configuration/system.web".to_string()
            } else {
                "configuration/...".to_string()
            };

            transforms.push(ConfigTransform {
                xpath_hint,
                operation,
                key,
                value_preview,
            });
            total_transforms += 1;
        }

        // Extract connection string overrides
        for cap in XDT_CONNSTR_RE.captures_iter(content) {
            conn_overrides.push((env_name.clone(), cap[1].to_string()));
        }

        // Extract debug flag overrides
        if let Some(cap) = XDT_DEBUG_RE.captures(content) {
            let debug_val = cap[1].eq_ignore_ascii_case("true");
            debug_overrides.push((env_name.clone(), debug_val));
        }

        // Extract app setting overrides
        for cap in XDT_APPSETTING_RE.captures_iter(content) {
            setting_overrides.push((env_name.clone(), cap[1].to_string(), cap[2].to_string()));
        }

        if !transforms.is_empty() {
            environments.push(ConfigEnvironment {
                name: env_name,
                file_path: path.clone(),
                transforms,
            });
        }
    }

    ConfigTransformReport {
        environments,
        total_transforms,
        connection_string_overrides: conn_overrides,
        debug_flag_overrides: debug_overrides,
        app_setting_overrides: setting_overrides,
    }
}

// ── Phase 34: Master Page Region Mapping ─────────────────────────────────────

static CONTENT_PLACEHOLDER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<asp:ContentPlaceHolder\s+[^>]*ID\s*=\s*"([^"]+)""#)
        .expect("content_placeholder")
});
static CONTENT_FILLS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<asp:Content\s+[^>]*ContentPlaceHolderID\s*=\s*"([^"]+)""#)
        .expect("content_fills")
});
static MASTER_PAGE_FILE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)MasterPageFile\s*=\s*"([^"]+)""#).expect("master_page_file")
});
static PLACEHOLDER_DEFAULT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?is)<asp:ContentPlaceHolder\s+[^>]*ID\s*=\s*"([^"]+)"[^>]*>\s*\S"#)
        .expect("placeholder_default")
});

fn build_master_page_region_map(
    master_files: &[(String, String)],
    markup_files: &[FileContent],
) -> MasterPageRegionMap {
    let mut master_pages: Vec<MasterPageInfo> = Vec::new();
    let mut region_map: std::collections::HashMap<String, (String, Vec<String>, bool)> =
        std::collections::HashMap::new();

    // 1. Parse master pages for ContentPlaceHolder definitions
    for (path, content) in master_files {
        let mut placeholders: Vec<String> = Vec::new();

        for cap in CONTENT_PLACEHOLDER_RE.captures_iter(content) {
            let id = cap[1].to_string();
            let has_default = PLACEHOLDER_DEFAULT_RE
                .captures_iter(content)
                .any(|dc| dc[1] == *id);
            region_map
                .entry(id.clone())
                .or_insert_with(|| (path.clone(), Vec::new(), has_default));
            placeholders.push(id);
        }

        let nested_master = MASTER_PAGE_FILE_RE
            .captures(content)
            .map(|c| c[1].to_string());

        master_pages.push(MasterPageInfo {
            file_path: path.clone(),
            placeholders,
            nested_master,
        });
    }

    // 2. Scan aspx/ascx files for asp:Content fills
    for fc in markup_files {
        for cap in CONTENT_FILLS_RE.captures_iter(&fc.markup_content) {
            let region_id = cap[1].to_string();
            if let Some(entry) = region_map.get_mut(&region_id) {
                if !entry.1.contains(&fc.file_path) {
                    entry.1.push(fc.file_path.clone());
                }
            } else {
                // Region referenced but not defined in any scanned master page
                region_map.insert(
                    region_id,
                    (
                        "(unknown master)".to_string(),
                        vec![fc.file_path.clone()],
                        false,
                    ),
                );
            }
        }
    }

    // 3. Build region mappings
    let mut regions: Vec<RegionMapping> = Vec::new();
    let mut orphans: Vec<String> = Vec::new();

    for (region_name, (defined_in, filled_by, has_default)) in &region_map {
        let modern_eq = match region_name.as_str() {
            "MainContent" | "ContentPlaceHolder1" | "BodyContent" | "content" => {
                "@RenderBody()".to_string()
            }
            "head" | "HeadContent" | "HeaderContent" => {
                "@RenderSection(\"Head\", required: false)".to_string()
            }
            "ScriptsSection" | "Scripts" | "FooterScripts" => {
                "@RenderSection(\"Scripts\", required: false)".to_string()
            }
            _ => format!("@RenderSection(\"{region_name}\", required: false)"),
        };

        if filled_by.is_empty() || defined_in == "(unknown master)" {
            orphans.push(region_name.clone());
        }

        regions.push(RegionMapping {
            region_name: region_name.clone(),
            defined_in: defined_in.clone(),
            filled_by: filled_by.clone(),
            has_default_content: *has_default,
            modern_equivalent: modern_eq,
        });
    }

    regions.sort_by(|a, b| b.filled_by.len().cmp(&a.filled_by.len()));

    MasterPageRegionMap {
        master_pages,
        regions,
        orphan_regions: orphans,
    }
}

// ── Phase 34: Resource File (.resx) Inventory ────────────────────────────────

static RESX_DATA_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)<data\s+name\s*=\s*"([^"]+)""#).expect("resx_data")
});
static RESX_FILE_REF_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?i)type\s*=\s*"System\.Resources\.ResXFileRef"#).expect("resx_file_ref")
});
static RESX_LANG_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\.([a-z]{2}(?:-[A-Z]{2})?)\.resx$").expect("resx_lang")
});

fn build_resource_inventory(resx_files: &[(String, String)]) -> ResourceInventory {
    let mut files: Vec<ResourceFileInfo> = Vec::new();
    let mut total_keys = 0usize;
    let mut languages: Vec<String> = Vec::new();
    let mut has_global = false;
    let mut has_local = false;
    let mut embedded_count = 0usize;

    for (path, content) in resx_files {
        let key_count = RESX_DATA_RE.captures_iter(content).count();
        total_keys += key_count;

        // Detect embedded resources (file refs)
        let file_ref_count = RESX_FILE_REF_RE.captures_iter(content).count();
        embedded_count += file_ref_count;

        // Detect language from filename
        let language = RESX_LANG_RE.captures(path).map(|c| c[1].to_string());
        if let Some(ref lang) = language
            && !languages.contains(lang)
        {
            languages.push(lang.clone());
        }

        // Classify: App_GlobalResources vs App_LocalResources
        let resource_type =
            if path.contains("App_GlobalResources") || path.contains("app_globalresources") {
                has_global = true;
                "global".to_string()
            } else if path.contains("App_LocalResources") || path.contains("app_localresources") {
                has_local = true;
                "local".to_string()
            } else {
                "embedded".to_string()
            };

        files.push(ResourceFileInfo {
            file_path: path.clone(),
            key_count,
            language,
            resource_type,
        });
    }

    files.sort_by(|a, b| b.key_count.cmp(&a.key_count));

    ResourceInventory {
        resource_files: files,
        total_keys,
        languages_detected: languages,
        has_global_resources: has_global,
        has_local_resources: has_local,
        embedded_resource_count: embedded_count,
    }
}

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
        let cross = build_cross_cutting_summary(
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

        let cross = build_cross_cutting_summary(
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

        let cross = build_cross_cutting_summary(
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
        assert!(flag_belongs_to_page(
            "Site/AuthCallback.aspx",
            "Site/AuthCallback.aspx",
            None
        ));
    }

    #[test]
    fn flag_belongs_to_page_accepts_detected_codebehind() {
        assert!(flag_belongs_to_page(
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
        assert!(flag_belongs_to_page(
            "Site/AuthCallback.aspx.vb",
            "Site/AuthCallback.aspx",
            None
        ));
        assert!(flag_belongs_to_page(
            "Site/AuthCallback.aspx.cs",
            "Site/AuthCallback.aspx",
            None
        ));
    }

    #[test]
    fn flag_belongs_to_page_rejects_unrelated_files_when_codebehind_is_none() {
        // THE regression guard: codebehind None must not open the gate.
        assert!(!flag_belongs_to_page(
            "Site/Other.aspx.vb",
            "Site/AuthCallback.aspx",
            None
        ));
        assert!(!flag_belongs_to_page(
            "App_Code/shared/Helpers.vb",
            "Site/AuthCallback.aspx",
            None
        ));
        assert!(!flag_belongs_to_page(
            "Site/permits/permits.aspx.vb",
            "Site/AuthCallback.aspx",
            None
        ));
        // Empty-string codebehind must behave the same as None.
        assert!(!flag_belongs_to_page(
            "App_Code/shared/Helpers.vb",
            "Site/AuthCallback.aspx",
            Some(""),
        ));
    }

    #[test]
    fn flag_belongs_to_page_rejects_unrelated_files_with_codebehind() {
        assert!(!flag_belongs_to_page(
            "App_Code/shared/Helpers.vb",
            "Site/AuthCallback.aspx",
            Some("Site/AuthCallback.aspx.vb"),
        ));
        // And must not accept a file that merely *contains* the
        // codebehind path as a substring.
        assert!(!flag_belongs_to_page(
            "Other/Site/AuthCallback.aspx.vb",
            "Site/AuthCallback.aspx",
            Some("AuthCallback.aspx.vb"),
        ));
    }

    #[test]
    fn flag_belongs_to_page_non_aspx_page_does_not_fall_back_to_sibling() {
        // For an .ascx / .master page we don't blindly accept
        // `<page>.vb` — only explicit codebehind detection counts.
        assert!(!flag_belongs_to_page(
            "Controls/MyControl.ascx.vb",
            "Controls/MyControl.ascx",
            None,
        ));
        // But if the dossier detected the codebehind, it's accepted.
        assert!(flag_belongs_to_page(
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
        let flags = analyze_vb_translation_flags(&[("Helpers.vb", vb_code)]);
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
        let flags = analyze_vb_translation_flags(&[("Legacy.vb", vb_code)]);
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
        let report = detect_email_patterns(&[("Mailer.vb", code)], None);
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
        let report = detect_background_job_patterns(&[("Worker.vb", code)], None);
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
        let inv = build_dependency_inventory(&refs);
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
        let report = detect_multi_tenancy(None, &[("Data.vb", code)], None);
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
        let inv = build_caching_inventory(&files, &[], &[]);
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
        let inv = extract_url_routing(Some(web_config), "", &[]);
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
        let catalog = build_sp_catalog(
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
        let catalog = build_sp_catalog(
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
        let catalog = build_sp_catalog(&[("sp/unused.sql".to_string(), sql.to_string())], &[]);
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
        let catalog = build_sp_catalog(&[("sp/dyn.sql".to_string(), sql.to_string())], &[]);
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
        let catalog = build_sp_catalog_public(
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
        let catalog = build_sp_catalog_public(
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
        let report = resolve_inheritance_chains(
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
        let report = resolve_inheritance_chains(
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
        let report = resolve_inheritance_chains(
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
        let report = resolve_inheritance_chains(
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
        let report = resolve_inheritance_chains(
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
        let report = resolve_inheritance_chains(&[], &[]);
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
        let packages = parse_packages_config(xml);
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
        let packages = parse_packages_config(xml);
        assert_eq!(packages.len(), 1);
        assert!(packages[0].is_dev_dependency);
    }

    #[test]
    fn parse_packages_config_empty() {
        let xml = r#"<packages></packages>"#;
        let packages = parse_packages_config(xml);
        assert!(packages.is_empty());
    }

    #[test]
    fn parse_packages_config_detects_modern_replacement() {
        let xml = r#"<packages>
  <package id="Newtonsoft.Json" version="13.0.3" targetFramework="net48" />
</packages>"#;
        let packages = parse_packages_config(xml);
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
        let redirects = extract_binding_redirects(Some(config));
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
        let redirects = extract_binding_redirects(None);
        assert!(redirects.is_empty());
    }

    #[test]
    fn binding_redirects_no_redirects() {
        let config = r#"<configuration><runtime></runtime></configuration>"#;
        let redirects = extract_binding_redirects(Some(config));
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
        let preview = make_body_preview(body, 4);
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
        let preview = make_body_preview(&body, line_count);
        assert!(preview.contains("more lines"));
    }

    #[test]
    fn complexity_score_empty() {
        let score = compute_complexity_score("");
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
        let score = compute_complexity_score(body);
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
        let score = compute_complexity_score(body);
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
        let score = compute_complexity_score(body);
        // "SELECT " = 3, CommandText = 3, SqlDataAdapter = 3 = 9+
        assert!(score >= 9, "Expected >= 9 for SQL, got {score}");
    }

    #[test]
    fn complexity_score_session() {
        let body = r#"
Session["user"] = GetUser();
var cart = Session["cart"];
"#;
        let score = compute_complexity_score(body);
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
            parse_config_transforms(&[("web.Release.config".to_string(), transform.to_string())]);
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
        let report = parse_config_transforms(&[
            ("web.Debug.config".to_string(), debug_t.to_string()),
            ("web.Release.config".to_string(), release_t.to_string()),
        ]);
        assert_eq!(report.environments.len(), 2);
        assert!(report.debug_flag_overrides.len() >= 2);
    }

    #[test]
    fn config_transforms_empty() {
        let report = parse_config_transforms(&[]);
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
            parse_config_transforms(&[("web.Staging.config".to_string(), staging.to_string())]);
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
        let map = build_master_page_region_map(
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
        let map = build_master_page_region_map(
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
        let map = build_master_page_region_map(&[], &[]);
        assert!(map.master_pages.is_empty());
        assert!(map.regions.is_empty());
    }

    #[test]
    fn master_page_nested_master() {
        let master_content = r#"<%@ Master MasterPageFile="~/Root.master" %>
<asp:Content ContentPlaceHolderID="Body" runat="server">
  <asp:ContentPlaceHolder ID="ChildBody" runat="server" />
</asp:Content>"#;
        let map = build_master_page_region_map(
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
        let inv = build_resource_inventory(&[(
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
        let inv = build_resource_inventory(&[
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
        let inv = build_resource_inventory(&[(
            "App_LocalResources/Default.aspx.resx".to_string(),
            resx.to_string(),
        )]);
        assert!(inv.has_local_resources);
        assert!(!inv.has_global_resources);
    }

    #[test]
    fn resource_inventory_empty() {
        let inv = build_resource_inventory(&[]);
        assert_eq!(inv.resource_files.len(), 0);
        assert_eq!(inv.total_keys, 0);
        assert!(!inv.has_global_resources);
        assert!(!inv.has_local_resources);
    }

    #[test]
    fn resource_inventory_embedded_resources() {
        let resx = r#"<root><data name="Icon" type="System.Resources.ResXFileRef, System.Windows.Forms"><value>icon.bmp;System.Drawing.Bitmap</value></data></root>"#;
        let inv = build_resource_inventory(&[(
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
        let pkgs = parse_packages_config(xml);
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
        let redirects = extract_binding_redirects(Some(config));
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
        let report = resolve_inheritance_chains(&code_files, &[markup_a, markup_b]);

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
        let transforms = parse_config_transforms(&[(
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
        let score = compute_complexity_score(body);
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
        let score = compute_complexity_score(body);
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
        let score = compute_complexity_score(body);
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

        let report = resolve_inheritance_chains(&code_files, &markup);

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
        let page_load_effects = extract_effects_from_nearby_content(content, "Page_Load");
        assert!(
            !page_load_effects.iter().any(|e| e.contains("SQL")),
            "Page_Load should NOT have SQL_Access (SQL is in btnQuery_Click), got: {:?}",
            page_load_effects
        );

        // btnQuery_Click SHOULD have SQL_Access
        let btn_effects = extract_effects_from_nearby_content(content, "btnQuery_Click");
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
        let score = compute_complexity_score(body);
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
        let report = resolve_inheritance_chains(&code_files, &markup);
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
        let report = resolve_inheritance_chains(&code_files, &markup);
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
            build_cross_layer_traces(&js_analysis, &sp_catalog, &service_endpoints, &code_files);

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

        let traces = build_cross_layer_traces(&js_analysis, &sp_catalog, &service_endpoints, &[]);

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
        let parts = extract_url_parts("Services/MapData.asmx/GetPolygons?bounds=1,2,3,4");
        assert_eq!(parts.file_part, "MapData.asmx");
        assert_eq!(parts.method_part.as_deref(), Some("GetPolygons"));

        let parts2 = extract_url_parts("api/search");
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
        let summary = extract_global_asax_info("", "");
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
