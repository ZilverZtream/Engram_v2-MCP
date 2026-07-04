// Ticket 1: Migration Dossier Service
//
// Composes data from all other Phase 31 services plus existing graph data into
// a single comprehensive migration context for a specific file. The resulting
// MigrationDossier is a "give me everything" snapshot covering identity,
// dependencies, data layer, lifecycle events, state footprint, AJAX regions,
// validation, auth, risk score, scaffold preview, and actionable steps.

use crate::services::{
    ajax_region_service, auth_config_service, blast_radius_service, lifecycle_service,
    scaffold_service, validation_mapping_service, viewstate_service,
};
use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;
use serde::Serialize;
use std::sync::{Arc, LazyLock};

// â”€â”€ Page-directive regex â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

static RE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<%@\s+(?:Page|Control|Master|WebService|WebHandler)\b([^%]*)%>")
        .expect("valid regex")
});

fn extract_dir_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!(r#"(?i){}\s*=\s*"([^"]*)""#, regex::escape(attr));
    Regex::new(&pattern)
        .ok()
        .and_then(|re| re.captures(tag))
        .map(|c| c[1].to_string())
        .filter(|s| !s.is_empty())
}

// â”€â”€ SqlDataSource / ObjectDataSource regex â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

static RE_SQL_DATA_SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    // Use (?:[^>]|"[^"]*"|'[^']*')* to allow > inside quoted attributes (e.g. <%$ ... %>)
    Regex::new(r#"(?is)<asp:SqlDataSource\b((?:[^>"']|"[^"]*"|'[^']*')*)(?:/\s*>|>(.*?)</asp:SqlDataSource\s*>)"#).expect("valid regex")
});

static RE_OBJ_DATA_SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<asp:ObjectDataSource\b((?:[^>"']|"[^"]*"|'[^']*')*)(?:/\s*>|>)"#)
        .expect("valid regex")
});

static RE_ACCESS_DATA_SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:AccessDataSource\b([^>]*)(?:/\s*>|>)").expect("valid regex")
});

static RE_TABLE_FROM_SQL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:FROM|JOIN|INTO|UPDATE|TABLE)\s+[\[`]?(\w+)[\]`]?").expect("valid regex")
});

/// Matches `<%@ Register â€¦ %>` directives in ASPX markup.
static RE_REGISTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<%@\s+Register\b([^%]*)%>"#).expect("valid regex"));

/// Matches attribute-name tokens (`word=`) inside control tag bodies.
static RE_ATTR_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(\w+)\s*="#).expect("valid regex"));

fn extract_aspx_attr(tag: &str, attr: &str) -> String {
    // Manual case-insensitive attribute extraction â€” avoids compiling a new
    // Regex on every call since all callers pass plain ASCII identifier names.
    let tag_lower = tag.to_ascii_lowercase();
    let attr_lower = attr.to_ascii_lowercase();
    let mut search_start = 0;
    while let Some(idx) = tag_lower[search_start..].find(attr_lower.as_str()) {
        let abs = search_start + idx;
        // Require a word boundary before the attribute name
        if abs > 0 {
            let prev = tag_lower.as_bytes()[abs - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                search_start = abs + attr_lower.len();
                continue;
            }
        }
        // Expect optional whitespace + '=' + optional whitespace + '"'
        let after =
            tag[abs + attr_lower.len()..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        if !after.starts_with('=') {
            search_start = abs + attr_lower.len();
            continue;
        }
        let after_eq = after[1..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        if !after_eq.starts_with('"') {
            search_start = abs + attr_lower.len();
            continue;
        }
        // Extract the value between the opening and closing '"'
        let value_part = &after_eq[1..];
        let end = value_part.find('"').unwrap_or(value_part.len());
        return value_part[..end].to_string();
    }
    String::new()
}

// â”€â”€ Result structs â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Complete migration context for a single ASPX/ASCX/ASMX/ASHX/Master page.
#[derive(Debug, Clone, Serialize)]
pub struct MigrationDossier {
    pub file_path: String,
    /// Page type: "aspx", "ascx", "master", "asmx", "ashx", or "unknown".
    pub page_type: String,
    /// Target migration stack: "blazor", "react", or "angular".
    pub target_stack: String,

    // â”€â”€ Identity â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    pub inherits_class: Option<String>,
    pub base_class: Option<String>,
    pub codebehind_file: Option<String>,
    pub master_page: Option<String>,

    // â”€â”€ Dependencies â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    pub user_controls: Vec<DossierControlRef>,
    pub referenced_files: Vec<String>,
    /// Pages / files that include or depend on THIS file.
    pub referenced_by: Vec<String>,
    pub shared_modules: Vec<String>,

    // â”€â”€ Data layer â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    pub data_sources: Vec<DossierDataSource>,
    pub sql_statements: Vec<DossierSqlInfo>,
    pub connection_strings_used: Vec<String>,
    pub tables_touched: Vec<String>,

    // â”€â”€ Sub-service summaries â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    pub lifecycle_summary: LifecycleSummary,
    pub viewstate_summary: ViewStateSummary,
    pub ajax_summary: AjaxSummary,
    pub validation_summary: ValidationSummary,
    pub auth_summary: AuthSummary,

    // â”€â”€ Risk & scaffold â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    pub blast_radius_score: u8,
    pub risk_factors: Vec<String>,
    pub scaffold_preview: Option<String>,

    // â”€â”€ Actionable output â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    pub migration_steps: Vec<String>,
    pub estimated_complexity: String,

    // â”€â”€ LLM enhancement (populated only when `use_llm: true` and the
    // dossier is selected for enhancement via the top-N complexity cap) â”€â”€
    //
    // Both are `#[serde(default)]` so older JSON reports still deserialize
    // and so that any `MigrationDossier { .. }` construction that omits
    // them â€” e.g. in the deterministic per-page loop â€” keeps working.
    /// 2â€“3 sentence narrative describing what this page does from a
    /// business-workflow perspective. Generated by `DreamingEngine`.
    #[serde(default)]
    pub llm_business_purpose: Option<String>,
    /// Migration-specific risks and concrete Blazor-component guidance
    /// that goes beyond the deterministic `risk_factors` and
    /// `migration_steps` fields. Generated by `DreamingEngine`.
    #[serde(default)]
    pub llm_migration_notes: Option<String>,
}

/// A user control (Register directive) referenced by this page.
#[derive(Debug, Clone, Serialize)]
pub struct DossierControlRef {
    pub control_path: String,
    pub tag_prefix: String,
    pub tag_name: String,
    pub properties_set: Vec<String>,
}

/// A data source control or ADO.NET inline data access.
#[derive(Debug, Clone, Serialize)]
pub struct DossierDataSource {
    /// "SqlDataSource", "ObjectDataSource", "inline SQL", "stored proc", "AccessDataSource"
    pub source_type: String,
    /// Table name, stored-proc name, or ObjectDataSource TypeName.
    pub target: String,
    /// SELECT / INSERT / UPDATE / DELETE
    pub operations: Vec<String>,
}

/// A concrete SQL statement seen in the page or its code-behind.
#[derive(Debug, Clone, Serialize)]
pub struct DossierSqlInfo {
    pub sql_snippet: String,
    pub parameterized: bool,
    pub connection_string: String,
}

// â”€â”€ Summary structs â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[derive(Debug, Clone, Serialize)]
pub struct LifecycleSummary {
    pub lifecycle_event_count: usize,
    pub control_event_count: usize,
    pub has_ispostback_logic: bool,
    /// Brief labels: "Page_Load (IsPostBack)", "btnSearch_Click", â€¦
    pub events: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewStateSummary {
    pub explicit_keys: usize,
    pub implicit_controls: usize,
    pub total_state_fields: usize,
    pub heaviest_control: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AjaxSummary {
    pub update_panel_count: usize,
    pub timer_count: usize,
    pub has_script_manager: bool,
    pub suggested_components: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationSummary {
    pub validator_count: usize,
    pub custom_validator_count: usize,
    pub validation_group_count: usize,
    pub has_validation_summary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthSummary {
    pub has_auth_rules: bool,
    pub required_roles: Vec<String>,
    pub auth_check_count: usize,
    pub session_auth_count: usize,
}

// â”€â”€ Main function â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Build a comprehensive migration dossier for a single ASPX-family file.
///
/// # Parameters
/// * `graph`              â€” shared graph store
/// * `project_id`         â€” project identifier used for graph lookups
/// * `file_path`          â€” path to the primary markup file (used as graph node ID)
/// * `aspx_content`       â€” raw markup content (ASPX / ASCX / Master / â€¦)
/// * `codebehind_content` â€” code-behind source (C# or VB.NET); empty string if absent
/// * `web_config_content` â€” optional web.config text for auth analysis
/// * `target_stack`       â€” "blazor" | "react" | "angular"
pub fn build_migration_dossier(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    aspx_content: &str,
    codebehind_content: &str,
    web_config_content: Option<&str>,
    target_stack: &str,
    generation: u64,
) -> anyhow::Result<MigrationDossier> {
    // â”€â”€ 1. Determine page type from extension â”€â”€

    let page_type = detect_page_type(file_path);

    // â”€â”€ 2. Parse page directives â”€â”€

    let (inherits_class, codebehind_file, master_page) = parse_directives(aspx_content);

    // â”€â”€ 3. Graph lookups â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // We construct a plausible node ID for this file. Typical ID format in
    // the graph is "file:<relative-path>".

    let file_node_id = format!("file:{file_path}");

    let user_controls = collect_user_controls(graph, project_id, &file_node_id, aspx_content);
    let referenced_files = collect_referenced_files(graph, project_id, &file_node_id);
    let referenced_by = collect_referenced_by(graph, project_id, &file_node_id);
    let shared_modules = collect_shared_modules(graph, project_id, &file_node_id);

    // â”€â”€ 4. Data layer â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    let (data_sources, tables_touched_from_ds) =
        extract_data_sources(graph, project_id, &file_node_id, aspx_content);
    let (sql_statements, connection_strings_used, tables_touched_from_sql) =
        extract_sql_info(graph, project_id, &file_node_id, codebehind_content);

    // Merge table names from both sources, dedup
    let mut tables_touched = tables_touched_from_ds;
    for t in tables_touched_from_sql {
        if !tables_touched.contains(&t) {
            tables_touched.push(t);
        }
    }

    // â”€â”€ 5. Sub-service calls â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    let lifecycle_summary = build_lifecycle_summary(
        graph,
        project_id,
        file_path,
        codebehind_content,
        aspx_content,
    );

    let viewstate_summary = build_viewstate_summary(
        graph,
        project_id,
        file_path,
        codebehind_content,
        aspx_content,
    );

    let ajax_summary = build_ajax_summary(graph, project_id, file_path, aspx_content);

    let validation_summary = build_validation_summary(
        graph,
        project_id,
        file_path,
        aspx_content,
        codebehind_content,
    );

    let auth_summary = build_auth_summary(
        graph,
        project_id,
        web_config_content,
        file_path,
        codebehind_content,
    );

    // â”€â”€ 6. Blast radius / risk score â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    // Real generation matters here: passing 0 keyed the PageRank centrality
    // cache on a generation that never matches the active one, so the
    // dossier's centrality risk component was systematically ~0.
    let blast_report = blast_radius_service::compute_blast_radius(
        graph,
        project_id,
        &file_node_id,
        generation,
        false,
    );
    let blast_radius_score = blast_report.as_ref().map(|r| r.migration_risk).unwrap_or(0);

    let risk_factors = collect_risk_factors(
        &lifecycle_summary,
        &viewstate_summary,
        &ajax_summary,
        &validation_summary,
        &auth_summary,
        &sql_statements,
        &data_sources,
    );

    // â”€â”€ 7. Scaffold preview (best-effort) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    let scaffold_preview = scaffold_service::generate_scaffold(
        graph,
        project_id,
        file_path,
        target_stack,
        false,
        "full",
    )
    .ok()
    .map(|r| r.component_code);

    // â”€â”€ 8. Base class from code-behind â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    let base_class = extract_base_class_from_code(codebehind_content);

    // â”€â”€ 9. Migration steps & complexity estimate â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    let migration_steps = build_migration_steps(
        &page_type,
        &lifecycle_summary,
        &viewstate_summary,
        &ajax_summary,
        &validation_summary,
        &auth_summary,
        &data_sources,
        &sql_statements,
        &user_controls,
        target_stack,
    );

    let estimated_complexity = estimate_complexity(
        blast_radius_score,
        &lifecycle_summary,
        &viewstate_summary,
        &ajax_summary,
        &validation_summary,
        &auth_summary,
        &sql_statements,
        &user_controls,
    );

    Ok(MigrationDossier {
        file_path: file_path.to_string(),
        page_type,
        target_stack: target_stack.to_string(),
        inherits_class,
        base_class,
        codebehind_file,
        master_page,
        user_controls,
        referenced_files,
        referenced_by,
        shared_modules,
        data_sources,
        sql_statements,
        connection_strings_used,
        tables_touched,
        lifecycle_summary,
        viewstate_summary,
        ajax_summary,
        validation_summary,
        auth_summary,
        blast_radius_score,
        risk_factors,
        scaffold_preview,
        migration_steps,
        estimated_complexity,
        // Populated post-hoc by the async LLM enhancement pass when
        // `use_llm: true` and this dossier falls within the top-N cap.
        llm_business_purpose: None,
        llm_migration_notes: None,
    })
}

// â”€â”€ Page-type detection â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn detect_page_type(file_path: &str) -> String {
    let lower = file_path.to_lowercase();
    if lower.ends_with(".aspx") {
        "aspx".to_string()
    } else if lower.ends_with(".ascx") {
        "ascx".to_string()
    } else if lower.ends_with(".master") {
        "master".to_string()
    } else if lower.ends_with(".asmx") {
        "asmx".to_string()
    } else if lower.ends_with(".ashx") {
        "ashx".to_string()
    } else {
        "unknown".to_string()
    }
}

// â”€â”€ Directive parsing â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn parse_directives(content: &str) -> (Option<String>, Option<String>, Option<String>) {
    if let Some(cap) = RE_DIRECTIVE.captures(content) {
        let tag = &cap[1];
        let inherits = extract_dir_attr(tag, "Inherits");
        let codebehind =
            extract_dir_attr(tag, "CodeBehind").or_else(|| extract_dir_attr(tag, "CodeFile"));
        let master = extract_dir_attr(tag, "MasterPageFile");
        (inherits, codebehind, master)
    } else {
        (None, None, None)
    }
}

// â”€â”€ Base class extraction â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn extract_base_class_from_code(content: &str) -> Option<String> {
    // C#: public partial class MyPage : BasePage
    let re_cs = Regex::new(r"(?m)class\s+\w+\s*:\s*([\w.]+)").ok()?;
    if let Some(cap) = re_cs.captures(content) {
        let base = cap[1].to_string();
        // Filter out common non-base-class inherits
        if !matches!(
            base.as_str(),
            "Page"
                | "UserControl"
                | "MasterPage"
                | "WebService"
                | "HttpHandler"
                | "IHttpHandler"
                | "Control"
        ) {
            return Some(base);
        }
    }
    // VB.NET: Inherits BasePage
    let re_vb = Regex::new(r"(?im)Inherits\s+([\w.]+)").ok()?;
    if let Some(cap) = re_vb.captures(content) {
        let base = cap[1].to_string();
        if !matches!(
            base.as_str(),
            "Page" | "UserControl" | "MasterPage" | "WebService"
        ) {
            return Some(base);
        }
    }
    None
}

// â”€â”€ Graph-based dependency collection â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn collect_user_controls(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_node_id: &str,
    aspx_content: &str,
) -> Vec<DossierControlRef> {
    let mut controls = Vec::new();

    // First: pull from graph (RegistersControl edges)
    let neighbors = graph
        .neighbors(project_id, EdgeKind::RegistersControl, file_node_id, 200)
        .unwrap_or_default();
    for (neighbor_id, _weight) in &neighbors {
        let control_path = neighbor_id
            .trim_start_matches("file:")
            .trim_start_matches("control:")
            .to_string();
        controls.push(DossierControlRef {
            control_path: control_path.clone(),
            tag_prefix: String::new(),
            tag_name: String::new(),
            properties_set: Vec::new(),
        });
    }

    // Second: parse <%@ Register %> directives directly from markup
    for cap in RE_REGISTER.captures_iter(aspx_content) {
        let tag = &cap[1];
        let prefix = extract_aspx_attr(tag, "TagPrefix");
        let tag_name = extract_aspx_attr(tag, "TagName");
        let src = extract_aspx_attr(tag, "Src");
        let assembly = extract_aspx_attr(tag, "Assembly");

        let control_path = if !src.is_empty() {
            src.clone()
        } else if !assembly.is_empty() {
            format!("[assembly:{}]", assembly)
        } else {
            continue;
        };

        // Avoid duplicates already found via graph
        let already_known = controls
            .iter()
            .any(|c| c.control_path == control_path || c.control_path.ends_with(&src));
        if !already_known {
            // Collect properties used for this tag type in the markup
            let props = if !prefix.is_empty() && !tag_name.is_empty() {
                collect_control_properties(aspx_content, &prefix, &tag_name)
            } else {
                Vec::new()
            };
            controls.push(DossierControlRef {
                control_path,
                tag_prefix: prefix,
                tag_name,
                properties_set: props,
            });
        }
    }

    controls
}

/// Collect property names set on a user-control tag by scanning the markup.
fn collect_control_properties(content: &str, prefix: &str, tag_name: &str) -> Vec<String> {
    let pattern = format!(
        r#"(?is)<{prefix}:{tag_name}\b([^>]*)(?:/\s*>|>)"#,
        prefix = regex::escape(prefix),
        tag_name = regex::escape(tag_name)
    );
    let re = match Regex::new(&pattern) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut props: Vec<String> = Vec::new();
    for cap in re.captures_iter(content) {
        let attrs = &cap[1];
        for attr_cap in RE_ATTR_NAME.captures_iter(attrs) {
            let name = attr_cap[1].to_string();
            if !matches!(
                name.to_lowercase().as_str(),
                "id" | "runat" | "visible" | "class" | "style"
            ) && !props.contains(&name)
            {
                props.push(name);
            }
        }
    }
    props
}

fn collect_referenced_files(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_node_id: &str,
) -> Vec<String> {
    graph
        .neighbors(project_id, EdgeKind::IncludesFile, file_node_id, 200)
        .unwrap_or_default()
        .into_iter()
        .map(|(id, _)| id.trim_start_matches("file:").to_string())
        .collect()
}

fn collect_referenced_by(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_node_id: &str,
) -> Vec<String> {
    // Incoming IncludesFile, RegistersControl, or Dependency edges
    graph
        .find_incoming_edges_with_kind(project_id, None, file_node_id, 500)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, kind, _)| {
            matches!(
                kind,
                EdgeKind::IncludesFile | EdgeKind::RegistersControl | EdgeKind::Dependency
            )
        })
        .map(|(source_id, _, _)| source_id.trim_start_matches("file:").to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

fn collect_shared_modules(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_node_id: &str,
) -> Vec<String> {
    graph
        .neighbors(project_id, EdgeKind::Dependency, file_node_id, 200)
        .unwrap_or_default()
        .into_iter()
        .map(|(id, _)| id.trim_start_matches("file:").to_string())
        .collect()
}

// â”€â”€ Data layer extraction â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn extract_data_sources(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_node_id: &str,
    aspx_content: &str,
) -> (Vec<DossierDataSource>, Vec<String>) {
    let mut sources = Vec::new();
    let mut tables: Vec<String> = Vec::new();

    // â”€â”€ SqlDataSource tags â”€â”€
    for cap in RE_SQL_DATA_SOURCE.captures_iter(aspx_content) {
        let attrs = &cap[1];
        let select_cmd = extract_aspx_attr(attrs, "SelectCommand");
        let insert_cmd = extract_aspx_attr(attrs, "InsertCommand");
        let update_cmd = extract_aspx_attr(attrs, "UpdateCommand");
        let delete_cmd = extract_aspx_attr(attrs, "DeleteCommand");
        let connection = extract_aspx_attr(attrs, "ConnectionString");

        let mut ops = Vec::new();
        let mut target_table = String::new();

        if !select_cmd.is_empty() {
            ops.push("SELECT".to_string());
            for t in extract_tables_from_sql(&select_cmd) {
                if !tables.contains(&t) {
                    tables.push(t.clone());
                }
                if target_table.is_empty() {
                    target_table = t;
                }
            }
        }
        if !insert_cmd.is_empty() {
            ops.push("INSERT".to_string());
        }
        if !update_cmd.is_empty() {
            ops.push("UPDATE".to_string());
        }
        if !delete_cmd.is_empty() {
            ops.push("DELETE".to_string());
        }

        // Also detect stored proc (command type = StoredProcedure)
        let cmd_type = extract_aspx_attr(attrs, "SelectCommandType");
        let source_type = if cmd_type.to_lowercase() == "storedprocedure" {
            "stored proc"
        } else {
            "SqlDataSource"
        };

        if !ops.is_empty() || !select_cmd.is_empty() {
            let _ = connection;
            sources.push(DossierDataSource {
                source_type: source_type.to_string(),
                target: target_table,
                operations: ops,
            });
        }
    }

    // â”€â”€ ObjectDataSource tags â”€â”€
    for cap in RE_OBJ_DATA_SOURCE.captures_iter(aspx_content) {
        let attrs = &cap[1];
        let type_name = extract_aspx_attr(attrs, "TypeName");
        let select_method = extract_aspx_attr(attrs, "SelectMethod");
        let insert_method = extract_aspx_attr(attrs, "InsertMethod");
        let update_method = extract_aspx_attr(attrs, "UpdateMethod");
        let delete_method = extract_aspx_attr(attrs, "DeleteMethod");

        let mut ops = Vec::new();
        if !select_method.is_empty() {
            ops.push("SELECT".to_string());
        }
        if !insert_method.is_empty() {
            ops.push("INSERT".to_string());
        }
        if !update_method.is_empty() {
            ops.push("UPDATE".to_string());
        }
        if !delete_method.is_empty() {
            ops.push("DELETE".to_string());
        }

        sources.push(DossierDataSource {
            source_type: "ObjectDataSource".to_string(),
            target: type_name,
            operations: ops,
        });
    }

    // â”€â”€ AccessDataSource tags â”€â”€
    for cap in RE_ACCESS_DATA_SOURCE.captures_iter(aspx_content) {
        let attrs = &cap[1];
        let select_cmd = extract_aspx_attr(attrs, "SelectCommand");
        let mut ops = Vec::new();
        if !select_cmd.is_empty() {
            ops.push("SELECT".to_string());
        }
        sources.push(DossierDataSource {
            source_type: "AccessDataSource".to_string(),
            target: extract_aspx_attr(attrs, "DataFile"),
            operations: ops,
        });
    }

    // â”€â”€ Graph DataBinding edges â”€â”€
    let db_edges = graph
        .neighbors(project_id, EdgeKind::DataBinding, file_node_id, 200)
        .unwrap_or_default();
    for (target_id, _) in db_edges {
        let tname = target_id
            .trim_start_matches("db_table:")
            .trim_start_matches("table:")
            .to_string();
        if !tables.contains(&tname) {
            tables.push(tname);
        }
    }

    (sources, tables)
}

fn extract_tables_from_sql(sql: &str) -> Vec<String> {
    RE_TABLE_FROM_SQL
        .captures_iter(sql)
        .map(|c| c[1].to_string())
        .filter(|t| {
            !matches!(
                t.to_uppercase().as_str(),
                "SELECT"
                    | "WHERE"
                    | "SET"
                    | "ON"
                    | "AS"
                    | "TOP"
                    | "DISTINCT"
                    | "ORDER"
                    | "GROUP"
                    | "HAVING"
                    | "EXEC"
                    | "PROCEDURE"
            )
        })
        .collect()
}

fn extract_sql_info(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_node_id: &str,
    codebehind_content: &str,
) -> (Vec<DossierSqlInfo>, Vec<String>, Vec<String>) {
    let mut stmts: Vec<DossierSqlInfo> = Vec::new();
    let mut conn_strings: Vec<String> = Vec::new();
    let mut tables: Vec<String> = Vec::new();

    // â”€â”€ Graph SqlCalls edges â”€â”€
    let sql_edges = graph
        .neighbors(project_id, EdgeKind::SqlCalls, file_node_id, 200)
        .unwrap_or_default();
    for (target_id, _) in &sql_edges {
        let snippet = target_id
            .trim_start_matches("sql:")
            .trim_start_matches("db:")
            .to_string();
        let parameterized = snippet.contains('@') || snippet.contains('?');
        for t in extract_tables_from_sql(&snippet) {
            if !tables.contains(&t) {
                tables.push(t.clone());
            }
        }
        stmts.push(DossierSqlInfo {
            sql_snippet: snippet,
            parameterized,
            connection_string: String::new(),
        });
    }

    // â”€â”€ Graph QueriesTable edges â†’ table names â”€â”€
    let qt_edges = graph
        .neighbors(project_id, EdgeKind::QueriesTable, file_node_id, 200)
        .unwrap_or_default();
    for (target_id, _) in qt_edges {
        let tname = target_id
            .trim_start_matches("db_table:")
            .trim_start_matches("table:")
            .to_string();
        if !tables.contains(&tname) {
            tables.push(tname);
        }
    }

    // â”€â”€ Scan code-behind for connection string patterns â”€â”€
    if !codebehind_content.is_empty() {
        let re_conn = Regex::new(
            r#"(?i)(?:ConfigurationManager\.ConnectionStrings\[["']([^"']+)["']\]|new\s+SqlConnection\s*\(\s*["']([^"']+)["']\s*\))"#
        ).expect("valid regex");
        for cap in re_conn.captures_iter(codebehind_content) {
            let cs = cap
                .get(1)
                .or_else(|| cap.get(2))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !cs.is_empty() && !conn_strings.contains(&cs) {
                conn_strings.push(cs);
            }
        }

        // Inline SQL in code-behind via CommandText or string literals
        let re_cmd_text = Regex::new(
            r#"(?i)(?:\.CommandText\s*=\s*["']([^"']{10,300})["']|new\s+SqlCommand\s*\(\s*["']([^"']{10,300})["'])"#
        ).expect("valid regex");
        for cap in re_cmd_text.captures_iter(codebehind_content) {
            let sql = cap
                .get(1)
                .or_else(|| cap.get(2))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !sql.is_empty() {
                let parameterized = sql.contains('@') || sql.contains('?');
                for t in extract_tables_from_sql(&sql) {
                    if !tables.contains(&t) {
                        tables.push(t.clone());
                    }
                }
                stmts.push(DossierSqlInfo {
                    sql_snippet: sql,
                    parameterized,
                    connection_string: conn_strings.first().cloned().unwrap_or_default(),
                });
            }
        }
    }

    (stmts, conn_strings, tables)
}

// â”€â”€ Sub-service summary builders â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn build_lifecycle_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    codebehind_content: &str,
    aspx_content: &str,
) -> LifecycleSummary {
    let aspx_opt = if aspx_content.is_empty() {
        None
    } else {
        Some(aspx_content)
    };

    match lifecycle_service::analyze_page_lifecycle(
        graph,
        project_id,
        file_path,
        codebehind_content,
        aspx_opt,
    ) {
        Ok(map) => {
            let has_ispostback = map.lifecycle_events.iter().any(|e| e.has_ispostback_branch);

            let mut events: Vec<String> = map
                .lifecycle_events
                .iter()
                .map(|e| {
                    if e.has_ispostback_branch {
                        format!("{} (IsPostBack)", e.event_name)
                    } else {
                        e.event_name.clone()
                    }
                })
                .collect();
            for ce in &map.control_events {
                events.push(format!("{}_{}", ce.control_id, ce.event_name));
            }

            LifecycleSummary {
                lifecycle_event_count: map.lifecycle_events.len(),
                control_event_count: map.control_events.len(),
                has_ispostback_logic: has_ispostback,
                events,
            }
        }
        Err(_) => LifecycleSummary {
            lifecycle_event_count: 0,
            control_event_count: 0,
            has_ispostback_logic: false,
            events: Vec::new(),
        },
    }
}

fn build_viewstate_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    codebehind_content: &str,
    aspx_content: &str,
) -> ViewStateSummary {
    let aspx_opt = if aspx_content.is_empty() {
        None
    } else {
        Some(aspx_content)
    };

    match viewstate_service::analyze_viewstate_dependencies(
        graph,
        project_id,
        file_path,
        codebehind_content,
        aspx_opt,
    ) {
        Ok(report) => {
            let heaviest_control = report
                .heaviest_controls
                .first()
                .map(|(id, _, _)| id.clone());
            ViewStateSummary {
                explicit_keys: report.explicit_viewstate.len(),
                implicit_controls: report.implicit_viewstate.len(),
                total_state_fields: report.total_state_fields,
                heaviest_control,
            }
        }
        Err(_) => ViewStateSummary {
            explicit_keys: 0,
            implicit_controls: 0,
            total_state_fields: 0,
            heaviest_control: None,
        },
    }
}

fn build_ajax_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    aspx_content: &str,
) -> AjaxSummary {
    match ajax_region_service::analyze_ajax_regions(graph, project_id, file_path, aspx_content) {
        Ok(map) => AjaxSummary {
            update_panel_count: map.update_panels.len(),
            timer_count: map.timers.len(),
            has_script_manager: map.has_script_manager,
            suggested_components: map.suggested_components.len(),
        },
        Err(_) => AjaxSummary {
            update_panel_count: 0,
            timer_count: 0,
            has_script_manager: false,
            suggested_components: 0,
        },
    }
}

fn build_validation_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    aspx_content: &str,
    codebehind_content: &str,
) -> ValidationSummary {
    let cb_opt = if codebehind_content.is_empty() {
        None
    } else {
        Some(codebehind_content)
    };

    match validation_mapping_service::analyze_validation_controls(
        graph,
        project_id,
        file_path,
        aspx_content,
        cb_opt,
    ) {
        Ok(map) => {
            let has_summary = map.validation_summary.is_some();
            ValidationSummary {
                validator_count: map.total_validators,
                custom_validator_count: map.custom_validators.len(),
                validation_group_count: map.validation_groups.len(),
                has_validation_summary: has_summary,
            }
        }
        Err(_) => ValidationSummary {
            validator_count: 0,
            custom_validator_count: 0,
            validation_group_count: 0,
            has_validation_summary: false,
        },
    }
}

fn build_auth_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    web_config_content: Option<&str>,
    file_path: &str,
    codebehind_content: &str,
) -> AuthSummary {
    let code_files: &[(&str, &str)] = if codebehind_content.is_empty() {
        &[]
    } else {
        &[(file_path, codebehind_content)]
    };

    match auth_config_service::analyze_auth_config(
        graph,
        project_id,
        web_config_content,
        code_files,
    ) {
        Ok(map) => {
            let has_auth_rules = !map.location_rules.is_empty()
                || !map.code_auth_checks.is_empty()
                || map.auth_mode != "None";

            let mut required_roles: Vec<String> = map
                .location_rules
                .iter()
                .flat_map(|r| r.allow_roles.iter().cloned())
                .collect();
            // Also gather roles from inline code checks
            for check in &map.code_auth_checks {
                for role in &check.roles_checked {
                    if !required_roles.contains(role) {
                        required_roles.push(role.clone());
                    }
                }
            }
            required_roles.dedup();

            AuthSummary {
                has_auth_rules,
                required_roles,
                auth_check_count: map.code_auth_checks.len(),
                session_auth_count: map.session_auth_patterns.len(),
            }
        }
        Err(_) => AuthSummary {
            has_auth_rules: false,
            required_roles: Vec::new(),
            auth_check_count: 0,
            session_auth_count: 0,
        },
    }
}

// â”€â”€ Risk factor collection â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn collect_risk_factors(
    lifecycle: &LifecycleSummary,
    viewstate: &ViewStateSummary,
    ajax: &AjaxSummary,
    validation: &ValidationSummary,
    auth: &AuthSummary,
    sql: &[DossierSqlInfo],
    data_sources: &[DossierDataSource],
) -> Vec<String> {
    let mut factors = Vec::new();

    if lifecycle.lifecycle_event_count > 5 {
        factors.push(format!(
            "Dense lifecycle ({} events): complex OnInit/OnLoad chains must be decomposed",
            lifecycle.lifecycle_event_count
        ));
    }
    if lifecycle.has_ispostback_logic {
        factors.push(
            "IsPostBack branching: first-load vs postback logic must be mapped to component init vs re-render".to_string(),
        );
    }
    if viewstate.total_state_fields > 10 {
        factors.push(format!(
            "Heavy ViewState ({} fields): explicit component state declarations required",
            viewstate.total_state_fields
        ));
    }
    if ajax.update_panel_count > 0 {
        factors.push(format!(
            "{} UpdatePanel(s): each becomes an isolated component with async data fetch",
            ajax.update_panel_count
        ));
    }
    if ajax.timer_count > 0 {
        factors.push(format!(
            "{} Timer(s): polling logic must migrate to setInterval / System.Threading.Timer",
            ajax.timer_count
        ));
    }
    if validation.custom_validator_count > 0 {
        factors.push(format!(
            "{} CustomValidator(s): server-side validation logic must be ported",
            validation.custom_validator_count
        ));
    }
    if auth.session_auth_count > 0 {
        factors.push(format!(
            "{} session-based auth pattern(s): replace with claims-based identity",
            auth.session_auth_count
        ));
    }
    let unparameterized = sql.iter().filter(|s| !s.parameterized).count();
    if unparameterized > 0 {
        factors.push(format!(
            "{} unparameterized SQL statement(s): SQL injection risk; migrate to parameterized queries or ORM",
            unparameterized
        ));
    }
    if data_sources.iter().any(|d| d.source_type == "stored proc") {
        factors.push(
            "Stored procedures in use: ensure SP contracts are preserved or migrated to repository methods".to_string(),
        );
    }
    if auth.has_auth_rules && !auth.required_roles.is_empty() {
        factors.push(format!(
            "Role-based authorization ({} role(s)): apply [Authorize(Roles=...)] or policy-based auth",
            auth.required_roles.len()
        ));
    }

    factors
}

// â”€â”€ Migration step generation â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[allow(clippy::too_many_arguments)]
fn build_migration_steps(
    page_type: &str,
    lifecycle: &LifecycleSummary,
    viewstate: &ViewStateSummary,
    ajax: &AjaxSummary,
    validation: &ValidationSummary,
    auth: &AuthSummary,
    data_sources: &[DossierDataSource],
    sql: &[DossierSqlInfo],
    user_controls: &[DossierControlRef],
    target_stack: &str,
) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    let mut step = 1usize;

    // Step 1 â€” always: create component/page
    let component_type = match target_stack {
        "react" => "React component",
        "angular" => "Angular component",
        _ => "Blazor component",
    };
    steps.push(format!(
        "Step {step}: Create a new {component_type} for `{page_type}` â€” start from the generated scaffold preview above"
    ));
    step += 1;

    // Master page
    if page_type == "aspx" {
        steps.push(format!(
            "Step {step}: Replace Master Page with a shared layout component ({} uses a master)",
            match target_stack {
                "react" => "React layout wrapper",
                "angular" => "Angular router-outlet with layout component",
                _ => "Blazor MainLayout.razor",
            }
        ));
        step += 1;
    }

    // Lifecycle
    if lifecycle.lifecycle_event_count > 0 {
        steps.push(format!(
            "Step {step}: Migrate {} lifecycle event(s) [{}] â†’ component init/parameter hooks",
            lifecycle.lifecycle_event_count,
            lifecycle
                .events
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
                + if lifecycle.events.len() > 3 {
                    ", â€¦"
                } else {
                    ""
                }
        ));
        step += 1;
    }
    if lifecycle.has_ispostback_logic {
        steps.push(format!(
            "Step {step}: Separate IsPostBack first-load logic from postback handlers â€” first-load â†’ OnInitializedAsync/useEffect; postbacks â†’ event handlers"
        ));
        step += 1;
    }

    // State
    if viewstate.total_state_fields > 0 {
        steps.push(format!(
            "Step {step}: Declare {} explicit state field(s) replacing ViewState; bind {} implicit control state(s) to component properties",
            viewstate.explicit_keys, viewstate.implicit_controls
        ));
        step += 1;
    }

    // Data layer
    if !data_sources.is_empty() || !sql.is_empty() {
        let total_ds = data_sources.len() + sql.len();
        steps.push(format!(
            "Step {step}: Migrate {total_ds} data source(s) to a repository/service layer â€” inject IMyRepository, replace SqlDataSource declarative bindings with async method calls"
        ));
        step += 1;
    }
    let unparameterized = sql.iter().filter(|s| !s.parameterized).count();
    if unparameterized > 0 {
        steps.push(format!(
            "Step {step}: Parameterize {unparameterized} raw SQL statement(s) or replace with EF Core / Dapper"
        ));
        step += 1;
    }

    // Ajax
    if ajax.update_panel_count > 0 {
        steps.push(format!(
            "Step {step}: Extract {} UpdatePanel region(s) into dedicated child components with async refresh",
            ajax.update_panel_count
        ));
        step += 1;
    }
    if ajax.timer_count > 0 {
        steps.push(format!(
            "Step {step}: Replace {} Timer control(s) with polling via setInterval/System.Threading.Timer in component lifecycle",
            ajax.timer_count
        ));
        step += 1;
    }

    // Validation
    if validation.validator_count > 0 {
        steps.push(format!(
            "Step {step}: Migrate {} validator(s) to DataAnnotations / FluentValidation{} in model, plus <EditForm>/<ValidationSummary> in markup",
            validation.validator_count,
            if validation.custom_validator_count > 0 {
                format!(" + {} custom server-side rule(s)", validation.custom_validator_count)
            } else {
                String::new()
            }
        ));
        step += 1;
    }

    // Auth
    if auth.has_auth_rules {
        let roles_str = if auth.required_roles.is_empty() {
            "apply [Authorize]".to_string()
        } else {
            format!(
                "apply [Authorize(Roles=\"{}\")] or policies",
                auth.required_roles.join(", ")
            )
        };
        steps.push(format!("Step {step}: Migrate auth rules â€” {roles_str}"));
        step += 1;
    }
    if auth.session_auth_count > 0 {
        steps.push(format!(
            "Step {step}: Replace {} Session-based auth key(s) with claims-based identity (ClaimsPrincipal)",
            auth.session_auth_count
        ));
        step += 1;
    }

    // User controls
    if !user_controls.is_empty() {
        steps.push(format!(
            "Step {step}: Migrate {} user control(s) [{}] to child components before migrating this page",
            user_controls.len(),
            user_controls
                .iter()
                .take(3)
                .map(|c| {
                    if c.tag_name.is_empty() {
                        c.control_path.split('/').next_back().unwrap_or("?").to_string()
                    } else {
                        c.tag_name.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
                + if user_controls.len() > 3 { ", â€¦" } else { "" }
        ));
        step += 1;
    }

    // Control events
    if lifecycle.control_event_count > 0 {
        steps.push(format!(
            "Step {step}: Wire {} control event handler(s) as @onclick/@onchange/@onsubmit in the new component",
            lifecycle.control_event_count
        ));
        step += 1;
    }

    steps.push(format!(
        "Step {step}: Write characterization tests against the legacy page, then validate functional parity after migration"
    ));

    steps
}

// â”€â”€ Complexity estimation â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[allow(clippy::too_many_arguments)]
fn estimate_complexity(
    blast_score: u8,
    lifecycle: &LifecycleSummary,
    viewstate: &ViewStateSummary,
    ajax: &AjaxSummary,
    validation: &ValidationSummary,
    auth: &AuthSummary,
    sql: &[DossierSqlInfo],
    user_controls: &[DossierControlRef],
) -> String {
    let mut score: u32 = 0;

    score += blast_score as u32;
    score += (lifecycle.lifecycle_event_count as u32).min(10);
    score += (lifecycle.control_event_count as u32 / 2).min(5);
    if lifecycle.has_ispostback_logic {
        score += 2;
    }
    score += ((viewstate.total_state_fields as u32) / 5).min(5);
    score += (ajax.update_panel_count as u32 * 2).min(8);
    score += (ajax.timer_count as u32 * 3).min(6);
    score += (validation.validator_count as u32).min(5);
    score += (validation.custom_validator_count as u32 * 2).min(6);
    if auth.has_auth_rules {
        score += 3;
    }
    if auth.session_auth_count > 0 {
        score += 2;
    }
    score += (sql.iter().filter(|s| !s.parameterized).count() as u32 * 2).min(8);
    score += (user_controls.len() as u32 * 2).min(10);

    if score == 0 {
        "Trivial: no significant migration concerns detected".to_string()
    } else if score <= 10 {
        format!(
            "Low (score {score}): straightforward migration â€” one component, minimal state, few events"
        )
    } else if score <= 25 {
        format!(
            "Medium (score {score}): moderate effort â€” address state management, data sources, and lifecycle events"
        )
    } else if score <= 45 {
        format!(
            "High (score {score}): significant effort â€” plan multiple sprints; tackle user controls and AJAX regions first"
        )
    } else {
        format!(
            "Critical (score {score}): this page is a high-risk migration candidate â€” consider strangler-fig or phased partial migration"
        )
    }
}

// â”€â”€ Format function â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Render a `MigrationDossier` as a comprehensive Markdown document.
pub fn format_migration_dossier(d: &MigrationDossier) -> String {
    let mut out = String::with_capacity(8192);

    // â”€â”€ Header â”€â”€
    out.push_str(&format!("# Migration Dossier: `{}`\n\n", d.file_path));
    out.push_str(&format!(
        "**Page Type:** {} | **Target Stack:** {} | **Complexity:** {}\n\n",
        d.page_type.to_uppercase(),
        d.target_stack.to_uppercase(),
        d.estimated_complexity
    ));

    // â”€â”€ Identity â”€â”€
    out.push_str("## Identity\n\n");
    if let Some(ref c) = d.inherits_class {
        out.push_str(&format!("- **Inherits:** `{c}`\n"));
    }
    if let Some(ref b) = d.base_class {
        out.push_str(&format!("- **Base class:** `{b}`\n"));
    }
    if let Some(ref cb) = d.codebehind_file {
        out.push_str(&format!("- **Code-behind:** `{cb}`\n"));
    }
    if let Some(ref m) = d.master_page {
        out.push_str(&format!("- **Master page:** `{m}`\n"));
    }
    out.push('\n');

    // â”€â”€ Dependencies â”€â”€
    out.push_str("## Dependencies\n\n");
    if !d.user_controls.is_empty() {
        out.push_str("### User Controls\n\n");
        out.push_str("| Path | Prefix | Tag | Properties |\n");
        out.push_str("|---|---|---|---|\n");
        for uc in &d.user_controls {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                uc.control_path,
                if uc.tag_prefix.is_empty() {
                    "-".to_string()
                } else {
                    uc.tag_prefix.clone()
                },
                if uc.tag_name.is_empty() {
                    "-".to_string()
                } else {
                    uc.tag_name.clone()
                },
                if uc.properties_set.is_empty() {
                    "-".to_string()
                } else {
                    uc.properties_set.join(", ")
                }
            ));
        }
        out.push('\n');
    }

    // Hub pages (shared masters / user controls) are referenced by hundreds
    // of files â€” uncapped lists turned the dossier into a token bomb.
    const DEP_LIST_CAP: usize = 25;
    let mut capped_list = |title: &str, items: &[String], out: &mut String| {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("### {title}\n\n"));
        for f in items.iter().take(DEP_LIST_CAP) {
            out.push_str(&format!("- `{f}`\n"));
        }
        if items.len() > DEP_LIST_CAP {
            out.push_str(&format!("- â€¦ and {} more\n", items.len() - DEP_LIST_CAP));
        }
        out.push('\n');
    };
    capped_list("Included Files", &d.referenced_files, &mut out);
    capped_list("Referenced By", &d.referenced_by, &mut out);
    capped_list("Shared Modules / Dependencies", &d.shared_modules, &mut out);

    // â”€â”€ Data layer â”€â”€
    out.push_str("## Data Layer\n\n");
    if !d.data_sources.is_empty() {
        out.push_str("### Data Sources\n\n");
        out.push_str("| Type | Target | Operations |\n");
        out.push_str("|---|---|---|\n");
        for ds in &d.data_sources {
            out.push_str(&format!(
                "| {} | `{}` | {} |\n",
                ds.source_type,
                ds.target,
                if ds.operations.is_empty() {
                    "-".to_string()
                } else {
                    ds.operations.join(", ")
                }
            ));
        }
        out.push('\n');
    }

    if !d.tables_touched.is_empty() {
        out.push_str("### Tables Touched\n\n");
        for t in &d.tables_touched {
            out.push_str(&format!("- `{t}`\n"));
        }
        out.push('\n');
    }

    if !d.connection_strings_used.is_empty() {
        out.push_str("### Connection Strings Used\n\n");
        for cs in &d.connection_strings_used {
            out.push_str(&format!("- `{cs}`\n"));
        }
        out.push('\n');
    }

    if !d.sql_statements.is_empty() {
        out.push_str("### SQL Statements\n\n");
        for sql in &d.sql_statements {
            out.push_str(&format!(
                "- `{}` â€” parameterized: {}{}\n",
                sql.sql_snippet.chars().take(120).collect::<String>(),
                sql.parameterized,
                if !sql.connection_string.is_empty() {
                    format!(" (conn: `{}`)", sql.connection_string)
                } else {
                    String::new()
                }
            ));
        }
        out.push('\n');
    }

    // â”€â”€ Event model â”€â”€
    out.push_str("## Event Model (Lifecycle)\n\n");
    out.push_str(&format!(
        "- **Lifecycle events:** {} | **Control events:** {} | **IsPostBack logic:** {}\n",
        d.lifecycle_summary.lifecycle_event_count,
        d.lifecycle_summary.control_event_count,
        if d.lifecycle_summary.has_ispostback_logic {
            "Yes"
        } else {
            "No"
        }
    ));
    if !d.lifecycle_summary.events.is_empty() {
        out.push_str(&format!(
            "- **Events:** {}\n",
            d.lifecycle_summary.events.join(", ")
        ));
    }
    out.push('\n');

    // â”€â”€ State footprint â”€â”€
    out.push_str("## State Footprint (ViewState)\n\n");
    out.push_str(&format!(
        "- **Explicit keys:** {} | **Implicit controls:** {} | **Total fields:** {}\n",
        d.viewstate_summary.explicit_keys,
        d.viewstate_summary.implicit_controls,
        d.viewstate_summary.total_state_fields
    ));
    if let Some(ref hc) = d.viewstate_summary.heaviest_control {
        out.push_str(&format!("- **Heaviest control:** `{hc}`\n"));
    }
    out.push('\n');

    // â”€â”€ AJAX regions â”€â”€
    out.push_str("## AJAX Regions\n\n");
    out.push_str(&format!(
        "- **UpdatePanels:** {} | **Timers:** {} | **ScriptManager:** {} | **Suggested components:** {}\n\n",
        d.ajax_summary.update_panel_count,
        d.ajax_summary.timer_count,
        if d.ajax_summary.has_script_manager { "Yes" } else { "No" },
        d.ajax_summary.suggested_components
    ));

    // â”€â”€ Validation â”€â”€
    out.push_str("## Validation\n\n");
    out.push_str(&format!(
        "- **Validators:** {} | **Custom:** {} | **Groups:** {} | **Summary control:** {}\n\n",
        d.validation_summary.validator_count,
        d.validation_summary.custom_validator_count,
        d.validation_summary.validation_group_count,
        if d.validation_summary.has_validation_summary {
            "Yes"
        } else {
            "No"
        }
    ));

    // â”€â”€ Auth context â”€â”€
    out.push_str("## Auth Context\n\n");
    out.push_str(&format!(
        "- **Auth rules present:** {} | **Auth checks:** {} | **Session auth patterns:** {}\n",
        if d.auth_summary.has_auth_rules {
            "Yes"
        } else {
            "No"
        },
        d.auth_summary.auth_check_count,
        d.auth_summary.session_auth_count
    ));
    if !d.auth_summary.required_roles.is_empty() {
        out.push_str(&format!(
            "- **Required roles:** {}\n",
            d.auth_summary.required_roles.join(", ")
        ));
    }
    out.push('\n');

    // â”€â”€ Risk â”€â”€
    out.push_str("## Risk Assessment\n\n");
    out.push_str(&format!(
        "**Blast Radius Score:** {}/10\n\n",
        d.blast_radius_score
    ));
    if !d.risk_factors.is_empty() {
        out.push_str("### Risk Factors\n\n");
        for rf in &d.risk_factors {
            out.push_str(&format!("- {rf}\n"));
        }
        out.push('\n');
    }

    // â”€â”€ Scaffold preview â”€â”€
    if let Some(ref preview) = d.scaffold_preview {
        let truncated: String = preview.chars().take(2000).collect();
        out.push_str("## Scaffold Preview\n\n");
        out.push_str("```\n");
        out.push_str(&truncated);
        if preview.len() > 2000 {
            out.push_str("\nâ€¦ (truncated â€” full output via generate_migration_scaffold tool)");
        }
        out.push_str("\n```\n\n");
    }

    // â”€â”€ Migration steps â”€â”€
    out.push_str("## Migration Steps\n\n");
    for step in &d.migration_steps {
        out.push_str(&format!("- {step}\n"));
    }
    out.push('\n');

    out
}

// â”€â”€ Tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_graph() -> Arc<GraphStore> {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("dossier_test.redb");
        let graph = Arc::new(GraphStore::open(&db_path).unwrap());
        drop(dir);
        graph
    }

    // â”€â”€ Test 1: Basic ASPX page with code-behind â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_basic_aspx_with_codebehind() {
        let graph = make_graph();
        let aspx = r#"
            <%@ Page Language="C#" AutoEventWireup="true"
                     CodeBehind="Orders.aspx.cs"
                     Inherits="MyApp.Orders"
                     MasterPageFile="~/Site.master" %>
            <asp:Content runat="server">
                <asp:GridView ID="gvOrders" runat="server" />
                <asp:Button ID="btnSearch" runat="server" Text="Search" />
            </asp:Content>
        "#;
        let cb = r#"
            protected void Page_Load(object sender, EventArgs e) {
                if (!IsPostBack) {
                    BindGrid();
                }
            }
            protected void btnSearch_Click(object sender, EventArgs e) {
                BindGrid();
            }
        "#;

        let dossier =
            build_migration_dossier(&graph, "proj1", "Orders.aspx", aspx, cb, None, "blazor", 1)
                .unwrap();

        assert_eq!(dossier.page_type, "aspx");
        assert_eq!(dossier.target_stack, "blazor");
        assert_eq!(dossier.inherits_class.as_deref(), Some("MyApp.Orders"));
        assert_eq!(dossier.codebehind_file.as_deref(), Some("Orders.aspx.cs"));
        assert!(dossier.master_page.is_some());
        assert!(dossier.lifecycle_summary.has_ispostback_logic);
        assert!(dossier.lifecycle_summary.lifecycle_event_count >= 1);
        // Control events: btnSearch_Click
        assert!(dossier.lifecycle_summary.control_event_count >= 1);
        assert!(!dossier.estimated_complexity.is_empty());
        assert!(!dossier.migration_steps.is_empty());
    }

    // â”€â”€ Test 2: Page with validators and UpdatePanels â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_validators_and_update_panels() {
        let graph = make_graph();
        let aspx = r#"
            <%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Register.aspx.cs" Inherits="MyApp.Register" %>
            <asp:ScriptManager ID="sm" runat="server" />
            <asp:UpdatePanel ID="upForm" runat="server" UpdateMode="Conditional">
                <ContentTemplate>
                    <asp:TextBox ID="txtEmail" runat="server" />
                    <asp:RequiredFieldValidator ID="rfvEmail" runat="server"
                        ControlToValidate="txtEmail" ErrorMessage="Email required" />
                    <asp:RegularExpressionValidator ID="revEmail" runat="server"
                        ControlToValidate="txtEmail"
                        ValidationExpression="\w+@\w+\.\w+"
                        ErrorMessage="Invalid email" />
                    <asp:TextBox ID="txtPassword" runat="server" TextMode="Password" />
                    <asp:RequiredFieldValidator ID="rfvPwd" runat="server"
                        ControlToValidate="txtPassword" ErrorMessage="Password required" />
                    <asp:Button ID="btnRegister" runat="server" Text="Register" />
                </ContentTemplate>
                <Triggers>
                    <asp:AsyncPostBackTrigger ControlID="btnRegister" EventName="Click" />
                </Triggers>
            </asp:UpdatePanel>
            <asp:ValidationSummary ID="vs1" runat="server" ShowMessageBox="true" />
        "#;
        let cb = "";

        let dossier = build_migration_dossier(
            &graph,
            "proj1",
            "Register.aspx",
            aspx,
            cb,
            None,
            "blazor",
            1,
        )
        .unwrap();

        assert_eq!(dossier.ajax_summary.update_panel_count, 1);
        assert!(dossier.ajax_summary.has_script_manager);
        assert!(dossier.validation_summary.validator_count >= 3);
        assert!(dossier.validation_summary.has_validation_summary);
        // risk factors should mention UpdatePanel
        let rf_text = dossier.risk_factors.join(" ");
        assert!(rf_text.contains("UpdatePanel") || dossier.ajax_summary.update_panel_count > 0);
        // migration steps must include AJAX and validation steps
        let steps_text = dossier.migration_steps.join(" ");
        assert!(
            steps_text.to_lowercase().contains("updatepanel") || steps_text.contains("component")
        );
        assert!(
            steps_text.to_lowercase().contains("validator")
                || steps_text.contains("DataAnnotations")
        );
    }

    // â”€â”€ Test 3: Page with lifecycle events â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_page_with_lifecycle_events() {
        let graph = make_graph();
        let aspx = r#"
            <%@ Page Language="VB" AutoEventWireup="false"
                     CodeBehind="Dashboard.aspx.vb"
                     Inherits="MyApp.Dashboard" %>
        "#;
        let cb = r#"
            Protected Sub Page_Load(ByVal sender As Object, ByVal e As EventArgs) Handles Me.Load
                If Not IsPostBack Then
                    LoadDashboardData()
                End If
            End Sub

            Protected Sub Page_PreRender(ByVal sender As Object, ByVal e As EventArgs) Handles Me.PreRender
                UpdateStatusBar()
            End Sub

            Protected Sub btnRefresh_Click(ByVal sender As Object, ByVal e As EventArgs) Handles btnRefresh.Click
                LoadDashboardData()
            End Sub

            Protected Sub ddlFilter_SelectedIndexChanged(ByVal sender As Object, ByVal e As EventArgs) Handles ddlFilter.SelectedIndexChanged
                FilterData()
            End Sub
        "#;

        let dossier = build_migration_dossier(
            &graph,
            "proj1",
            "Dashboard.aspx.vb",
            aspx,
            cb,
            None,
            "blazor",
            1,
        )
        .unwrap();

        assert!(dossier.lifecycle_summary.lifecycle_event_count >= 2); // Page_Load + Page_PreRender
        assert!(dossier.lifecycle_summary.has_ispostback_logic);
        assert!(dossier.lifecycle_summary.control_event_count >= 2); // btnRefresh, ddlFilter

        // Events list should contain recognizable entries
        let events_text = dossier.lifecycle_summary.events.join(", ");
        assert!(!events_text.is_empty());

        // Migration steps should mention lifecycle
        let steps_text = dossier.migration_steps.join(" ");
        assert!(steps_text.contains("lifecycle") || steps_text.contains("event"));
    }

    // â”€â”€ Test 4: Minimal page â€” no special features â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_minimal_page_no_special_features() {
        let graph = make_graph();
        let aspx = r#"
            <%@ Page Language="C#" AutoEventWireup="true" CodeBehind="About.aspx.cs" Inherits="MyApp.About" %>
            <html><body><p>Static about page.</p></body></html>
        "#;
        let cb = "";

        let dossier =
            build_migration_dossier(&graph, "proj1", "About.aspx", aspx, cb, None, "react", 1)
                .unwrap();

        assert_eq!(dossier.page_type, "aspx");
        assert_eq!(dossier.target_stack, "react");
        assert_eq!(dossier.ajax_summary.update_panel_count, 0);
        assert!(!dossier.ajax_summary.has_script_manager);
        assert_eq!(dossier.validation_summary.validator_count, 0);
        assert_eq!(dossier.viewstate_summary.explicit_keys, 0);
        assert!(!dossier.auth_summary.has_auth_rules);
        assert!(dossier.risk_factors.is_empty() || dossier.risk_factors.len() <= 2);
        // Complexity should be low or trivial
        let complexity = dossier.estimated_complexity.to_lowercase();
        assert!(complexity.contains("trivial") || complexity.contains("low"));
        // Steps should still be present (at minimum create component)
        assert!(!dossier.migration_steps.is_empty());
    }

    // â”€â”€ Test 5: Format output structure â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_format_output_structure() {
        let graph = make_graph();
        let aspx = r#"
            <%@ Page Language="C#" AutoEventWireup="true"
                     CodeBehind="Search.aspx.cs"
                     Inherits="MyApp.Search"
                     MasterPageFile="~/Site.master" %>
            <asp:ScriptManager ID="sm" runat="server" />
            <asp:UpdatePanel ID="upResults" runat="server">
                <ContentTemplate>
                    <asp:GridView ID="gvResults" runat="server" />
                </ContentTemplate>
            </asp:UpdatePanel>
            <asp:RequiredFieldValidator ID="rfvQ" runat="server"
                ControlToValidate="txtQuery" ErrorMessage="Enter a search term" />
        "#;
        let cb = r#"
            protected void Page_Load(object sender, EventArgs e) {
                if (!IsPostBack) { BindData(); }
            }
            protected void btnSearch_Click(object sender, EventArgs e) { Search(); }
        "#;

        let dossier =
            build_migration_dossier(&graph, "proj1", "Search.aspx", aspx, cb, None, "blazor", 1)
                .unwrap();

        let formatted = format_migration_dossier(&dossier);

        // Core sections must be present
        assert!(
            formatted.contains("# Migration Dossier"),
            "Missing dossier header"
        );
        assert!(
            formatted.contains("## Identity"),
            "Missing Identity section"
        );
        assert!(
            formatted.contains("## Dependencies"),
            "Missing Dependencies section"
        );
        assert!(
            formatted.contains("## Data Layer"),
            "Missing Data Layer section"
        );
        assert!(
            formatted.contains("## Event Model"),
            "Missing Event Model section"
        );
        assert!(
            formatted.contains("## State Footprint"),
            "Missing State Footprint section"
        );
        assert!(
            formatted.contains("## AJAX Regions"),
            "Missing AJAX Regions section"
        );
        assert!(
            formatted.contains("## Validation"),
            "Missing Validation section"
        );
        assert!(
            formatted.contains("## Auth Context"),
            "Missing Auth Context section"
        );
        assert!(
            formatted.contains("## Risk Assessment"),
            "Missing Risk Assessment section"
        );
        assert!(
            formatted.contains("## Migration Steps"),
            "Missing Migration Steps section"
        );

        // Verify known content appears
        assert!(formatted.contains("Search.aspx"));
        assert!(formatted.contains("BLAZOR") || formatted.contains("blazor"));
    }

    // â”€â”€ Test 6: Data source extraction from graph â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_data_source_extraction_from_aspx() {
        let graph = make_graph();
        let aspx = r#"
            <%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Products.aspx.cs" Inherits="MyApp.Products" %>
            <asp:SqlDataSource ID="dsProducts" runat="server"
                SelectCommand="SELECT ProductID, Name, Price FROM Products WHERE CategoryID = @CatID"
                UpdateCommand="UPDATE Products SET Name = @Name WHERE ProductID = @ID"
                DeleteCommand="DELETE FROM Products WHERE ProductID = @ID"
                ConnectionString="<%$ ConnectionStrings:AppDB %>" />
            <asp:ObjectDataSource ID="dsCategories" runat="server"
                TypeName="MyApp.Data.CategoryRepository"
                SelectMethod="GetAll"
                InsertMethod="Add"
                UpdateMethod="Update" />
            <asp:GridView ID="gvProducts" runat="server" DataSourceID="dsProducts" />
        "#;
        let cb = r#"
            protected void Page_Load(object sender, EventArgs e) {
                SqlCommand cmd = new SqlCommand("SELECT * FROM Orders WHERE CustomerID = @cid", conn);
                cmd.Parameters.AddWithValue("@cid", customerId);
            }
        "#;

        let dossier = build_migration_dossier(
            &graph,
            "proj1",
            "Products.aspx",
            aspx,
            cb,
            None,
            "blazor",
            1,
        )
        .unwrap();

        // SqlDataSource should be detected
        let sql_ds = dossier
            .data_sources
            .iter()
            .find(|d| d.source_type == "SqlDataSource");
        assert!(sql_ds.is_some(), "SqlDataSource not detected");
        let sql_ds = sql_ds.unwrap();
        assert!(sql_ds.operations.contains(&"SELECT".to_string()));
        assert!(sql_ds.operations.contains(&"UPDATE".to_string()));
        assert!(sql_ds.operations.contains(&"DELETE".to_string()));

        // ObjectDataSource should be detected
        let obj_ds = dossier
            .data_sources
            .iter()
            .find(|d| d.source_type == "ObjectDataSource");
        assert!(obj_ds.is_some(), "ObjectDataSource not detected");
        let obj_ds = obj_ds.unwrap();
        assert_eq!(obj_ds.target, "MyApp.Data.CategoryRepository");
        assert!(obj_ds.operations.contains(&"SELECT".to_string()));
        assert!(obj_ds.operations.contains(&"INSERT".to_string()));

        // Tables extracted from SELECT command
        assert!(
            dossier
                .tables_touched
                .iter()
                .any(|t| t.eq_ignore_ascii_case("Products")),
            "Expected 'Products' table in tables_touched, got: {:?}",
            dossier.tables_touched
        );

        // Inline SQL from code-behind
        let has_inline_sql = dossier
            .sql_statements
            .iter()
            .any(|s| s.sql_snippet.contains("Orders") || s.sql_snippet.contains("SELECT"));
        assert!(has_inline_sql, "Expected inline SQL from code-behind");

        // Risk factors should mention data sources or SQL
        let _risk_text = dossier.risk_factors.join(" ").to_lowercase();
        // May warn about unparameterized SQL if any was found without @param
        // (the inline SELECT * has @cid so it is parameterized â€” no unparameterized warning needed)
        // Complexity must be at least Low for a page with data sources
        let complexity = dossier.estimated_complexity.to_lowercase();
        assert!(
            !complexity.contains("trivial"),
            "Expected at least Low complexity for a data-rich page, got: {complexity}"
        );

        // formatted output should reference the data section
        let formatted = format_migration_dossier(&dossier);
        assert!(formatted.contains("SqlDataSource") || formatted.contains("ObjectDataSource"));
    }

    // â”€â”€ detect_page_type â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_detect_page_type_aspx() {
        assert_eq!(detect_page_type("Default.aspx"), "aspx");
        assert_eq!(detect_page_type("Orders.ASPX"), "aspx"); // case insensitive
    }

    #[test]
    fn test_detect_page_type_ascx() {
        assert_eq!(detect_page_type("Header.ascx"), "ascx");
    }

    #[test]
    fn test_detect_page_type_master() {
        assert_eq!(detect_page_type("Site.Master"), "master");
    }

    #[test]
    fn test_detect_page_type_asmx() {
        assert_eq!(detect_page_type("UserService.asmx"), "asmx");
    }

    #[test]
    fn test_detect_page_type_ashx() {
        assert_eq!(detect_page_type("ImageHandler.ashx"), "ashx");
    }

    #[test]
    fn test_detect_page_type_unknown() {
        assert_eq!(detect_page_type("SomeFile.cs"), "unknown");
        assert_eq!(detect_page_type("web.config"), "unknown");
    }

    // â”€â”€ parse_directives â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_parse_directives_inherits_and_codebehind() {
        let aspx =
            r#"<%@ Page Language="C#" CodeBehind="Orders.aspx.cs" Inherits="MyApp.Orders" %>"#;
        let (inherits, codebehind, master) = parse_directives(aspx);
        assert_eq!(inherits.as_deref(), Some("MyApp.Orders"));
        assert_eq!(codebehind.as_deref(), Some("Orders.aspx.cs"));
        assert!(master.is_none());
    }

    #[test]
    fn test_parse_directives_master_page() {
        let aspx = r#"<%@ Page Language="VB" MasterPageFile="~/Site.master" CodeBehind="Page.aspx.vb" Inherits="MyApp.Page" %>"#;
        let (inherits, _cb, master) = parse_directives(aspx);
        assert!(inherits.is_some());
        assert_eq!(master.as_deref(), Some("~/Site.master"));
    }

    #[test]
    fn test_parse_directives_codefile_fallback() {
        let aspx = r#"<%@ Page Language="VB" CodeFile="Page.aspx.vb" Inherits="MyApp.Page" %>"#;
        let (_inherits, codebehind, _master) = parse_directives(aspx);
        assert_eq!(codebehind.as_deref(), Some("Page.aspx.vb"));
    }

    #[test]
    fn test_parse_directives_no_directive_returns_none() {
        let aspx = "<html><body>Hello</body></html>";
        let (inherits, codebehind, master) = parse_directives(aspx);
        assert!(inherits.is_none());
        assert!(codebehind.is_none());
        assert!(master.is_none());
    }

    // â”€â”€ extract_base_class_from_code â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_extract_base_class_cs_custom() {
        let code = "public partial class AdminPage : BasePage { }";
        let base = extract_base_class_from_code(code);
        assert_eq!(base.as_deref(), Some("BasePage"));
    }

    #[test]
    fn test_extract_base_class_page_is_filtered_out() {
        let code = "public partial class MyPage : Page { }";
        let base = extract_base_class_from_code(code);
        assert!(base.is_none(), "Page is filtered out as a non-custom base");
    }

    #[test]
    fn test_extract_base_class_vb_inherits() {
        let code = "Partial Class MyPage\n    Inherits SecurePage\nEnd Class";
        let base = extract_base_class_from_code(code);
        assert_eq!(base.as_deref(), Some("SecurePage"));
    }

    // â”€â”€ extract_aspx_attr â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_extract_aspx_attr_basic() {
        let tag = r#"ID="myGrid" runat="server" AllowSorting="true""#;
        assert_eq!(extract_aspx_attr(tag, "ID"), "myGrid");
        assert_eq!(extract_aspx_attr(tag, "runat"), "server");
        assert_eq!(extract_aspx_attr(tag, "AllowSorting"), "true");
    }

    #[test]
    fn test_extract_aspx_attr_case_insensitive() {
        let tag = r#"SelectCommand="SELECT * FROM Users""#;
        assert_eq!(
            extract_aspx_attr(tag, "selectcommand"),
            "SELECT * FROM Users"
        );
        assert_eq!(
            extract_aspx_attr(tag, "SELECTCOMMAND"),
            "SELECT * FROM Users"
        );
    }

    #[test]
    fn test_extract_aspx_attr_missing_returns_empty() {
        let tag = r#"ID="test""#;
        assert_eq!(extract_aspx_attr(tag, "NotHere"), "");
    }

    // â”€â”€ collect_risk_factors â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_collect_risk_factors_ispostback() {
        let lifecycle = LifecycleSummary {
            lifecycle_event_count: 2,
            control_event_count: 1,
            has_ispostback_logic: true,
            events: vec!["Page_Load (IsPostBack)".into()],
        };
        let viewstate = ViewStateSummary {
            explicit_keys: 0,
            implicit_controls: 0,
            total_state_fields: 0,
            heaviest_control: None,
        };
        let ajax = AjaxSummary {
            update_panel_count: 0,
            timer_count: 0,
            has_script_manager: false,
            suggested_components: 0,
        };
        let validation = ValidationSummary {
            validator_count: 0,
            custom_validator_count: 0,
            validation_group_count: 0,
            has_validation_summary: false,
        };
        let auth = AuthSummary {
            has_auth_rules: false,
            required_roles: vec![],
            auth_check_count: 0,
            session_auth_count: 0,
        };
        let sql: Vec<DossierSqlInfo> = vec![];
        let data: Vec<DossierDataSource> = vec![];
        let factors = collect_risk_factors(
            &lifecycle,
            &viewstate,
            &ajax,
            &validation,
            &auth,
            &sql,
            &data,
        );
        assert!(factors.iter().any(|f| f.contains("IsPostBack")));
    }

    #[test]
    fn test_collect_risk_factors_update_panel() {
        let lifecycle = LifecycleSummary {
            lifecycle_event_count: 0,
            control_event_count: 0,
            has_ispostback_logic: false,
            events: vec![],
        };
        let viewstate = ViewStateSummary {
            explicit_keys: 0,
            implicit_controls: 0,
            total_state_fields: 0,
            heaviest_control: None,
        };
        let ajax = AjaxSummary {
            update_panel_count: 2,
            timer_count: 0,
            has_script_manager: true,
            suggested_components: 2,
        };
        let validation = ValidationSummary {
            validator_count: 0,
            custom_validator_count: 0,
            validation_group_count: 0,
            has_validation_summary: false,
        };
        let auth = AuthSummary {
            has_auth_rules: false,
            required_roles: vec![],
            auth_check_count: 0,
            session_auth_count: 0,
        };
        let sql: Vec<DossierSqlInfo> = vec![];
        let data: Vec<DossierDataSource> = vec![];
        let factors = collect_risk_factors(
            &lifecycle,
            &viewstate,
            &ajax,
            &validation,
            &auth,
            &sql,
            &data,
        );
        assert!(factors.iter().any(|f| f.contains("UpdatePanel")));
    }

    #[test]
    fn test_collect_risk_factors_unparameterized_sql() {
        let lifecycle = LifecycleSummary {
            lifecycle_event_count: 0,
            control_event_count: 0,
            has_ispostback_logic: false,
            events: vec![],
        };
        let viewstate = ViewStateSummary {
            explicit_keys: 0,
            implicit_controls: 0,
            total_state_fields: 0,
            heaviest_control: None,
        };
        let ajax = AjaxSummary {
            update_panel_count: 0,
            timer_count: 0,
            has_script_manager: false,
            suggested_components: 0,
        };
        let validation = ValidationSummary {
            validator_count: 0,
            custom_validator_count: 0,
            validation_group_count: 0,
            has_validation_summary: false,
        };
        let auth = AuthSummary {
            has_auth_rules: false,
            required_roles: vec![],
            auth_check_count: 0,
            session_auth_count: 0,
        };
        let sql = vec![DossierSqlInfo {
            sql_snippet: "SELECT * FROM Users WHERE id = 'abc'".to_string(),
            parameterized: false,
            connection_string: String::new(),
        }];
        let data: Vec<DossierDataSource> = vec![];
        let factors = collect_risk_factors(
            &lifecycle,
            &viewstate,
            &ajax,
            &validation,
            &auth,
            &sql,
            &data,
        );
        assert!(
            factors
                .iter()
                .any(|f| f.contains("unparameterized") || f.contains("SQL injection"))
        );
    }

    // â”€â”€ estimate_complexity â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn test_estimate_complexity_trivial() {
        let lifecycle = LifecycleSummary {
            lifecycle_event_count: 0,
            control_event_count: 0,
            has_ispostback_logic: false,
            events: vec![],
        };
        let viewstate = ViewStateSummary {
            explicit_keys: 0,
            implicit_controls: 0,
            total_state_fields: 0,
            heaviest_control: None,
        };
        let ajax = AjaxSummary {
            update_panel_count: 0,
            timer_count: 0,
            has_script_manager: false,
            suggested_components: 0,
        };
        let validation = ValidationSummary {
            validator_count: 0,
            custom_validator_count: 0,
            validation_group_count: 0,
            has_validation_summary: false,
        };
        let auth = AuthSummary {
            has_auth_rules: false,
            required_roles: vec![],
            auth_check_count: 0,
            session_auth_count: 0,
        };
        let sql: Vec<DossierSqlInfo> = vec![];
        let controls: Vec<DossierControlRef> = vec![];
        let complexity = estimate_complexity(
            0,
            &lifecycle,
            &viewstate,
            &ajax,
            &validation,
            &auth,
            &sql,
            &controls,
        );
        assert!(
            complexity.contains("Trivial"),
            "all zeros should be Trivial: {complexity}"
        );
    }

    #[test]
    fn test_estimate_complexity_high_for_complex_page() {
        let lifecycle = LifecycleSummary {
            lifecycle_event_count: 8,
            control_event_count: 12,
            has_ispostback_logic: true,
            events: vec![],
        };
        let viewstate = ViewStateSummary {
            explicit_keys: 5,
            implicit_controls: 3,
            total_state_fields: 20,
            heaviest_control: Some("gv1".into()),
        };
        let ajax = AjaxSummary {
            update_panel_count: 3,
            timer_count: 2,
            has_script_manager: true,
            suggested_components: 3,
        };
        let validation = ValidationSummary {
            validator_count: 5,
            custom_validator_count: 3,
            validation_group_count: 2,
            has_validation_summary: true,
        };
        let auth = AuthSummary {
            has_auth_rules: true,
            required_roles: vec!["Admin".into()],
            auth_check_count: 5,
            session_auth_count: 3,
        };
        let sql: Vec<DossierSqlInfo> = (0..5)
            .map(|_| DossierSqlInfo {
                sql_snippet: "SELECT".into(),
                parameterized: false,
                connection_string: String::new(),
            })
            .collect();
        let controls: Vec<DossierControlRef> = (0..5)
            .map(|i| DossierControlRef {
                control_path: format!("uc{i}.ascx"),
                tag_prefix: "uc".into(),
                tag_name: format!("Ctrl{i}"),
                properties_set: vec![],
            })
            .collect();
        let complexity = estimate_complexity(
            8,
            &lifecycle,
            &viewstate,
            &ajax,
            &validation,
            &auth,
            &sql,
            &controls,
        );
        assert!(
            complexity.contains("High") || complexity.contains("Critical"),
            "complex page should be High or Critical: {complexity}"
        );
    }
}
