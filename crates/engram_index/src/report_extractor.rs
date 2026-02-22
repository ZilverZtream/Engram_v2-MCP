/// Report file extractor for SSRS (.rdl, .rdlc) and Crystal Reports detection.
///
/// **SSRS Report Extraction** (`extract_ssrs_report`):
///   - `<DataSource>` + `<ConnectionString>` → `connection_string` symbol
///   - `<CommandText>` SQL → `sql_calls` edges, `queries_table` edges for table references
///   - `<ReportParameter>` → metadata on file-level `report` node
///   - `<Field>` + `<DataField>` → `reads_column` edges
///   - `<Subreport>` + `<ReportName>` → `dependency` edges
///   - File-level `insight` node with migration guidance
///
/// **Crystal Reports Detection** (`extract_crystal_reports_usage`):
///   - `CrystalDecisions.CrystalReports.Engine` namespace usage
///   - `ReportDocument` class instantiation and `.Load("path.rpt")` calls
///   - `SetDataSource` / `CrystalReportViewer` usage
///   - `anti_pattern` edge: binary .rpt files are migration blockers
///
/// **Crystal Reports in Markup** (`extract_crystal_reports_in_markup`):
///   - `<CR:CrystalReportViewer>` / `<CrystalDecisions:...>` tag prefixes
///
/// Uses regex-based XML extraction (no xml crate dependency) since SSRS report
/// XML has a predictable structure suitable for pattern matching.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

// SSRS patterns
static DATASOURCE_RE: OnceLock<Regex> = OnceLock::new();
static CONN_STRING_RE: OnceLock<Regex> = OnceLock::new();
static COMMAND_TEXT_RE: OnceLock<Regex> = OnceLock::new();
static REPORT_PARAM_RE: OnceLock<Regex> = OnceLock::new();
static DATA_FIELD_RE: OnceLock<Regex> = OnceLock::new();
static DATASET_RE: OnceLock<Regex> = OnceLock::new();
static SUBREPORT_NAME_RE: OnceLock<Regex> = OnceLock::new();
static SQL_TABLE_RE: OnceLock<Regex> = OnceLock::new();

// Crystal Reports patterns (code-behind)
static CR_NAMESPACE_RE: OnceLock<Regex> = OnceLock::new();
static CR_REPORT_DOC_NEW_RE: OnceLock<Regex> = OnceLock::new();
static CR_LOAD_RE: OnceLock<Regex> = OnceLock::new();
static CR_SET_DATASOURCE_RE: OnceLock<Regex> = OnceLock::new();
static CR_VIEWER_CODE_RE: OnceLock<Regex> = OnceLock::new();

// Crystal Reports patterns (markup)
static CR_VIEWER_MARKUP_RE: OnceLock<Regex> = OnceLock::new();
static CR_TAG_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
static CR_REPORT_SOURCE_RE: OnceLock<Regex> = OnceLock::new();

fn get_compiled_regex<'a>(
    lock: &'a OnceLock<Regex>,
    pattern: &str,
    label: &str,
) -> Option<&'a Regex> {
    if let Some(re) = lock.get() {
        return Some(re);
    }
    match Regex::new(pattern) {
        Ok(re) => Some(lock.get_or_init(|| re)),
        Err(err) => {
            tracing::error!("failed to compile {label} regex: {err}");
            None
        }
    }
}

// ── Line-number helper ──────────────────────────────────────────────────────

/// Build byte-offset → line-start index for O(log n) line lookups.
fn build_line_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (i, c) in source.char_indices() {
        if c == '\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// Convert a byte offset to a 0-based line number.
fn offset_to_line(offsets: &[usize], byte_pos: usize) -> u32 {
    match offsets.binary_search(&byte_pos) {
        Ok(line) => line as u32,
        Err(line) => line.saturating_sub(1) as u32,
    }
}

// ── Helper: extract filename stem ───────────────────────────────────────────

/// Get the filename without directory from a RelPath.
fn file_stem(rel_path: &RelPath) -> String {
    rel_path
        .file_name()
        .unwrap_or(rel_path.as_str())
        .to_string()
}

// ── Helper: extract table names from SQL ────────────────────────────────────

/// Parse simple SQL to extract table names referenced in FROM / JOIN clauses.
///
/// Handles:
///   - `FROM TableName`
///   - `FROM [dbo].[TableName]`
///   - `JOIN TableName`
///   - `INSERT INTO TableName`
///   - `UPDATE TableName`
fn extract_table_names_from_sql(sql: &str) -> Vec<String> {
    let re = match get_compiled_regex(
        &SQL_TABLE_RE,
        r"(?i)(?:FROM|JOIN|INTO|UPDATE)\s+(?:\[?\w+\]?\.)?(\[?\w+\]?)",
        "sql_table_ref",
    ) {
        Some(r) => r,
        None => return Vec::new(),
    };

    let mut tables = Vec::new();
    for cap in re.captures_iter(sql) {
        if let Some(m) = cap.get(1) {
            let name = strip_brackets(m.as_str());
            // Skip SQL keywords that can appear after FROM/JOIN in subqueries.
            let lower = name.to_lowercase();
            if matches!(
                lower.as_str(),
                "select" | "where" | "set" | "values" | "as" | "on" | "and" | "or"
            ) {
                continue;
            }
            if !name.is_empty() && !tables.contains(&name.to_string()) {
                tables.push(name.to_string());
            }
        }
    }
    tables
}

/// Strip surrounding brackets `[Name]` → `Name`.
fn strip_brackets(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with('[') && s.ends_with(']') && s.len() >= 2 {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. SSRS Report Extractor
// ═══════════════════════════════════════════════════════════════════════════

/// Extract symbols and edges from an SSRS report file (`.rdl` / `.rdlc`).
///
/// Returns `(symbols, edges)` where:
///   - symbols include the file-level `report` node, `connection_string` nodes,
///     and an `insight` node with migration guidance
///   - edges include `sql_calls`, `queries_table`, `reads_column`, `dependency`,
///     and `contains` relationships
pub fn extract_ssrs_report(
    rel_path: &RelPath,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    let line_offsets = build_line_offsets(source);
    let file_name = file_stem(rel_path);
    let file_path = rel_path.as_str().to_string();

    // Counters for the file-level report metadata.
    let mut parameter_count: u32 = 0;
    let mut dataset_count: u32 = 0;
    let mut has_subreports = false;

    // ── Data sources ────────────────────────────────────────────────────────
    extract_ssrs_data_sources(source, &line_offsets, &file_path, &mut symbols, &mut edges);

    // ── Command text (SQL queries in datasets) ──────────────────────────────
    extract_ssrs_command_text(source, &line_offsets, &file_path, &file_name, &mut edges);

    // ── Dataset count ───────────────────────────────────────────────────────
    if let Some(ds_re) = get_compiled_regex(
        &DATASET_RE,
        r#"(?i)<DataSet\b[^>]*\bName\s*=\s*["']([^"']+)["']"#,
        "ssrs_dataset",
    ) {
        dataset_count = ds_re.captures_iter(source).count() as u32;
    }

    // ── Report parameters ───────────────────────────────────────────────────
    if let Some(param_re) = get_compiled_regex(
        &REPORT_PARAM_RE,
        r#"(?i)<ReportParameter\b[^>]*\bName\s*=\s*["']([^"']+)["']"#,
        "ssrs_report_param",
    ) {
        let params: Vec<String> = param_re
            .captures_iter(source)
            .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .collect();
        parameter_count = params.len() as u32;
    }

    // ── Dataset field columns ───────────────────────────────────────────────
    extract_ssrs_data_fields(source, &line_offsets, &file_path, &file_name, &mut edges);

    // ── Subreport references ────────────────────────────────────────────────
    if let Some(sub_re) = get_compiled_regex(
        &SUBREPORT_NAME_RE,
        r"(?i)<ReportName>\s*([^<]+?)\s*</ReportName>",
        "ssrs_subreport_name",
    ) {
        for cap in sub_re.captures_iter(source) {
            if let Some(m) = cap.get(1) {
                has_subreports = true;
                let subreport_name = m.as_str().trim().to_string();
                let line = offset_to_line(&line_offsets, m.start());

                edges.push(ExtractedEdge {
                    source_name: file_path.clone(),
                    source_kind: "file",
                    source_start_line: line,
                    source_language: "xml",
                    target_name: subreport_name.clone(),
                    target_kind: Some("file"),
                    target_start_line: None,
                    kind: "dependency",
                    metadata: Some(HashMap::from([
                        ("relationship".into(), "subreport".into()),
                        ("subreport_name".into(), subreport_name),
                    ])),
                });
            }
        }
    }

    // ── File-level report symbol ────────────────────────────────────────────
    let mut report_meta = HashMap::new();
    report_meta.insert("report_type".into(), "ssrs".into());
    report_meta.insert("parameter_count".into(), parameter_count.to_string());
    report_meta.insert("dataset_count".into(), dataset_count.to_string());
    report_meta.insert("has_subreports".into(), has_subreports.to_string());
    report_meta.insert("file".into(), file_path.clone());

    symbols.push(ExtractedSymbol {
        name: file_name.clone(),
        kind: "report",
        start_line: 0,
        end_line: source.lines().count().saturating_sub(1) as u32,
        metadata: Some(report_meta),
    });

    // ── Insight node with migration guidance ────────────────────────────────
    let mut insight_meta = HashMap::new();
    insight_meta.insert("report_type".into(), "ssrs".into());
    insight_meta.insert(
        "modern_equivalent".into(),
        "SSRS on SQL Server, DevExpress Reports, or Telerik Reporting".into(),
    );
    insight_meta.insert("parameter_count".into(), parameter_count.to_string());
    insight_meta.insert("dataset_count".into(), dataset_count.to_string());
    insight_meta.insert("has_subreports".into(), has_subreports.to_string());

    symbols.push(ExtractedSymbol {
        name: format!("ssrs_report:{file_name}"),
        kind: "insight",
        start_line: 0,
        end_line: 0,
        metadata: Some(insight_meta),
    });

    (symbols, edges)
}

/// Extract `<DataSource>` elements with `<ConnectionString>` from SSRS XML.
fn extract_ssrs_data_sources(
    source: &str,
    line_offsets: &[usize],
    file_path: &str,
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) {
    // Match <DataSource Name="..."> blocks.
    let ds_re = match get_compiled_regex(
        &DATASOURCE_RE,
        r#"(?is)<DataSource\b[^>]*\bName\s*=\s*["']([^"']+)["'][^>]*>(.*?)</DataSource>"#,
        "ssrs_datasource",
    ) {
        Some(r) => r,
        None => return,
    };

    let conn_re = match get_compiled_regex(
        &CONN_STRING_RE,
        r"(?is)<ConnectionString>\s*(.*?)\s*</ConnectionString>",
        "ssrs_conn_string",
    ) {
        Some(r) => r,
        None => return,
    };

    for ds_cap in ds_re.captures_iter(source) {
        let ds_name = ds_cap.get(1).map_or("", |m| m.as_str()).to_string();
        let ds_body = ds_cap.get(2).map_or("", |m| m.as_str());
        let ds_start = ds_cap.get(0).map_or(0, |m| m.start());
        let line = offset_to_line(line_offsets, ds_start);

        // Extract connection string from this data source block.
        if let Some(conn_cap) = conn_re.captures(ds_body) {
            let conn_value = conn_cap.get(1).map_or("", |m| m.as_str()).trim();
            // Redact actual connection string values — store only the data source name.
            let mut meta = HashMap::new();
            meta.insert("data_source_name".into(), ds_name.clone());
            meta.insert("file".into(), file_path.to_string());
            if !conn_value.is_empty() {
                // Store a sanitized hint (just "present") — do not log secrets.
                meta.insert("connection_string_present".into(), "true".into());
            }

            symbols.push(ExtractedSymbol {
                name: format!("datasource:{ds_name}"),
                kind: "connection_string",
                start_line: line,
                end_line: line,
                metadata: Some(meta),
            });

            // Edge: report file contains this data source.
            edges.push(ExtractedEdge {
                source_name: file_path.to_string(),
                source_kind: "file",
                source_start_line: line,
                source_language: "xml",
                target_name: format!("datasource:{ds_name}"),
                target_kind: Some("connection_string"),
                target_start_line: Some(line),
                kind: "contains",
                metadata: None,
            });
        }
    }
}

/// Extract `<CommandText>` SQL queries and derive `sql_calls` + `queries_table` edges.
fn extract_ssrs_command_text(
    source: &str,
    line_offsets: &[usize],
    file_path: &str,
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let cmd_re = match get_compiled_regex(
        &COMMAND_TEXT_RE,
        r"(?is)<CommandText>\s*(.*?)\s*</CommandText>",
        "ssrs_command_text",
    ) {
        Some(r) => r,
        None => return,
    };

    for (idx, cap) in cmd_re.captures_iter(source).enumerate() {
        let sql_text = cap.get(1).map_or("", |m| m.as_str()).trim();
        if sql_text.is_empty() {
            continue;
        }
        let match_start = cap.get(0).map_or(0, |m| m.start());
        let line = offset_to_line(line_offsets, match_start);

        // Truncate SQL text in metadata to prevent excessively large graph nodes.
        let sql_preview = if sql_text.len() > 500 {
            format!("{}...", &sql_text[..500])
        } else {
            sql_text.to_string()
        };

        // sql_calls edge: report file → sql query.
        let query_name = format!("ssrs_query:{file_name}:{idx}");
        let mut sql_meta = HashMap::new();
        sql_meta.insert("sql".into(), sql_preview);
        sql_meta.insert("query_index".into(), idx.to_string());

        edges.push(ExtractedEdge {
            source_name: file_path.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "xml",
            target_name: query_name,
            target_kind: Some("function"),
            target_start_line: None,
            kind: "sql_calls",
            metadata: Some(sql_meta),
        });

        // queries_table edges: one per table referenced in the SQL.
        for table_name in extract_table_names_from_sql(sql_text) {
            edges.push(ExtractedEdge {
                source_name: file_path.to_string(),
                source_kind: "file",
                source_start_line: line,
                source_language: "xml",
                target_name: table_name.to_lowercase(),
                target_kind: Some("db_table"),
                target_start_line: None,
                kind: "queries_table",
                metadata: Some(HashMap::from([(
                    "source_report".into(),
                    file_name.to_string(),
                )])),
            });
        }
    }
}

/// Extract `<Field>` + `<DataField>` column references and emit `reads_column` edges.
fn extract_ssrs_data_fields(
    source: &str,
    line_offsets: &[usize],
    file_path: &str,
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let field_re = match get_compiled_regex(
        &DATA_FIELD_RE,
        r#"(?is)<Field\b[^>]*\bName\s*=\s*["']([^"']+)["'][^>]*>.*?<DataField>\s*([^<]+?)\s*</DataField>"#,
        "ssrs_data_field",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in field_re.captures_iter(source) {
        let _field_name = cap.get(1).map_or("", |m| m.as_str());
        let data_field = cap.get(2).map_or("", |m| m.as_str()).trim();
        if data_field.is_empty() {
            continue;
        }
        let match_start = cap.get(0).map_or(0, |m| m.start());
        let line = offset_to_line(line_offsets, match_start);

        edges.push(ExtractedEdge {
            source_name: file_path.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "xml",
            target_name: data_field.to_lowercase(),
            target_kind: Some("db_column"),
            target_start_line: None,
            kind: "reads_column",
            metadata: Some(HashMap::from([(
                "source_report".into(),
                file_name.to_string(),
            )])),
        });
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Crystal Reports Detection (code-behind)
// ═══════════════════════════════════════════════════════════════════════════

/// Detect Crystal Reports usage patterns in C# or VB.NET code files.
///
/// Returns `(symbols, edges)` where:
///   - symbols include an `insight` node with migration guidance when Crystal
///     Reports usage is detected
///   - edges include `dependency` edges to referenced `.rpt` files and an
///     `anti_pattern` edge flagging Crystal Reports as a migration blocker
pub fn extract_crystal_reports_usage(
    rel_path: &RelPath,
    source: &str,
    language: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    let file_path = rel_path.as_str().to_string();
    let line_offsets = build_line_offsets(source);

    let source_language: &'static str = match language {
        "csharp" | "cs" => "csharp",
        "vb" | "vbnet" => "vbnet",
        _ => "csharp",
    };

    let mut has_crystal = false;
    let mut rpt_files: Vec<String> = Vec::new();

    // ── Namespace import detection ──────────────────────────────────────────
    if let Some(ns_re) = get_compiled_regex(
        &CR_NAMESPACE_RE,
        r"(?i)(?:using|Imports)\s+CrystalDecisions(?:\.\w+)+",
        "cr_namespace",
    ) {
        if ns_re.is_match(source) {
            has_crystal = true;
        }
    }

    // ── ReportDocument instantiation ────────────────────────────────────────
    if let Some(rd_re) = get_compiled_regex(
        &CR_REPORT_DOC_NEW_RE,
        r"(?i)\bnew\s+ReportDocument\s*\(",
        "cr_report_doc_new",
    ) {
        if rd_re.is_match(source) {
            has_crystal = true;
        }
    }

    // ── ReportDocument.Load("path.rpt") calls ──────────────────────────────
    if let Some(load_re) = get_compiled_regex(
        &CR_LOAD_RE,
        r#"(?i)\.Load\s*\(\s*["']([^"']+\.rpt)["']"#,
        "cr_load",
    ) {
        for cap in load_re.captures_iter(source) {
            has_crystal = true;
            if let Some(m) = cap.get(1) {
                let rpt_path = m.as_str().to_string();
                let line = offset_to_line(&line_offsets, m.start());

                if !rpt_files.contains(&rpt_path) {
                    rpt_files.push(rpt_path.clone());
                }

                // Dependency edge to the .rpt file.
                edges.push(ExtractedEdge {
                    source_name: file_path.clone(),
                    source_kind: "file",
                    source_start_line: line,
                    source_language,
                    target_name: rpt_path,
                    target_kind: Some("file"),
                    target_start_line: None,
                    kind: "dependency",
                    metadata: Some(HashMap::from([(
                        "relationship".into(),
                        "crystal_report_load".into(),
                    )])),
                });
            }
        }
    }

    // ── SetDataSource calls ─────────────────────────────────────────────────
    if let Some(sds_re) = get_compiled_regex(
        &CR_SET_DATASOURCE_RE,
        r"(?i)\.SetDataSource\s*\(",
        "cr_set_datasource",
    ) {
        if sds_re.is_match(source) {
            has_crystal = true;
        }
    }

    // ── CrystalReportViewer usage in code ───────────────────────────────────
    if let Some(viewer_re) = get_compiled_regex(
        &CR_VIEWER_CODE_RE,
        r"(?i)\bCrystalReportViewer\b",
        "cr_viewer_code",
    ) {
        if viewer_re.is_match(source) {
            has_crystal = true;
        }
    }

    // ── Emit results only when Crystal Reports usage was detected ───────────
    if has_crystal {
        // Insight node.
        let mut insight_meta = HashMap::new();
        insight_meta.insert(
            "modern_equivalent".into(),
            "Migrate to SSRS, DevExpress Reports, or generate PDF via code (QuestPDF, iText)"
                .into(),
        );
        insight_meta.insert("file".into(), file_path.clone());
        if !rpt_files.is_empty() {
            insight_meta.insert("rpt_files".into(), rpt_files.join(", "));
        }

        symbols.push(ExtractedSymbol {
            name: "crystal_reports_usage".into(),
            kind: "insight",
            start_line: 0,
            end_line: 0,
            metadata: Some(insight_meta),
        });

        // Anti-pattern edge: Crystal Reports is a migration blocker.
        let mut ap_meta = HashMap::new();
        ap_meta.insert("blocker_type".into(), "crystal_reports".into());
        ap_meta.insert(
            "reason".into(),
            "Binary .rpt files cannot be programmatically migrated; \
             Crystal Reports runtime requires COM interop or legacy SDK"
                .into(),
        );
        if !rpt_files.is_empty() {
            ap_meta.insert("rpt_files".into(), rpt_files.join(", "));
        }

        edges.push(ExtractedEdge {
            source_name: file_path.clone(),
            source_kind: "file",
            source_start_line: 0,
            source_language,
            target_name: "crystal_reports_usage".into(),
            target_kind: Some("insight"),
            target_start_line: None,
            kind: "anti_pattern",
            metadata: Some(ap_meta),
        });
    }

    (symbols, edges)
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Crystal Reports in ASPX Markup
// ═══════════════════════════════════════════════════════════════════════════

/// Detect Crystal Reports viewer controls in `.aspx` / `.ascx` markup.
///
/// Looks for:
///   - `<CR:CrystalReportViewer ...>`
///   - `<CrystalDecisions:CrystalReportViewer ...>`
///   - `ReportSource` attribute values pointing to `.rpt` files
///
/// Returns `(symbols, edges)` with an `insight` node and an `anti_pattern`
/// edge when Crystal Reports markup is found.
pub fn extract_crystal_reports_in_markup(
    rel_path: &RelPath,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    let file_path = rel_path.as_str().to_string();
    let line_offsets = build_line_offsets(source);

    let mut has_crystal = false;
    let mut rpt_files: Vec<String> = Vec::new();

    // ── <CR:CrystalReportViewer> tags ───────────────────────────────────────
    if let Some(viewer_re) = get_compiled_regex(
        &CR_VIEWER_MARKUP_RE,
        r"(?i)<CR:CrystalReportViewer\b",
        "cr_viewer_markup",
    ) {
        for m in viewer_re.find_iter(source) {
            has_crystal = true;
            let line = offset_to_line(&line_offsets, m.start());

            edges.push(ExtractedEdge {
                source_name: file_path.clone(),
                source_kind: "file",
                source_start_line: line,
                source_language: "aspx",
                target_name: "CrystalReportViewer".into(),
                target_kind: Some("control"),
                target_start_line: None,
                kind: "contains",
                metadata: Some(HashMap::from([(
                    "control_type".into(),
                    "CR:CrystalReportViewer".into(),
                )])),
            });
        }
    }

    // ── <CrystalDecisions:...> tag prefix ───────────────────────────────────
    if let Some(prefix_re) = get_compiled_regex(
        &CR_TAG_PREFIX_RE,
        r"(?i)<CrystalDecisions:(\w+)\b",
        "cr_tag_prefix",
    ) {
        for cap in prefix_re.captures_iter(source) {
            has_crystal = true;
            let control_name = cap.get(1).map_or("Unknown", |m| m.as_str());
            let match_start = cap.get(0).map_or(0, |m| m.start());
            let line = offset_to_line(&line_offsets, match_start);

            edges.push(ExtractedEdge {
                source_name: file_path.clone(),
                source_kind: "file",
                source_start_line: line,
                source_language: "aspx",
                target_name: format!("CrystalDecisions:{control_name}"),
                target_kind: Some("control"),
                target_start_line: None,
                kind: "contains",
                metadata: Some(HashMap::from([(
                    "control_type".into(),
                    format!("CrystalDecisions:{control_name}"),
                )])),
            });
        }
    }

    // ── ReportSource attribute referencing .rpt files ────────────────────────
    if let Some(src_re) = get_compiled_regex(
        &CR_REPORT_SOURCE_RE,
        r#"(?i)ReportSource\s*=\s*["']([^"']+\.rpt)["']"#,
        "cr_report_source",
    ) {
        for cap in src_re.captures_iter(source) {
            if let Some(m) = cap.get(1) {
                let rpt_path = m.as_str().to_string();
                let line = offset_to_line(&line_offsets, m.start());

                if !rpt_files.contains(&rpt_path) {
                    rpt_files.push(rpt_path.clone());
                }

                edges.push(ExtractedEdge {
                    source_name: file_path.clone(),
                    source_kind: "file",
                    source_start_line: line,
                    source_language: "aspx",
                    target_name: rpt_path,
                    target_kind: Some("file"),
                    target_start_line: None,
                    kind: "dependency",
                    metadata: Some(HashMap::from([(
                        "relationship".into(),
                        "crystal_report_source".into(),
                    )])),
                });
            }
        }
    }

    // ── Emit insight + anti-pattern if Crystal Reports markup was found ─────
    if has_crystal {
        let mut insight_meta = HashMap::new();
        insight_meta.insert(
            "modern_equivalent".into(),
            "Migrate to SSRS, DevExpress Reports, or generate PDF via code (QuestPDF, iText)"
                .into(),
        );
        insight_meta.insert("file".into(), file_path.clone());
        insight_meta.insert("detected_in".into(), "markup".into());
        if !rpt_files.is_empty() {
            insight_meta.insert("rpt_files".into(), rpt_files.join(", "));
        }

        symbols.push(ExtractedSymbol {
            name: "crystal_reports_usage".into(),
            kind: "insight",
            start_line: 0,
            end_line: 0,
            metadata: Some(insight_meta),
        });

        let mut ap_meta = HashMap::new();
        ap_meta.insert("blocker_type".into(), "crystal_reports".into());
        ap_meta.insert("detected_in".into(), "markup".into());
        ap_meta.insert(
            "reason".into(),
            "Binary .rpt files cannot be programmatically migrated; \
             Crystal Reports viewer control requires legacy runtime"
                .into(),
        );
        if !rpt_files.is_empty() {
            ap_meta.insert("rpt_files".into(), rpt_files.join(", "));
        }

        edges.push(ExtractedEdge {
            source_name: file_path.clone(),
            source_kind: "file",
            source_start_line: 0,
            source_language: "aspx",
            target_name: "crystal_reports_usage".into(),
            target_kind: Some("insight"),
            target_start_line: None,
            kind: "anti_pattern",
            metadata: Some(ap_meta),
        });
    }

    (symbols, edges)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── SSRS tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_ssrs_basic_report() {
        let rdl = r#"<?xml version="1.0" encoding="utf-8"?>
<Report xmlns="http://schemas.microsoft.com/sqlserver/reporting/2008/01/reportdefinition">
  <DataSources>
    <DataSource Name="MyDS">
      <ConnectionString>Data Source=server;Initial Catalog=MyDB</ConnectionString>
    </DataSource>
  </DataSources>
  <DataSets>
    <DataSet Name="Orders">
      <Query>
        <DataSourceName>MyDS</DataSourceName>
        <CommandText>SELECT OrderId, CustomerId FROM dbo.Orders WHERE Status = @Status</CommandText>
      </Query>
      <Fields>
        <Field Name="OrderId">
          <DataField>OrderId</DataField>
        </Field>
        <Field Name="CustomerId">
          <DataField>CustomerId</DataField>
        </Field>
      </Fields>
    </DataSet>
  </DataSets>
  <ReportParameters>
    <ReportParameter Name="Status">
      <DataType>String</DataType>
    </ReportParameter>
  </ReportParameters>
</Report>"#;

        let rel = RelPath::new("Reports/OrderReport.rdl");
        let (syms, edges) = extract_ssrs_report(&rel, rdl);

        // Report symbol.
        let report = syms
            .iter()
            .find(|s| s.kind == "report")
            .expect("report symbol");
        assert_eq!(report.name, "OrderReport.rdl");
        let meta = report.metadata.as_ref().expect("report meta");
        assert_eq!(meta.get("report_type").map(|s| s.as_str()), Some("ssrs"));
        assert_eq!(meta.get("parameter_count").map(|s| s.as_str()), Some("1"));
        assert_eq!(meta.get("dataset_count").map(|s| s.as_str()), Some("1"));
        assert_eq!(
            meta.get("has_subreports").map(|s| s.as_str()),
            Some("false")
        );

        // Insight symbol.
        let insight = syms
            .iter()
            .find(|s| s.kind == "insight")
            .expect("insight symbol");
        assert!(insight.name.starts_with("ssrs_report:"));
        let imeta = insight.metadata.as_ref().expect("insight meta");
        assert!(imeta.get("modern_equivalent").is_some());

        // Connection string symbol.
        let conn = syms
            .iter()
            .find(|s| s.kind == "connection_string")
            .expect("conn_string symbol");
        assert_eq!(conn.name, "datasource:MyDS");

        // sql_calls edge.
        let sql_edges: Vec<_> = edges.iter().filter(|e| e.kind == "sql_calls").collect();
        assert_eq!(sql_edges.len(), 1);

        // queries_table edge for Orders.
        let qt_edges: Vec<_> = edges.iter().filter(|e| e.kind == "queries_table").collect();
        assert!(
            qt_edges.iter().any(|e| e.target_name == "orders"),
            "Expected queries_table edge to 'orders', got: {:?}",
            qt_edges.iter().map(|e| &e.target_name).collect::<Vec<_>>()
        );

        // reads_column edges.
        let rc_edges: Vec<_> = edges.iter().filter(|e| e.kind == "reads_column").collect();
        assert_eq!(rc_edges.len(), 2);
        let col_names: Vec<_> = rc_edges.iter().map(|e| e.target_name.as_str()).collect();
        assert!(col_names.contains(&"orderid"));
        assert!(col_names.contains(&"customerid"));

        // contains edge (datasource).
        let contains_edges: Vec<_> = edges.iter().filter(|e| e.kind == "contains").collect();
        assert!(!contains_edges.is_empty());
    }

    #[test]
    fn test_ssrs_with_subreport() {
        let rdl = r#"<Report>
  <Body>
    <ReportItems>
      <Subreport Name="SubDetail">
        <ReportName>DetailReport.rdl</ReportName>
      </Subreport>
    </ReportItems>
  </Body>
</Report>"#;

        let rel = RelPath::new("Reports/MasterReport.rdl");
        let (syms, edges) = extract_ssrs_report(&rel, rdl);

        let report = syms.iter().find(|s| s.kind == "report").expect("report");
        let meta = report.metadata.as_ref().expect("meta");
        assert_eq!(meta.get("has_subreports").map(|s| s.as_str()), Some("true"));

        let dep_edges: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(dep_edges.len(), 1);
        assert_eq!(dep_edges[0].target_name, "DetailReport.rdl");
        let dep_meta = dep_edges[0].metadata.as_ref().expect("dep meta");
        assert_eq!(
            dep_meta.get("relationship").map(|s| s.as_str()),
            Some("subreport")
        );
    }

    #[test]
    fn test_ssrs_multiple_datasets() {
        let rdl = r#"<Report>
  <DataSets>
    <DataSet Name="Customers">
      <Query>
        <CommandText>SELECT Name FROM Customers</CommandText>
      </Query>
      <Fields>
        <Field Name="Name"><DataField>Name</DataField></Field>
      </Fields>
    </DataSet>
    <DataSet Name="Orders">
      <Query>
        <CommandText>SELECT * FROM Orders JOIN OrderItems ON Orders.Id = OrderItems.OrderId</CommandText>
      </Query>
    </DataSet>
  </DataSets>
</Report>"#;

        let rel = RelPath::new("Reports/Multi.rdl");
        let (syms, edges) = extract_ssrs_report(&rel, rdl);

        let report = syms.iter().find(|s| s.kind == "report").expect("report");
        let meta = report.metadata.as_ref().expect("meta");
        assert_eq!(meta.get("dataset_count").map(|s| s.as_str()), Some("2"));

        // Two sql_calls edges.
        let sql_edges: Vec<_> = edges.iter().filter(|e| e.kind == "sql_calls").collect();
        assert_eq!(sql_edges.len(), 2);

        // queries_table edges: Customers, Orders, OrderItems.
        let qt_edges: Vec<_> = edges.iter().filter(|e| e.kind == "queries_table").collect();
        let table_names: Vec<_> = qt_edges.iter().map(|e| e.target_name.as_str()).collect();
        assert!(table_names.contains(&"customers"));
        assert!(table_names.contains(&"orders"));
        assert!(table_names.contains(&"orderitems"));
    }

    #[test]
    fn test_ssrs_empty_source() {
        let rel = RelPath::new("Reports/Empty.rdl");
        let (syms, edges) = extract_ssrs_report(&rel, "");

        // Should still emit the report and insight symbols.
        assert!(syms.iter().any(|s| s.kind == "report"));
        assert!(syms.iter().any(|s| s.kind == "insight"));
        assert!(edges.is_empty());
    }

    #[test]
    fn test_ssrs_multiple_parameters() {
        let rdl = r#"<Report>
  <ReportParameters>
    <ReportParameter Name="StartDate"><DataType>DateTime</DataType></ReportParameter>
    <ReportParameter Name="EndDate"><DataType>DateTime</DataType></ReportParameter>
    <ReportParameter Name="Region"><DataType>String</DataType></ReportParameter>
  </ReportParameters>
</Report>"#;

        let rel = RelPath::new("Reports/Parameterized.rdl");
        let (syms, _edges) = extract_ssrs_report(&rel, rdl);

        let report = syms.iter().find(|s| s.kind == "report").expect("report");
        let meta = report.metadata.as_ref().expect("meta");
        assert_eq!(meta.get("parameter_count").map(|s| s.as_str()), Some("3"));
    }

    // ── Crystal Reports (code-behind) tests ─────────────────────────────────

    #[test]
    fn test_crystal_reports_csharp() {
        let source = r#"
using CrystalDecisions.CrystalReports.Engine;
using CrystalDecisions.Shared;

public partial class ReportPage : System.Web.UI.Page
{
    protected void Page_Load(object sender, EventArgs e)
    {
        ReportDocument report = new ReportDocument();
        report.Load("Reports/SalesReport.rpt");
        report.SetDataSource(GetData());
        CrystalReportViewer1.ReportSource = report;
    }
}
"#;

        let rel = RelPath::new("Pages/ReportPage.aspx.cs");
        let (syms, edges) = extract_crystal_reports_usage(&rel, source, "csharp");

        // Insight symbol.
        let insight = syms
            .iter()
            .find(|s| s.kind == "insight")
            .expect("insight symbol");
        assert_eq!(insight.name, "crystal_reports_usage");
        let imeta = insight.metadata.as_ref().expect("insight meta");
        assert!(imeta.get("modern_equivalent").is_some());
        assert!(
            imeta
                .get("rpt_files")
                .expect("rpt_files")
                .contains("SalesReport.rpt")
        );

        // Dependency edge to .rpt file.
        let dep_edges: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(dep_edges.len(), 1);
        assert_eq!(dep_edges[0].target_name, "Reports/SalesReport.rpt");

        // Anti-pattern edge.
        let ap_edges: Vec<_> = edges.iter().filter(|e| e.kind == "anti_pattern").collect();
        assert_eq!(ap_edges.len(), 1);
        let ap_meta = ap_edges[0].metadata.as_ref().expect("ap meta");
        assert_eq!(
            ap_meta.get("blocker_type").map(|s| s.as_str()),
            Some("crystal_reports")
        );
    }

    #[test]
    fn test_crystal_reports_vb() {
        let source = r#"
Imports CrystalDecisions.CrystalReports.Engine

Public Class ReportForm
    Private Sub LoadReport()
        Dim rpt As New ReportDocument()
        rpt.Load("Reports/Inventory.rpt")
        rpt.SetDataSource(ds)
    End Sub
End Class
"#;

        let rel = RelPath::new("Forms/ReportForm.vb");
        let (syms, edges) = extract_crystal_reports_usage(&rel, source, "vb");

        assert!(syms.iter().any(|s| s.kind == "insight"));

        let dep_edges: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(dep_edges.len(), 1);
        assert_eq!(dep_edges[0].target_name, "Reports/Inventory.rpt");
        assert_eq!(dep_edges[0].source_language, "vbnet");

        let ap_edges: Vec<_> = edges.iter().filter(|e| e.kind == "anti_pattern").collect();
        assert_eq!(ap_edges.len(), 1);
    }

    #[test]
    fn test_crystal_reports_multiple_rpt_files() {
        let source = r#"
using CrystalDecisions.CrystalReports.Engine;

void LoadReports() {
    var r1 = new ReportDocument();
    r1.Load("Reports/Sales.rpt");
    var r2 = new ReportDocument();
    r2.Load("Reports/Inventory.rpt");
}
"#;

        let rel = RelPath::new("Code/Reports.cs");
        let (syms, edges) = extract_crystal_reports_usage(&rel, source, "csharp");

        let insight = syms.iter().find(|s| s.kind == "insight").expect("insight");
        let imeta = insight.metadata.as_ref().expect("meta");
        let rpt_list = imeta.get("rpt_files").expect("rpt_files");
        assert!(rpt_list.contains("Sales.rpt"));
        assert!(rpt_list.contains("Inventory.rpt"));

        let dep_edges: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(dep_edges.len(), 2);
    }

    #[test]
    fn test_crystal_reports_no_match() {
        let source = r#"
using System;
public class NormalPage : Page
{
    protected void Page_Load(object sender, EventArgs e) { }
}
"#;

        let rel = RelPath::new("Pages/Normal.aspx.cs");
        let (syms, edges) = extract_crystal_reports_usage(&rel, source, "csharp");

        assert!(syms.is_empty());
        assert!(edges.is_empty());
    }

    // ── Crystal Reports in markup tests ─────────────────────────────────────

    #[test]
    fn test_crystal_reports_aspx_viewer() {
        let markup = r#"
<%@ Page Language="C#" AutoEventWireup="true" CodeBehind="ReportPage.aspx.cs" Inherits="App.ReportPage" %>
<%@ Register Assembly="CrystalDecisions.Web" Namespace="CrystalDecisions.Web" TagPrefix="CR" %>
<html>
<body>
    <form id="form1" runat="server">
        <CR:CrystalReportViewer ID="Viewer1" runat="server"
            ReportSource="~/Reports/Monthly.rpt"
            AutoDataBind="true" />
    </form>
</body>
</html>
"#;

        let rel = RelPath::new("Pages/ReportPage.aspx");
        let (syms, edges) = extract_crystal_reports_in_markup(&rel, markup);

        // Insight symbol.
        assert!(syms.iter().any(|s| s.kind == "insight"));

        // Contains edge for CR:CrystalReportViewer.
        let contains_edges: Vec<_> = edges.iter().filter(|e| e.kind == "contains").collect();
        assert!(!contains_edges.is_empty());

        // Dependency edge to .rpt file.
        let dep_edges: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(dep_edges.len(), 1);
        assert!(dep_edges[0].target_name.contains("Monthly.rpt"));

        // Anti-pattern edge.
        let ap_edges: Vec<_> = edges.iter().filter(|e| e.kind == "anti_pattern").collect();
        assert_eq!(ap_edges.len(), 1);
    }

    #[test]
    fn test_crystal_decisions_tag_prefix() {
        let markup = r#"
<CrystalDecisions:CrystalReportViewer ID="crViewer" runat="server" />
<CrystalDecisions:ParameterFields ID="params" runat="server" />
"#;

        let rel = RelPath::new("Pages/CRPage.aspx");
        let (syms, edges) = extract_crystal_reports_in_markup(&rel, markup);

        assert!(syms.iter().any(|s| s.kind == "insight"));

        let contains_edges: Vec<_> = edges.iter().filter(|e| e.kind == "contains").collect();
        // Should detect both CrystalDecisions:... controls.
        assert!(contains_edges.len() >= 2);
    }

    #[test]
    fn test_markup_no_crystal_reports() {
        let markup = r#"
<html>
<body>
    <form id="form1" runat="server">
        <asp:GridView ID="gv1" runat="server" />
    </form>
</body>
</html>
"#;

        let rel = RelPath::new("Pages/Normal.aspx");
        let (syms, edges) = extract_crystal_reports_in_markup(&rel, markup);

        assert!(syms.is_empty());
        assert!(edges.is_empty());
    }

    // ── SQL table extraction tests ──────────────────────────────────────────

    #[test]
    fn test_extract_table_names_basic() {
        let sql = "SELECT o.Id, c.Name FROM Orders o JOIN Customers c ON o.CustomerId = c.Id";
        let tables = extract_table_names_from_sql(sql);
        assert!(tables.contains(&"Orders".to_string()));
        assert!(tables.contains(&"Customers".to_string()));
    }

    #[test]
    fn test_extract_table_names_with_schema() {
        let sql = "SELECT * FROM [dbo].[Products] WHERE Active = 1";
        let tables = extract_table_names_from_sql(sql);
        assert!(tables.contains(&"Products".to_string()));
    }

    #[test]
    fn test_extract_table_names_insert_update() {
        let sql = "INSERT INTO AuditLog (Action) VALUES ('test'); UPDATE Users SET Active = 0";
        let tables = extract_table_names_from_sql(sql);
        assert!(tables.contains(&"AuditLog".to_string()));
        assert!(tables.contains(&"Users".to_string()));
    }

    #[test]
    fn test_extract_table_names_dedup() {
        let sql = "SELECT * FROM Orders WHERE Id IN (SELECT OrderId FROM Orders)";
        let tables = extract_table_names_from_sql(sql);
        // Should only contain Orders once.
        assert_eq!(
            tables.iter().filter(|t| *t == "Orders").count(),
            1,
            "table names should be deduplicated"
        );
    }
}
