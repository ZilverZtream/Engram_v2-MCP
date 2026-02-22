//! Full project migration analysis — the "one call, everything" service.
//!
//! Orchestrates every migration sub-service to produce a single comprehensive
//! report covering every file in the project.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_graph::GraphStore;
use serde::Serialize;

use super::auth_config_service::AuthConfigMap;
use super::db_strategy_service::{self, FileDataAccessProfile};
use super::dossier_service::{self, MigrationDossier};
use super::migration_order_service::{self, MigrationOrderPlan};
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
}

/// An item (table, state key, control) shared across multiple files.
#[derive(Debug, Clone, Serialize)]
pub struct SharedItem {
    pub name: String,
    pub used_by: Vec<String>,
}

/// Pre-read file content bundle passed into the blocking analysis.
pub struct FileContent {
    pub file_path: String,
    pub markup_content: String,
    pub codebehind_content: Option<String>,
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Analyze an entire project for migration.
///
/// All file content must be pre-read (async) and passed in.  Every sub-service
/// call inside is synchronous and safe for `spawn_blocking`.
pub fn analyze_full_project(
    graph: &Arc<GraphStore>,
    project_id: &str,
    target_stack: &str,
    file_contents: &[FileContent],
    web_config_content: Option<&str>,
    code_files: &[(&str, &str)],
    max_files: usize,
) -> anyhow::Result<FullProjectMigrationReport> {
    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Simple UTC timestamp without chrono dependency
        let days = secs / 86400;
        let time_secs = secs % 86400;
        let h = time_secs / 3600;
        let m = (time_secs % 3600) / 60;
        let s = time_secs % 60;
        // Approximate date from epoch days (good enough for display)
        let (y, mo, d) = epoch_days_to_date(days);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
    };

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
        code_files,
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

    // ── 3. Cross-cutting aggregation ──────────────────────────────────────

    let cross_cutting = build_cross_cutting_summary(&page_dossiers, &state_migration);

    // ── 4. Build the wave lookup (file_path → wave number) ────────────────

    let mut wave_lookup: BTreeMap<String, u32> = BTreeMap::new();
    for wave in &migration_order.waves {
        for wf in &wave.files {
            wave_lookup.insert(wf.path.clone(), wave.wave_number);
        }
    }

    // ── 5. Render markdown ────────────────────────────────────────────────

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
        markdown_report,
    })
}

// ── Cross-cutting aggregation ─────────────────────────────────────────────────

fn build_cross_cutting_summary(
    dossiers: &[MigrationDossier],
    state_report: &StateMigrationReport,
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
        let cross = build_cross_cutting_summary(&[], &state);
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

        let cross = build_cross_cutting_summary(&[dossier1, dossier2, dossier3], &state);

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
}
