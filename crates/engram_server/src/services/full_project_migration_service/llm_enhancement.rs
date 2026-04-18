//! LLM-enhancement helpers for the full-project migration report.
//!
//! Phase 4 of the full_project_migration_service refactor. These
//! functions are opt-in (gated by the `use_llm` request flag) and
//! turn the deterministic report into a richer narrative by calling
//! the configured LLM backend. Everything here was moved verbatim
//! from the parent file.

#![allow(unused_imports, clippy::too_many_arguments)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::model::*;
use super::super::dossier_service::{self, MigrationDossier};
use super::super::migration_order_service::{self, MigrationOrderPlan};
// Wildcard pulls in parent-level items (edges_or_warn helpers, etc).
use super::*;

/// Numeric complexity ranking for a dossier — higher means "more worth
/// spending an LLM call on". Extracted from `estimated_complexity` (whose
/// prefix is `Low (score N)` / `Medium (score N)` / `High (score N)`) with
/// blast-radius score as the tiebreaker. Pure function, deterministic.
pub(super) fn dossier_llm_priority(d: &MigrationDossier) -> (u32, u8) {
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
pub(super) fn select_dossiers_for_llm<'a>(
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
pub(super) fn parse_page_llm_response(raw: &str) -> (Option<String>, Option<String>) {
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
pub(super) fn build_page_llm_prompt(
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
    use crate::services::business_logic_service::{analyze_file_logic, validate_llm_output};

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
                    crate::services::business_logic_service::FileBusinessLogic {
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
        crate::services::business_logic_service::FileBusinessLogic,
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

    report.markdown_report = rendering::render_markdown(
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

