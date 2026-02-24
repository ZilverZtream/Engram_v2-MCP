/// Classic ASP (.asp) file extractor.
///
/// Detects patterns specific to classic ASP/VBScript pages and emits symbols
/// and edges for the migration graph:
///
///   - **Server-side code blocks**: `<% ... %>` and `<script runat="server">` blocks
///     with `Sub`/`Function` definitions extracted as `function` symbols.
///   - **State access**: `Session("key")`, `Application("key")`,
///     `Request.QueryString("key")`, `Request.Form("key")`,
///     `Request.Cookies("key")`, `Response.Cookies("key")`.
///   - **Navigation**: `Response.Redirect`, `Server.Transfer` as `dependency` edges.
///   - **COM / Database**: `Server.CreateObject(...)` as `insight` nodes,
///     `conn.Open`, `conn.Execute`, `Command.CommandText` as `sql_calls` edges.
///   - **Includes**: `<!--#include file=...-->` / `<!--#include virtual=...-->`.
///   - **File-level insight**: Every classic ASP file gets a high-priority migration insight.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

/// Matches `<% ... %>` inline code blocks (non-greedy).
static INLINE_BLOCK_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `<script runat="server">...</script>` blocks.
static SCRIPT_RUNAT_RE: OnceLock<Regex> = OnceLock::new();

/// Matches VBScript `Sub`/`Function` declarations inside server code.
static VBS_FUNC_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `Session("key")` or `Application("key")`.
static SESSION_APP_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `Request.QueryString("key")`, `Request.Form("key")`, `Request.Cookies("key")`.
static REQUEST_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `Response.Cookies("key")`.
static RESPONSE_COOKIE_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `Response.Redirect "url"` or `Response.Redirect("url")`.
static REDIRECT_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `Server.Transfer "url"` or `Server.Transfer("url")`.
static SERVER_TRANSFER_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `Response.Write`.
static RESPONSE_WRITE_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `Server.CreateObject("ProgId")`.
static CREATE_OBJECT_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `conn.Open "connectionstring"` or `conn.Open("connectionstring")`.
static CONN_OPEN_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `conn.Execute("SQL")` or `rs = conn.Execute("SQL")`.
static CONN_EXECUTE_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `Command.CommandText = "SQL"` or `.CommandText = "SQL"`.
static COMMAND_TEXT_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `<!--#include file="path"-->` or `<!--#include virtual="path"-->`.
static INCLUDE_RE: OnceLock<Regex> = OnceLock::new();

// ── Regex Compiler Helper ────────────────────────────────────────────────────

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

// ── Regex Accessors ──────────────────────────────────────────────────────────

fn inline_block_regex() -> Option<&'static Regex> {
    // Note: Rust's `regex` crate does not support lookaheads, so we match all
    // <% ... %> blocks and filter out directive blocks (<%@ ... %>) after matching.
    get_compiled_regex(&INLINE_BLOCK_RE, r"(?si)<%(.*?)%>", "asp_inline_block")
}

fn script_runat_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &SCRIPT_RUNAT_RE,
        r#"(?si)<script[^>]+runat\s*=\s*["']server["'][^>]*>(.*?)</script>"#,
        "asp_script_runat",
    )
}

fn vbs_func_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &VBS_FUNC_RE,
        r"(?im)^\s*(?:Public\s+|Private\s+)?(Sub|Function)\s+(\w+)\s*\(",
        "asp_vbs_func",
    )
}

fn session_app_regex() -> Option<&'static Regex> {
    // Matches Session("key") or Application("key") with single or double quotes.
    get_compiled_regex(
        &SESSION_APP_RE,
        r#"(?i)(Session|Application)\s*\(\s*(?:"([^"]+)"|'([^']+)')\s*\)"#,
        "asp_session_app",
    )
}

fn request_regex() -> Option<&'static Regex> {
    // Matches Request.QueryString("key"), Request.Form("key"), Request.Cookies("key").
    get_compiled_regex(
        &REQUEST_RE,
        r#"(?i)Request\s*\.\s*(QueryString|Form|Cookies)\s*\(\s*(?:"([^"]+)"|'([^']+)')\s*\)"#,
        "asp_request",
    )
}

fn response_cookie_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &RESPONSE_COOKIE_RE,
        r#"(?i)Response\s*\.\s*Cookies\s*\(\s*(?:"([^"]+)"|'([^']+)')\s*\)"#,
        "asp_response_cookie",
    )
}

fn redirect_regex() -> Option<&'static Regex> {
    // Response.Redirect "url" or Response.Redirect("url") — single/double quotes.
    get_compiled_regex(
        &REDIRECT_RE,
        r#"(?i)Response\s*\.\s*Redirect\s*[\(]?\s*(?:"([^"]+)"|'([^']+)')\s*[\)]?"#,
        "asp_redirect",
    )
}

fn server_transfer_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &SERVER_TRANSFER_RE,
        r#"(?i)Server\s*\.\s*Transfer\s*[\(]?\s*(?:"([^"]+)"|'([^']+)')\s*[\)]?"#,
        "asp_server_transfer",
    )
}

fn response_write_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &RESPONSE_WRITE_RE,
        r"(?i)Response\s*\.\s*Write\b",
        "asp_response_write",
    )
}

fn create_object_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &CREATE_OBJECT_RE,
        r#"(?i)Server\s*\.\s*CreateObject\s*\(\s*(?:"([^"]+)"|'([^']+)')\s*\)"#,
        "asp_create_object",
    )
}

fn conn_open_regex() -> Option<&'static Regex> {
    // Matches variable.Open "connstring" or variable.Open("connstring").
    get_compiled_regex(
        &CONN_OPEN_RE,
        r#"(?i)\.\s*Open\s*[\(]?\s*(?:"([^"]+)"|'([^']+)')\s*[\)]?"#,
        "asp_conn_open",
    )
}

fn conn_execute_regex() -> Option<&'static Regex> {
    // Matches .Execute("SQL") with optional assignment (rs = conn.Execute("SQL")).
    // The closing paren is not required immediately after the string literal,
    // because classic ASP often concatenates: conn.Execute("SELECT ... " & var)
    get_compiled_regex(
        &CONN_EXECUTE_RE,
        r#"(?i)\.\s*Execute\s*\(\s*(?:"([^"]+)"|'([^']+)')"#,
        "asp_conn_execute",
    )
}

fn command_text_regex() -> Option<&'static Regex> {
    // Matches .CommandText = "SQL" or CommandText = "SQL".
    get_compiled_regex(
        &COMMAND_TEXT_RE,
        r#"(?i)\.?\s*CommandText\s*=\s*(?:"([^"]+)"|'([^']+)')"#,
        "asp_command_text",
    )
}

fn include_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &INCLUDE_RE,
        r#"(?i)<!--\s*#include\s+(file|virtual)\s*=\s*(?:"([^"]+)"|'([^']+)')\s*-->"#,
        "asp_include",
    )
}

// ── Main Entry Point ─────────────────────────────────────────────────────────

/// Extract symbols and edges from a classic ASP (.asp) source file.
///
/// Returns `(symbols, edges)` where symbols are function definitions, state
/// keys, COM insights, etc., and edges are state reads/writes, SQL calls,
/// include relationships, and navigation dependencies.
pub fn extract_classic_asp(
    rel_path: &RelPath,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    let file_name = rel_path.as_str().to_string();
    let lines: Vec<&str> = source.lines().collect();

    // ── 1. File-level insight: every classic ASP file gets a migration marker ──
    {
        let mut meta = HashMap::with_capacity(3);
        meta.insert("migration_priority".to_string(), "high".to_string());
        meta.insert("technology".to_string(), "classic_asp".to_string());
        meta.insert(
            "modern_equivalent".to_string(),
            "ASP.NET Core Razor Pages or Blazor".to_string(),
        );

        symbols.push(ExtractedSymbol {
            name: "classic_asp_file".to_string(),
            kind: "insight",
            start_line: 0,
            end_line: 0,
            metadata: Some(meta),
        });
    }

    // ── 2. Extract Sub/Function definitions from server-side code blocks ───────
    extract_server_functions(source, &lines, &mut symbols);

    // ── 3. State access detection ──────────────────────────────────────────────
    extract_state_accesses(source, &file_name, &lines, &mut symbols, &mut edges);

    // ── 4. Navigation detection ────────────────────────────────────────────────
    extract_navigation(source, &file_name, &lines, &mut edges);

    // ── 5. COM / Database access ───────────────────────────────────────────────
    extract_com_database(source, &file_name, &lines, &mut symbols, &mut edges);

    // ── 6. Include directive detection ─────────────────────────────────────────
    extract_includes(source, &file_name, &lines, &mut edges);

    (symbols, edges)
}

// ── Server-Side Code Block & Function Detection ──────────────────────────────

/// Extract VBScript `Sub`/`Function` definitions from `<% ... %>` and
/// `<script runat="server">` blocks.
fn extract_server_functions(source: &str, lines: &[&str], symbols: &mut Vec<ExtractedSymbol>) {
    let Some(func_re) = vbs_func_regex() else {
        return;
    };

    // Collect all server-side code regions as (start_byte, code_text) pairs.
    let mut code_regions: Vec<(usize, &str)> = Vec::new();

    if let Some(re) = inline_block_regex() {
        for cap in re.captures_iter(source) {
            if let (Some(full_match), Some(code)) = (cap.get(0), cap.get(1)) {
                // Skip directive blocks: <%@ ... %>
                let code_text = code.as_str();
                if code_text.trim_start().starts_with('@') {
                    continue;
                }
                code_regions.push((full_match.start(), code_text));
            }
        }
    }

    if let Some(re) = script_runat_regex() {
        for cap in re.captures_iter(source) {
            if let (Some(full_match), Some(code)) = (cap.get(0), cap.get(1)) {
                code_regions.push((full_match.start(), code.as_str()));
            }
        }
    }

    for (region_byte_offset, code) in &code_regions {
        // Compute the line number for the start of this region.
        let region_start_line = byte_offset_to_line(source, *region_byte_offset);

        for cap in func_re.captures_iter(code) {
            let func_name = cap.get(2).map_or("", |m| m.as_str());
            if func_name.is_empty() {
                continue;
            }

            // Approximate the line within the code region.
            let match_start = cap.get(0).map_or(0, |m| m.start());
            let local_line = code[..match_start].matches('\n').count() as u32;
            let start_line = region_start_line + local_line;

            // Estimate end_line by scanning for matching End Sub / End Function.
            let func_type = cap.get(1).map_or("Sub", |m| m.as_str());
            let end_line = find_end_line(code, match_start, func_type)
                .map(|local_end| region_start_line + local_end)
                .unwrap_or(start_line);

            symbols.push(ExtractedSymbol {
                name: func_name.to_string(),
                kind: "function",
                start_line,
                end_line,
                metadata: None,
            });
        }
    }

    // Also scan line-by-line for functions in lines within <% %> that might not
    // span multiple lines (fallback for single-line detection).
    let _ = lines; // already used via source byte-offset mapping above
}

// ── State Access Detection ───────────────────────────────────────────────────

/// Detect Session/Application/Request/Response state accesses and emit
/// `global_state` symbols and `reads_state`/`writes_state` edges.
fn extract_state_accesses(
    _source: &str,
    file_name: &str,
    lines: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) {
    let mut seen_state: HashMap<(String, String), bool> = HashMap::new();

    // ── Session("key") / Application("key") ──
    if let Some(re) = session_app_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("'") || trimmed.starts_with("REM ") {
                continue;
            }

            for cap in re.captures_iter(line) {
                let store_type = cap.get(1).map_or("", |m| m.as_str());
                let key = first_nonempty_group(&cap, &[2, 3]);
                if key.is_empty() {
                    continue;
                }

                let match_end = cap.get(0).map_or(0, |m| m.end());
                let is_write = is_assignment_lhs_at(line, match_end);

                let edge_kind = if is_write {
                    "writes_state"
                } else {
                    "reads_state"
                };
                let target = format!("state:{}:{}", store_type, key);

                let mut meta = HashMap::with_capacity(2);
                meta.insert("state_type".to_string(), store_type.to_string());
                meta.insert("state_key".to_string(), key.clone());

                edges.push(ExtractedEdge {
                    source_name: file_name.to_string(),
                    source_kind: "file",
                    source_start_line: line_idx as u32,
                    source_language: "vbscript",
                    target_name: target,
                    target_kind: Some("global_state"),
                    target_start_line: None,
                    kind: edge_kind,
                    metadata: Some(meta),
                });

                emit_state_symbol(symbols, &mut seen_state, store_type, &key, line_idx as u32);
            }
        }
    }

    // ── Request.QueryString / Request.Form / Request.Cookies ──
    if let Some(re) = request_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("'") || trimmed.starts_with("REM ") {
                continue;
            }

            for cap in re.captures_iter(line) {
                let collection = cap.get(1).map_or("", |m| m.as_str());
                let key = first_nonempty_group(&cap, &[2, 3]);
                if key.is_empty() {
                    continue;
                }

                let state_type = match collection.to_lowercase().as_str() {
                    "querystring" => "QueryString",
                    "form" => "Form",
                    "cookies" => "Cookie",
                    _ => "Request",
                };

                let target = format!("state:Request.{}:{}", collection, key);

                let mut meta = HashMap::with_capacity(2);
                meta.insert("state_type".to_string(), state_type.to_string());
                meta.insert("state_key".to_string(), key.clone());

                edges.push(ExtractedEdge {
                    source_name: file_name.to_string(),
                    source_kind: "file",
                    source_start_line: line_idx as u32,
                    source_language: "vbscript",
                    target_name: target,
                    target_kind: Some("global_state"),
                    target_start_line: None,
                    kind: "reads_state",
                    metadata: Some(meta),
                });

                emit_state_symbol(
                    symbols,
                    &mut seen_state,
                    &format!("Request.{}", collection),
                    &key,
                    line_idx as u32,
                );
            }
        }
    }

    // ── Response.Cookies("key") ──
    if let Some(re) = response_cookie_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("'") || trimmed.starts_with("REM ") {
                continue;
            }

            for cap in re.captures_iter(line) {
                let key = first_nonempty_group(&cap, &[1, 2]);
                if key.is_empty() {
                    continue;
                }

                let target = format!("state:Response.Cookies:{}", key);

                let mut meta = HashMap::with_capacity(2);
                meta.insert("state_type".to_string(), "Cookie".to_string());
                meta.insert("state_key".to_string(), key.clone());

                edges.push(ExtractedEdge {
                    source_name: file_name.to_string(),
                    source_kind: "file",
                    source_start_line: line_idx as u32,
                    source_language: "vbscript",
                    target_name: target,
                    target_kind: Some("global_state"),
                    target_start_line: None,
                    kind: "writes_state",
                    metadata: Some(meta),
                });

                emit_state_symbol(
                    symbols,
                    &mut seen_state,
                    "Response.Cookies",
                    &key,
                    line_idx as u32,
                );
            }
        }
    }
}

// ── Navigation Detection ─────────────────────────────────────────────────────

fn extract_navigation(
    _source: &str,
    file_name: &str,
    lines: &[&str],
    edges: &mut Vec<ExtractedEdge>,
) {
    // ── Response.Redirect ──
    if let Some(re) = redirect_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            for cap in re.captures_iter(line) {
                let url = first_nonempty_group(&cap, &[1, 2]);
                if url.is_empty() {
                    continue;
                }

                let mut meta = HashMap::with_capacity(1);
                meta.insert("navigation_type".to_string(), "redirect".to_string());

                edges.push(ExtractedEdge {
                    source_name: file_name.to_string(),
                    source_kind: "file",
                    source_start_line: line_idx as u32,
                    source_language: "vbscript",
                    target_name: url,
                    target_kind: Some("file"),
                    target_start_line: None,
                    kind: "dependency",
                    metadata: Some(meta),
                });
            }
        }
    }

    // ── Server.Transfer ──
    if let Some(re) = server_transfer_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            for cap in re.captures_iter(line) {
                let url = first_nonempty_group(&cap, &[1, 2]);
                if url.is_empty() {
                    continue;
                }

                let mut meta = HashMap::with_capacity(1);
                meta.insert("navigation_type".to_string(), "server_transfer".to_string());

                edges.push(ExtractedEdge {
                    source_name: file_name.to_string(),
                    source_kind: "file",
                    source_start_line: line_idx as u32,
                    source_language: "vbscript",
                    target_name: url,
                    target_kind: Some("file"),
                    target_start_line: None,
                    kind: "dependency",
                    metadata: Some(meta),
                });
            }
        }
    }

    // ── Response.Write (note presence in metadata, no separate edge) ──
    // Tracked for awareness but not emitted as a standalone edge.
}

// ── COM / Database Access ────────────────────────────────────────────────────

fn extract_com_database(
    _source: &str,
    file_name: &str,
    lines: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) {
    let mut seen_prog_ids: HashMap<String, bool> = HashMap::new();
    let mut has_response_write = false;

    // ── Server.CreateObject("ProgId") ──
    if let Some(re) = create_object_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            for cap in re.captures_iter(line) {
                let prog_id = first_nonempty_group(&cap, &[1, 2]);
                if prog_id.is_empty() {
                    continue;
                }

                // Emit an insight node for COM interop usage (dedup by prog_id).
                if !seen_prog_ids.contains_key(&prog_id) {
                    seen_prog_ids.insert(prog_id.clone(), true);

                    let mut meta = HashMap::with_capacity(2);
                    meta.insert("prog_id".to_string(), prog_id.clone());
                    meta.insert("technology".to_string(), "com_interop".to_string());

                    symbols.push(ExtractedSymbol {
                        name: "com_interop_usage".to_string(),
                        kind: "insight",
                        start_line: line_idx as u32,
                        end_line: line_idx as u32,
                        metadata: Some(meta),
                    });
                }
            }
        }
    }

    // ── conn.Open "connectionstring" ──
    if let Some(re) = conn_open_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            for cap in re.captures_iter(line) {
                let conn_str = first_nonempty_group(&cap, &[1, 2]);
                if conn_str.is_empty() {
                    continue;
                }

                let mut meta = HashMap::with_capacity(1);
                meta.insert("connection_string".to_string(), conn_str.clone());

                symbols.push(ExtractedSymbol {
                    name: format!("connection:{}", sanitize_conn_string(&conn_str)),
                    kind: "connection_string",
                    start_line: line_idx as u32,
                    end_line: line_idx as u32,
                    metadata: Some(meta),
                });
            }
        }
    }

    // ── conn.Execute("SQL") ──
    if let Some(re) = conn_execute_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            for cap in re.captures_iter(line) {
                let sql = first_nonempty_group(&cap, &[1, 2]);
                if sql.is_empty() {
                    continue;
                }

                let mut meta = HashMap::with_capacity(1);
                meta.insert("sql_snippet".to_string(), truncate_sql(&sql));

                edges.push(ExtractedEdge {
                    source_name: file_name.to_string(),
                    source_kind: "file",
                    source_start_line: line_idx as u32,
                    source_language: "vbscript",
                    target_name: extract_table_from_sql(&sql),
                    target_kind: Some("db_table"),
                    target_start_line: None,
                    kind: "sql_calls",
                    metadata: Some(meta),
                });
            }
        }
    }

    // ── Command.CommandText = "SQL" ──
    if let Some(re) = command_text_regex() {
        for (line_idx, line) in lines.iter().enumerate() {
            for cap in re.captures_iter(line) {
                let sql = first_nonempty_group(&cap, &[1, 2]);
                if sql.is_empty() {
                    continue;
                }

                let mut meta = HashMap::with_capacity(1);
                meta.insert("sql_snippet".to_string(), truncate_sql(&sql));

                edges.push(ExtractedEdge {
                    source_name: file_name.to_string(),
                    source_kind: "file",
                    source_start_line: line_idx as u32,
                    source_language: "vbscript",
                    target_name: extract_table_from_sql(&sql),
                    target_kind: Some("db_table"),
                    target_start_line: None,
                    kind: "sql_calls",
                    metadata: Some(meta),
                });
            }
        }
    }

    // ── Response.Write detection (metadata annotation) ──
    if let Some(re) = response_write_regex() {
        for line in lines.iter() {
            if re.is_match(line) {
                has_response_write = true;
                break;
            }
        }
    }

    // Annotate the file-level insight with Response.Write usage if detected.
    if has_response_write
        && let Some(insight) = symbols
            .iter_mut()
            .find(|s| s.name == "classic_asp_file" && s.kind == "insight")
        && let Some(ref mut meta) = insight.metadata
    {
        meta.insert("uses_response_write".to_string(), "true".to_string());
    }
}

// ── Include Directive Detection ──────────────────────────────────────────────

fn extract_includes(
    _source: &str,
    file_name: &str,
    lines: &[&str],
    edges: &mut Vec<ExtractedEdge>,
) {
    let Some(re) = include_regex() else {
        return;
    };

    for (line_idx, line) in lines.iter().enumerate() {
        for cap in re.captures_iter(line) {
            let include_type = cap.get(1).map_or("", |m| m.as_str());
            let path = first_nonempty_group(&cap, &[2, 3]);
            if path.is_empty() {
                continue;
            }

            let mut meta = HashMap::with_capacity(1);
            meta.insert("include_type".to_string(), include_type.to_lowercase());

            edges.push(ExtractedEdge {
                source_name: file_name.to_string(),
                source_kind: "file",
                source_start_line: line_idx as u32,
                source_language: "vbscript",
                target_name: path,
                target_kind: Some("file"),
                target_start_line: None,
                kind: "includes_file",
                metadata: Some(meta),
            });
        }
    }
}

// ── Helper Functions ─────────────────────────────────────────────────────────

/// Return the first non-empty group value from the specified group indices.
fn first_nonempty_group(cap: &regex::Captures, groups: &[usize]) -> String {
    for &idx in groups {
        if let Some(m) = cap.get(idx) {
            let s = m.as_str();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Check whether a matched expression is on the left-hand side of an assignment.
/// Uses the match's end byte offset within the line to avoid ambiguity when
/// the same token appears multiple times on a single line.
fn is_assignment_lhs_at(line: &str, match_end_in_line: usize) -> bool {
    if match_end_in_line > line.len() {
        return false;
    }
    let rest = &line[match_end_in_line..];
    let rest_trimmed = rest.trim_start();
    rest_trimmed.starts_with('=') && !rest_trimmed.starts_with("==")
}

/// Emit a unique `global_state` symbol for a state key, deduplicating by (store, key).
fn emit_state_symbol(
    symbols: &mut Vec<ExtractedSymbol>,
    seen: &mut HashMap<(String, String), bool>,
    store_type: &str,
    key: &str,
    line: u32,
) {
    let dedup_key = (store_type.to_string(), key.to_string());
    if seen.contains_key(&dedup_key) {
        return;
    }
    seen.insert(dedup_key, true);

    let mut meta = HashMap::with_capacity(2);
    meta.insert("state_type".to_string(), store_type.to_string());
    meta.insert("state_key".to_string(), key.to_string());

    symbols.push(ExtractedSymbol {
        name: format!("{}:{}", store_type, key),
        kind: "global_state",
        start_line: line,
        end_line: line,
        metadata: Some(meta),
    });
}

/// Compute 0-based line number from a byte offset into the source.
fn byte_offset_to_line(source: &str, byte_offset: usize) -> u32 {
    source[..byte_offset.min(source.len())]
        .matches('\n')
        .count() as u32
}

/// Find the end line (relative to code region start) for a Sub/Function by
/// scanning for `End Sub` or `End Function`.
fn find_end_line(code: &str, match_start: usize, func_type: &str) -> Option<u32> {
    let end_marker = format!("End {}", func_type);
    let remainder = &code[match_start..];
    for (i, line) in remainder.lines().enumerate() {
        if i == 0 {
            continue; // Skip the declaration line itself.
        }
        if line.trim().eq_ignore_ascii_case(&end_marker) {
            let offset = code[..match_start].matches('\n').count() as u32;
            return Some(offset + i as u32);
        }
    }
    None
}

/// Extract a probable table name from a SQL snippet.
/// Looks for `FROM tablename` or `INTO tablename` or `UPDATE tablename`.
fn extract_table_from_sql(sql: &str) -> String {
    static SQL_TABLE_RE: OnceLock<Regex> = OnceLock::new();
    let re = get_compiled_regex(
        &SQL_TABLE_RE,
        r"(?i)\b(?:FROM|INTO|UPDATE|JOIN)\s+\[?(\w+)\]?",
        "asp_sql_table",
    );
    if let Some(re) = re
        && let Some(cap) = re.captures(sql)
        && let Some(m) = cap.get(1)
    {
        return m.as_str().to_string();
    }
    // Fallback: use the entire SQL snippet (truncated) as the target name.
    truncate_sql(sql)
}

/// Truncate a SQL snippet for safe storage in metadata (max 200 chars).
fn truncate_sql(sql: &str) -> String {
    if sql.len() <= 200 {
        sql.to_string()
    } else {
        format!("{}...", &sql[..197])
    }
}

/// Sanitize a connection string for use as a symbol name.
/// Strips credentials and truncates.
fn sanitize_conn_string(conn: &str) -> String {
    // Remove password/pwd values for safety.
    static SANITIZE_RE: OnceLock<Regex> = OnceLock::new();
    let sanitized = if let Some(re) = get_compiled_regex(
        &SANITIZE_RE,
        r"(?i)(password|pwd)\s*=\s*[^;]+",
        "asp_sanitize_conn",
    ) {
        re.replace_all(conn, "$1=***").to_string()
    } else {
        conn.to_string()
    };

    if sanitized.len() <= 120 {
        sanitized
    } else {
        format!("{}...", &sanitized[..117])
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rel(path: &str) -> RelPath {
        RelPath::new(path)
    }

    // ── File-level insight ──

    #[test]
    fn test_file_level_insight_always_emitted() {
        let source = "<html><body>Hello</body></html>";
        let (syms, _) = extract_classic_asp(&rel("default.asp"), source);

        let insight = syms
            .iter()
            .find(|s| s.name == "classic_asp_file" && s.kind == "insight");
        assert!(insight.is_some(), "should emit classic_asp_file insight");

        let meta = insight.unwrap().metadata.as_ref().unwrap();
        assert_eq!(meta["migration_priority"], "high");
        assert_eq!(meta["technology"], "classic_asp");
        assert!(meta["modern_equivalent"].contains("ASP.NET Core"));
    }

    // ── Server-side function extraction ──

    #[test]
    fn test_inline_block_function_extraction() {
        let source = r#"<%
Sub ProcessOrder(orderId)
    ' do work
End Sub

Function GetTotal(cartId)
    GetTotal = 0
End Function
%>"#;
        let (syms, _) = extract_classic_asp(&rel("order.asp"), source);

        let funcs: Vec<_> = syms.iter().filter(|s| s.kind == "function").collect();
        assert_eq!(funcs.len(), 2, "should find Sub and Function");

        let names: Vec<&str> = funcs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"ProcessOrder"));
        assert!(names.contains(&"GetTotal"));
    }

    #[test]
    fn test_script_runat_server_function_extraction() {
        let source = r#"<script language="VBScript" runat="server">
Sub InitPage()
    ' initialize
End Sub
</script>"#;
        let (syms, _) = extract_classic_asp(&rel("init.asp"), source);

        let funcs: Vec<_> = syms.iter().filter(|s| s.kind == "function").collect();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "InitPage");
    }

    // ── State access ──

    #[test]
    fn test_session_read() {
        let source = r#"<%
Dim userId
userId = Session("UserId")
%>"#;
        let (syms, edges) = extract_classic_asp(&rel("page.asp"), source);

        let state_syms: Vec<_> = syms.iter().filter(|s| s.kind == "global_state").collect();
        assert_eq!(state_syms.len(), 1);
        assert_eq!(state_syms[0].name, "Session:UserId");

        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(reads.len(), 1);
    }

    #[test]
    fn test_session_write() {
        let source = r#"<% Session("UserName") = "admin" %>"#;
        let (_, edges) = extract_classic_asp(&rel("login.asp"), source);

        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        assert_eq!(writes.len(), 1);
    }

    #[test]
    fn test_application_access() {
        let source = r#"<%
Application("VisitorCount") = Application("VisitorCount") + 1
%>"#;
        let (syms, edges) = extract_classic_asp(&rel("global.asp"), source);

        let state_syms: Vec<_> = syms.iter().filter(|s| s.kind == "global_state").collect();
        assert_eq!(state_syms.len(), 1, "dedup by (store, key)");

        // First occurrence is a write (LHS of =), second is a read.
        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(reads.len(), 1);
    }

    #[test]
    fn test_request_querystring() {
        let source = r#"<% Dim id : id = Request.QueryString("id") %>"#;
        let (_, edges) = extract_classic_asp(&rel("detail.asp"), source);

        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(reads.len(), 1);

        let meta = reads[0].metadata.as_ref().unwrap();
        assert_eq!(meta["state_type"], "QueryString");
    }

    #[test]
    fn test_request_form() {
        let source = r#"<% Dim name : name = Request.Form("username") %>"#;
        let (_, edges) = extract_classic_asp(&rel("submit.asp"), source);

        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(reads.len(), 1);

        let meta = reads[0].metadata.as_ref().unwrap();
        assert_eq!(meta["state_type"], "Form");
    }

    #[test]
    fn test_request_cookies() {
        let source = r#"<% Dim pref : pref = Request.Cookies("theme") %>"#;
        let (_, edges) = extract_classic_asp(&rel("prefs.asp"), source);

        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(reads.len(), 1);

        let meta = reads[0].metadata.as_ref().unwrap();
        assert_eq!(meta["state_type"], "Cookie");
    }

    #[test]
    fn test_response_cookies_write() {
        let source = r#"<% Response.Cookies("theme") = "dark" %>"#;
        let (_, edges) = extract_classic_asp(&rel("set_pref.asp"), source);

        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        assert_eq!(writes.len(), 1);

        let meta = writes[0].metadata.as_ref().unwrap();
        assert_eq!(meta["state_type"], "Cookie");
    }

    #[test]
    fn test_single_quote_keys() {
        let source = r#"<% Dim x : x = Session('UserName') %>"#;
        let (syms, edges) = extract_classic_asp(&rel("sq.asp"), source);

        let state_syms: Vec<_> = syms.iter().filter(|s| s.kind == "global_state").collect();
        assert_eq!(state_syms.len(), 1);
        assert_eq!(state_syms[0].name, "Session:UserName");
        assert_eq!(edges.len(), 1);
    }

    // ── Navigation ──

    #[test]
    fn test_response_redirect() {
        let source = r#"<% Response.Redirect "login.asp" %>"#;
        let (_, edges) = extract_classic_asp(&rel("check.asp"), source);

        let deps: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].target_name, "login.asp");

        let meta = deps[0].metadata.as_ref().unwrap();
        assert_eq!(meta["navigation_type"], "redirect");
    }

    #[test]
    fn test_response_redirect_parens() {
        let source = r#"<% Response.Redirect("dashboard.asp") %>"#;
        let (_, edges) = extract_classic_asp(&rel("auth.asp"), source);

        let deps: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].target_name, "dashboard.asp");
    }

    #[test]
    fn test_server_transfer() {
        let source = r#"<% Server.Transfer "error.asp" %>"#;
        let (_, edges) = extract_classic_asp(&rel("handler.asp"), source);

        let deps: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].target_name, "error.asp");

        let meta = deps[0].metadata.as_ref().unwrap();
        assert_eq!(meta["navigation_type"], "server_transfer");
    }

    // ── COM / Database ──

    #[test]
    fn test_create_object_adodb_connection() {
        let source = r#"<% Set conn = Server.CreateObject("ADODB.Connection") %>"#;
        let (syms, _) = extract_classic_asp(&rel("db.asp"), source);

        let insights: Vec<_> = syms
            .iter()
            .filter(|s| s.name == "com_interop_usage" && s.kind == "insight")
            .collect();
        assert_eq!(insights.len(), 1);

        let meta = insights[0].metadata.as_ref().unwrap();
        assert_eq!(meta["prog_id"], "ADODB.Connection");
    }

    #[test]
    fn test_create_object_recordset() {
        let source = r#"<% Set rs = Server.CreateObject("ADODB.Recordset") %>"#;
        let (syms, _) = extract_classic_asp(&rel("query.asp"), source);

        let insights: Vec<_> = syms
            .iter()
            .filter(|s| s.name == "com_interop_usage")
            .collect();
        assert_eq!(insights.len(), 1);

        let meta = insights[0].metadata.as_ref().unwrap();
        assert_eq!(meta["prog_id"], "ADODB.Recordset");
    }

    #[test]
    fn test_create_object_other_com() {
        let source = r#"<% Set fso = Server.CreateObject("Scripting.FileSystemObject") %>"#;
        let (syms, _) = extract_classic_asp(&rel("files.asp"), source);

        let insights: Vec<_> = syms
            .iter()
            .filter(|s| s.name == "com_interop_usage")
            .collect();
        assert_eq!(insights.len(), 1);

        let meta = insights[0].metadata.as_ref().unwrap();
        assert_eq!(meta["prog_id"], "Scripting.FileSystemObject");
    }

    #[test]
    fn test_conn_open() {
        let source = r#"<% conn.Open "Provider=SQLOLEDB;Data Source=srv;Initial Catalog=mydb;User ID=sa;Password=secret" %>"#;
        let (syms, _) = extract_classic_asp(&rel("db.asp"), source);

        let conn_syms: Vec<_> = syms
            .iter()
            .filter(|s| s.kind == "connection_string")
            .collect();
        assert_eq!(conn_syms.len(), 1);
        // Password should be sanitized.
        assert!(conn_syms[0].name.contains("connection:"));
        assert!(!conn_syms[0].name.contains("secret"));
    }

    #[test]
    fn test_conn_execute_sql() {
        let source = r#"<% Set rs = conn.Execute("SELECT * FROM Users WHERE id = 1") %>"#;
        let (_, edges) = extract_classic_asp(&rel("users.asp"), source);

        let sql_edges: Vec<_> = edges.iter().filter(|e| e.kind == "sql_calls").collect();
        assert_eq!(sql_edges.len(), 1);
        assert_eq!(sql_edges[0].target_name, "Users");

        let meta = sql_edges[0].metadata.as_ref().unwrap();
        assert!(meta["sql_snippet"].contains("SELECT"));
    }

    #[test]
    fn test_command_text_sql() {
        let source = r#"<%
cmd.CommandText = "INSERT INTO Orders (customer_id, total) VALUES (1, 99.99)"
%>"#;
        let (_, edges) = extract_classic_asp(&rel("order.asp"), source);

        let sql_edges: Vec<_> = edges.iter().filter(|e| e.kind == "sql_calls").collect();
        assert_eq!(sql_edges.len(), 1);
        assert_eq!(sql_edges[0].target_name, "Orders");
    }

    // ── Includes ──

    #[test]
    fn test_include_file() {
        let source = r#"<!--#include file="header.asp"-->"#;
        let (_, edges) = extract_classic_asp(&rel("page.asp"), source);

        let includes: Vec<_> = edges.iter().filter(|e| e.kind == "includes_file").collect();
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].target_name, "header.asp");

        let meta = includes[0].metadata.as_ref().unwrap();
        assert_eq!(meta["include_type"], "file");
    }

    #[test]
    fn test_include_virtual() {
        let source = r#"<!--#include virtual="/inc/footer.asp"-->"#;
        let (_, edges) = extract_classic_asp(&rel("page.asp"), source);

        let includes: Vec<_> = edges.iter().filter(|e| e.kind == "includes_file").collect();
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].target_name, "/inc/footer.asp");

        let meta = includes[0].metadata.as_ref().unwrap();
        assert_eq!(meta["include_type"], "virtual");
    }

    #[test]
    fn test_multiple_includes() {
        let source = r#"<!--#include file="header.asp"-->
<html><body>Content</body></html>
<!--#include virtual="/inc/footer.asp"-->"#;
        let (_, edges) = extract_classic_asp(&rel("page.asp"), source);

        let includes: Vec<_> = edges.iter().filter(|e| e.kind == "includes_file").collect();
        assert_eq!(includes.len(), 2);
    }

    // ── Response.Write annotation ──

    #[test]
    fn test_response_write_annotation() {
        let source = r#"<% Response.Write "<h1>Hello</h1>" %>"#;
        let (syms, _) = extract_classic_asp(&rel("hello.asp"), source);

        let insight = syms
            .iter()
            .find(|s| s.name == "classic_asp_file" && s.kind == "insight")
            .unwrap();
        let meta = insight.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("uses_response_write").map(|s| s.as_str()),
            Some("true")
        );
    }

    #[test]
    fn test_no_response_write_annotation_when_absent() {
        let source = r#"<% Dim x : x = 1 %>"#;
        let (syms, _) = extract_classic_asp(&rel("simple.asp"), source);

        let insight = syms
            .iter()
            .find(|s| s.name == "classic_asp_file" && s.kind == "insight")
            .unwrap();
        let meta = insight.metadata.as_ref().unwrap();
        assert!(!meta.contains_key("uses_response_write"));
    }

    // ── Comprehensive scenario ──

    #[test]
    fn test_comprehensive_classic_asp_page() {
        let source = r#"<!--#include file="config.asp"-->
<!--#include virtual="/inc/db.asp"-->
<%
Dim userId, conn, rs

' Read session and request data
userId = Session("UserId")
Dim action : action = Request.QueryString("action")
Dim token : token = Request.Cookies("AuthToken")
Response.Cookies("LastVisit") = Now()

' Database access
Set conn = Server.CreateObject("ADODB.Connection")
conn.Open "Provider=SQLOLEDB;Server=db01;Database=app;UID=user;PWD=pass"
Set rs = conn.Execute("SELECT name FROM Users WHERE id = " & userId)

' Navigation
If IsEmpty(userId) Then
    Response.Redirect "login.asp"
End If

Sub RenderPage()
    Response.Write "<html><body>"
    Response.Write rs("name")
    Response.Write "</body></html>"
End Sub
%>"#;
        let (syms, edges) = extract_classic_asp(&rel("main.asp"), source);

        // File insight
        assert!(syms.iter().any(|s| s.name == "classic_asp_file"));

        // Function
        let funcs: Vec<_> = syms.iter().filter(|s| s.kind == "function").collect();
        assert!(funcs.iter().any(|f| f.name == "RenderPage"));

        // State
        let state_syms: Vec<_> = syms.iter().filter(|s| s.kind == "global_state").collect();
        assert!(state_syms.len() >= 3, "Session, QueryString, Cookies x2");

        // Includes
        let includes: Vec<_> = edges.iter().filter(|e| e.kind == "includes_file").collect();
        assert_eq!(includes.len(), 2);

        // COM insight
        let com_insights: Vec<_> = syms
            .iter()
            .filter(|s| s.name == "com_interop_usage")
            .collect();
        assert_eq!(com_insights.len(), 1);

        // Connection string
        let conn_syms: Vec<_> = syms
            .iter()
            .filter(|s| s.kind == "connection_string")
            .collect();
        assert_eq!(conn_syms.len(), 1);

        // SQL
        let sql_edges: Vec<_> = edges.iter().filter(|e| e.kind == "sql_calls").collect();
        assert_eq!(sql_edges.len(), 1);
        assert_eq!(sql_edges[0].target_name, "Users");

        // Navigation
        let deps: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].target_name, "login.asp");

        // Response.Write annotation
        let insight = syms.iter().find(|s| s.name == "classic_asp_file").unwrap();
        let meta = insight.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("uses_response_write").map(|s| s.as_str()),
            Some("true")
        );
    }

    // ── Edge cases ──

    #[test]
    fn test_empty_source() {
        let (syms, edges) = extract_classic_asp(&rel("empty.asp"), "");

        // Should still emit the file-level insight.
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "classic_asp_file");
        assert!(edges.is_empty());
    }

    #[test]
    fn test_comment_lines_skipped() {
        let source = r#"<%
' Session("DoNotDetect") = "ignored"
REM Session("AlsoIgnored") = "nope"
Session("Detected") = "yes"
%>"#;
        let (syms, edges) = extract_classic_asp(&rel("comments.asp"), source);

        let state_syms: Vec<_> = syms.iter().filter(|s| s.kind == "global_state").collect();
        assert_eq!(state_syms.len(), 1, "only non-comment line detected");
        assert_eq!(state_syms[0].name, "Session:Detected");

        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        assert_eq!(writes.len(), 1);
    }

    #[test]
    fn test_whitespace_tolerance() {
        let source = r#"<%
Set conn = Server.CreateObject( "ADODB.Connection" )
Dim x : x = Session( "Key" )
Response.Redirect( "next.asp" )
%>"#;
        let (syms, edges) = extract_classic_asp(&rel("ws.asp"), source);

        assert!(syms.iter().any(|s| s.name == "com_interop_usage"));
        assert!(syms.iter().any(|s| s.name == "Session:Key"));

        let deps: Vec<_> = edges.iter().filter(|e| e.kind == "dependency").collect();
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn test_dedup_com_objects() {
        let source = r#"<%
Set conn1 = Server.CreateObject("ADODB.Connection")
Set conn2 = Server.CreateObject("ADODB.Connection")
%>"#;
        let (syms, _) = extract_classic_asp(&rel("multi.asp"), source);

        let com: Vec<_> = syms
            .iter()
            .filter(|s| s.name == "com_interop_usage")
            .collect();
        assert_eq!(com.len(), 1, "same prog_id should be deduped");
    }

    #[test]
    fn test_multiple_com_objects() {
        let source = r#"<%
Set conn = Server.CreateObject("ADODB.Connection")
Set rs = Server.CreateObject("ADODB.Recordset")
Set cmd = Server.CreateObject("ADODB.Command")
%>"#;
        let (syms, _) = extract_classic_asp(&rel("multi_com.asp"), source);

        let com: Vec<_> = syms
            .iter()
            .filter(|s| s.name == "com_interop_usage")
            .collect();
        assert_eq!(com.len(), 3, "different prog_ids are separate");
    }
}
