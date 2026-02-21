/// JavaScript → ASP.NET bridge extractor.
///
/// Scans `.js` files for patterns that reference server-side ASP.NET controls
/// and endpoints, emitting edges that link the client-side layer to the
/// server-side graph nodes already produced by `webforms.rs` and
/// `config_extractor.rs`.
///
/// **Feature 3 — Frontend Bridge (JS to ASP.NET Control Mapper)**
///   - jQuery `$("[id$='CtrlId']")` / `$("[id$=CtrlId]")` selectors
///   - ASP.NET inline `<%= Ctrl.ClientID %>` expressions embedded in JS
///   - `__doPostBack('CtrlId', ...)` postback hijacks
///
/// **Feature 4 — Legacy AJAX & WebMethod Mapper**
///   - `$.ajax({ url: '...' })` calls
///   - `$.get('...')` / `$.post('...')` shorthand
///   - `fetch('...')` calls (modern JS in legacy codebases)
///   - `XMLHttpRequest.open('...', '...')` raw XHR calls
///   - `PageMethods.MethodName(...)` ASP.NET AJAX ScriptManager calls
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

/// Maximum source size we'll run regex extraction on (5 MiB).
/// Minified vendor bundles can be huge; we skip them to keep indexing bounded.
const MAX_JS_SOURCE_BYTES: usize = 5 * 1024 * 1024;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

// Feature 3: Frontend Bridge patterns
static JQUERY_ENDS_WITH_RE: OnceLock<Regex> = OnceLock::new();
static ASP_CLIENT_ID_RE: OnceLock<Regex> = OnceLock::new();
static POSTBACK_RE: OnceLock<Regex> = OnceLock::new();
static DOCUMENT_GETELEMENTBYID_RE: OnceLock<Regex> = OnceLock::new();

// Feature 4: AJAX / WebMethod patterns
static AJAX_CALL_RE: OnceLock<Regex> = OnceLock::new();
static AJAX_SHORTHAND_RE: OnceLock<Regex> = OnceLock::new();
static FETCH_CALL_RE: OnceLock<Regex> = OnceLock::new();
static XHR_OPEN_RE: OnceLock<Regex> = OnceLock::new();
static PAGE_METHODS_RE: OnceLock<Regex> = OnceLock::new();

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

/// Build a byte-offset → 1-based line number lookup using memchr.
fn build_line_index(source: &str) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut line_starts: Vec<usize> = vec![0];
    let mut pos = 0;
    while pos < bytes.len() {
        if let Some(nl) = memchr::memchr(b'\n', &bytes[pos..]) {
            line_starts.push(pos + nl + 1);
            pos += nl + 1;
        } else {
            break;
        }
    }
    line_starts
}

/// Return 1-based line number for byte offset.
fn line_of(line_starts: &[usize], byte_offset: usize) -> u32 {
    match line_starts.binary_search(&byte_offset) {
        Ok(idx) => (idx + 1) as u32,
        Err(idx) => idx as u32,
    }
}

// ── Relative-path derivation ────────────────────────────────────────────────

/// Derive a display-friendly relative path from the file's `Path`.
/// Returns the file name with extension (e.g., `"Scripts/map_utils.js"`).
fn source_file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown.js")
        .to_string()
}

// ── URL path splitting ──────────────────────────────────────────────────────

/// Split an AJAX URL into `(service_path, method_name)`.
///
/// Examples:
///   `"Services/MapData.asmx/GetPolygons"` → `("Services/MapData.asmx", Some("GetPolygons"))`
///   `"api/Patients/Search"` → `("api/Patients/Search", None)` (no known service ext)
///   `"../Handlers/Export.ashx"` → `("Handlers/Export.ashx", None)` (no method suffix)
fn split_service_url(raw_url: &str) -> (String, Option<String>) {
    // Strip query string and fragment
    let url = raw_url.split('?').next().unwrap_or(raw_url);
    let url = url.split('#').next().unwrap_or(url);
    // Normalise leading ../ and ./ segments
    let url = url.trim_start_matches("../").trim_start_matches("./");
    // Strip leading ~/
    let url = url.strip_prefix("~/").unwrap_or(url);
    // Strip leading /
    let url = url.strip_prefix('/').unwrap_or(url);

    // Known service extensions that can have a /MethodName suffix
    let service_exts = [".asmx", ".svc", ".ashx"];
    let lower = url.to_lowercase();

    for ext in &service_exts {
        if let Some(ext_pos) = lower.find(ext) {
            let after_ext = ext_pos + ext.len();
            if after_ext < url.len() && url.as_bytes()[after_ext] == b'/' {
                let path_part = &url[..after_ext];
                let method_part = &url[after_ext + 1..];
                if !method_part.is_empty() && !method_part.contains('/') {
                    return (path_part.to_string(), Some(method_part.to_string()));
                }
            }
            // Extension found but no /Method suffix
            return (url[..after_ext].to_string(), None);
        }
    }

    // No known service extension — could be a Web API route or page method
    // Try splitting on last '/' if the final segment looks like a method name
    // (PascalCase, no dots, no extension)
    if let Some(slash_pos) = url.rfind('/') {
        let tail = &url[slash_pos + 1..];
        if !tail.is_empty()
            && !tail.contains('.')
            && tail.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        {
            return (url[..slash_pos].to_string(), Some(tail.to_string()));
        }
    }

    (url.to_string(), None)
}

// ── Core extraction ─────────────────────────────────────────────────────────

/// Extract ASP.NET-related edges from a JavaScript source file.
///
/// This runs **in addition to** the default tree-sitter JS extraction (which
/// captures functions, classes, imports, and call edges). The JS extractor
/// focuses exclusively on cross-boundary edges that tree-sitter cannot detect:
/// jQuery control selectors, `__doPostBack`, and AJAX service calls.
pub fn extract_js(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut edges = Vec::new();
    let syms = Vec::new(); // JS extractor only produces edges, not symbols

    if source.len() > MAX_JS_SOURCE_BYTES {
        tracing::warn!(
            "js_extractor: skipping {} ({}B > {}B limit)",
            path.display(),
            source.len(),
            MAX_JS_SOURCE_BYTES
        );
        return (syms, edges);
    }

    let line_starts = build_line_index(source);
    let file_name = source_file_name(path);

    // ── Feature 3: DOM manipulation edges ───────────────────────────────

    extract_jquery_selectors(source, &line_starts, &file_name, &mut edges);
    extract_asp_client_ids(source, &line_starts, &file_name, &mut edges);
    extract_postbacks(source, &line_starts, &file_name, &mut edges);
    extract_getelementbyid(source, &line_starts, &file_name, &mut edges);

    // ── Feature 4: AJAX / WebMethod edges ───────────────────────────────

    extract_ajax_calls(source, &line_starts, &file_name, &mut edges);
    extract_ajax_shorthand(source, &line_starts, &file_name, &mut edges);
    extract_fetch_calls(source, &line_starts, &file_name, &mut edges);
    extract_xhr_calls(source, &line_starts, &file_name, &mut edges);
    extract_page_methods(source, &line_starts, &file_name, &mut edges);

    // Deduplicate: same (source, target, kind) triple should only appear once.
    dedup_edges(&mut edges);

    (syms, edges)
}

/// Check if a file extension is a JavaScript file that should have the
/// JS bridge extractor run on it.
pub fn is_js_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some("js")
    )
}

// ── Feature 3 extractors ────────────────────────────────────────────────────

/// jQuery `$("[id$='CtrlId']")` and `$("[id$=CtrlId]")` (ends-with selector).
///
/// Matches both single and double quotes, and the optional `$=` vs `$='`:
///   `$("[id$='txtSearch']")`
///   `$('[id$=txtSearch]')`
///   `$("[id$=\"btnSubmit\"]")`
fn extract_jquery_selectors(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &JQUERY_ENDS_WITH_RE,
        r#"(?i)\$\(\s*['"](?:[^'"]*?)id\$?=['"]?(?P<ctrl_id>[A-Za-z0-9_]+)['"]?"#,
        "JQUERY_ENDS_WITH",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let ctrl_id = &cap["ctrl_id"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: ctrl_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "manipulates_dom",
            metadata: Some(HashMap::from([(
                "selector_type".into(),
                "jquery_ends_with".into(),
            )])),
        });
    }
}

/// Inline `<%= CtrlId.ClientID %>` expressions embedded inside JS strings.
///
/// These appear in inline `<script>` blocks inside ASPX pages, but also in
/// external `.js` files that are generated server-side or use server-side
/// includes.
fn extract_asp_client_ids(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &ASP_CLIENT_ID_RE,
        r#"(?i)<%=\s*(?P<ctrl_id>[A-Za-z0-9_]+)\.ClientID\s*%>"#,
        "ASP_CLIENT_ID",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let ctrl_id = &cap["ctrl_id"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: ctrl_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "manipulates_dom",
            metadata: Some(HashMap::from([(
                "selector_type".into(),
                "asp_client_id".into(),
            )])),
        });
    }
}

/// `__doPostBack('CtrlUniqueId', 'arg')` postback hijack calls.
///
/// The first argument to `__doPostBack` is the UniqueID of the control
/// (e.g., `"ctl00$MainContent$gvPatients"` or `"btnRefresh"`).
fn extract_postbacks(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &POSTBACK_RE,
        r#"(?i)__doPostBack\s*\(\s*['"](?P<ctrl_id>[^'"]+)['"]"#,
        "POSTBACK",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let ctrl_id = &cap["ctrl_id"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        // Extract the short control name (last segment after $)
        let short_id = ctrl_id.rsplit('$').next().unwrap_or(ctrl_id);

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: short_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "triggers_postback",
            metadata: Some(HashMap::from([("unique_id".into(), ctrl_id.to_string())])),
        });
    }
}

/// `document.getElementById('CtrlId')` or `document.getElementById('<%= Ctrl.ClientID %>')`.
///
/// Falls back to the raw string ID when the ASP expression is not present.
fn extract_getelementbyid(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &DOCUMENT_GETELEMENTBYID_RE,
        r#"(?i)document\.getElementById\s*\(\s*['"](?P<ctrl_id>[A-Za-z0-9_]+)['"]"#,
        "DOCUMENT_GETELEMENTBYID",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let ctrl_id = &cap["ctrl_id"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: ctrl_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "manipulates_dom",
            metadata: Some(HashMap::from([(
                "selector_type".into(),
                "getelementbyid".into(),
            )])),
        });
    }
}

// ── Feature 4 extractors ────────────────────────────────────────────────────

/// `$.ajax({ url: 'Services/MapData.asmx/GetPolygons', ... })`.
///
/// Matches both `url:` and `url :` with any whitespace, and single/double quotes.
fn extract_ajax_calls(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &AJAX_CALL_RE,
        r#"(?i)\$\.ajax\(\s*\{[^}]*url\s*:\s*['"](?P<url>[^'"]+)['"]"#,
        "AJAX_CALL",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let url = &cap["url"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);
        emit_ajax_edge(edges, file_name, line, url, "jquery_ajax");
    }
}

/// `$.get('url')` / `$.post('url')` / `$.getJSON('url')`.
fn extract_ajax_shorthand(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &AJAX_SHORTHAND_RE,
        r#"(?i)\$\.(?:get|post|getJSON)\(\s*['"](?P<url>[^'"]+)['"]"#,
        "AJAX_SHORTHAND",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let url = &cap["url"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);
        emit_ajax_edge(edges, file_name, line, url, "jquery_shorthand");
    }
}

/// `fetch('url')` / `fetch("url")`.
fn extract_fetch_calls(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &FETCH_CALL_RE,
        r#"(?i)\bfetch\(\s*['"](?P<url>[^'"]+)['"]"#,
        "FETCH_CALL",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let url = &cap["url"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);
        emit_ajax_edge(edges, file_name, line, url, "fetch");
    }
}

/// `xhr.open('GET', 'url')` / `new XMLHttpRequest(); ... xhr.open('POST', 'url')`.
fn extract_xhr_calls(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &XHR_OPEN_RE,
        r#"(?i)\.open\(\s*['"](?:GET|POST|PUT|DELETE|PATCH)['"],\s*['"](?P<url>[^'"]+)['"]"#,
        "XHR_OPEN",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let url = &cap["url"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);
        emit_ajax_edge(edges, file_name, line, url, "xhr");
    }
}

/// `PageMethods.MethodName(args, onSuccess, onFailure)` — ASP.NET AJAX ScriptManager.
///
/// These are generated by `<asp:ScriptManager EnablePageMethods="true" />` and
/// route to `[WebMethod]`-decorated static methods on the page's code-behind.
fn extract_page_methods(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &PAGE_METHODS_RE,
        r#"(?i)PageMethods\.(?P<method>[A-Za-z_][A-Za-z0-9_]*)\s*\("#,
        "PAGE_METHODS",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let method_name = &cap["method"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        let mut meta = HashMap::with_capacity(2);
        meta.insert("ajax_transport".into(), "page_methods".into());
        meta.insert("ajax_target_method".into(), method_name.to_string());

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: method_name.to_string(),
            target_kind: Some("function"),
            target_start_line: None,
            kind: "api_call",
            metadata: Some(meta),
        });
    }
}

// ── Shared AJAX edge emitter ────────────────────────────────────────────────

/// Emit an `api_call` edge from a JS file to a resolved service path.
///
/// Splits the URL into `(path, method)` using `split_service_url`, determines
/// the target kind based on the path extension, and attaches `ajax_target_method`
/// metadata when a method name is extracted.
fn emit_ajax_edge(
    edges: &mut Vec<ExtractedEdge>,
    file_name: &str,
    line: u32,
    raw_url: &str,
    transport: &str,
) {
    // Skip data URIs, javascript:, and anchors
    let lower = raw_url.to_lowercase();
    if lower.starts_with("data:")
        || lower.starts_with("javascript:")
        || lower.starts_with('#')
        || lower.is_empty()
    {
        return;
    }

    let (path_part, method_part) = split_service_url(raw_url);

    // Determine target_kind from path extension
    let path_lower = path_part.to_lowercase();
    let target_kind = if path_lower.ends_with(".asmx") {
        "web_service"
    } else if path_lower.ends_with(".ashx") {
        "http_handler"
    } else if path_lower.ends_with(".svc") {
        "wcf_service"
    } else if path_lower.ends_with(".aspx") {
        "page"
    } else {
        "endpoint"
    };

    let mut meta = HashMap::with_capacity(3);
    meta.insert("ajax_transport".into(), transport.into());
    meta.insert("ajax_url".into(), raw_url.to_string());
    if let Some(ref method) = method_part {
        meta.insert("ajax_target_method".into(), method.clone());
    }

    edges.push(ExtractedEdge {
        source_name: file_name.to_string(),
        source_kind: "file",
        source_start_line: line,
        source_language: "javascript",
        target_name: path_part,
        target_kind: Some(target_kind),
        target_start_line: None,
        kind: "api_call",
        metadata: Some(meta),
    });
}

// ── Deduplication ───────────────────────────────────────────────────────────

/// Remove duplicate edges with the same `(source_name, target_name, kind)` triple.
/// Keeps the first occurrence (lowest line number).
fn dedup_edges(edges: &mut Vec<ExtractedEdge>) {
    let mut seen = HashSet::with_capacity(edges.len());
    edges.retain(|e| {
        let key = format!("{}|{}|{}", e.source_name, e.target_name, e.kind);
        seen.insert(key)
    });
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_path(name: &str) -> PathBuf {
        PathBuf::from(format!("Scripts/{name}"))
    }

    // ── Feature 3: jQuery ends-with selector ────────────────────────────

    #[test]
    fn jquery_ends_with_single_quotes() {
        let js = r#"var el = $("[id$='txtSearch']");"#;
        let (_, edges) = extract_js(&test_path("search.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "txtSearch");
        assert_eq!(edges[0].kind, "manipulates_dom");
        assert_eq!(edges[0].source_language, "javascript");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("selector_type").map(|s| s.as_str()),
            Some("jquery_ends_with")
        );
    }

    #[test]
    fn jquery_ends_with_double_quotes() {
        let js = r#"var el = $('[id$="btnSubmit"]');"#;
        let (_, edges) = extract_js(&test_path("form.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "btnSubmit");
        assert_eq!(edges[0].kind, "manipulates_dom");
    }

    #[test]
    fn jquery_ends_with_no_quotes_around_value() {
        let js = r#"var el = $("[id$=ddlCountry]");"#;
        let (_, edges) = extract_js(&test_path("form.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "ddlCountry");
    }

    // ── Feature 3: ASP.NET ClientID ─────────────────────────────────────

    #[test]
    fn asp_client_id_expression() {
        let js = r#"var id = '<%= txtFirstName.ClientID %>';"#;
        let (_, edges) = extract_js(&test_path("inline.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "txtFirstName");
        assert_eq!(edges[0].kind, "manipulates_dom");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("selector_type").map(|s| s.as_str()),
            Some("asp_client_id")
        );
    }

    // ── Feature 3: __doPostBack ─────────────────────────────────────────

    #[test]
    fn postback_simple_id() {
        let js = r#"__doPostBack('btnRefresh', '');"#;
        let (_, edges) = extract_js(&test_path("postback.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "btnRefresh");
        assert_eq!(edges[0].kind, "triggers_postback");
    }

    #[test]
    fn postback_unique_id_extracts_short_name() {
        let js = r#"__doPostBack('ctl00$MainContent$gvPatients', '');"#;
        let (_, edges) = extract_js(&test_path("grid.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "gvPatients");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("unique_id").map(|s| s.as_str()),
            Some("ctl00$MainContent$gvPatients")
        );
    }

    // ── Feature 3: document.getElementById ──────────────────────────────

    #[test]
    fn getelementbyid_simple() {
        let js = r#"var el = document.getElementById('pnlDetails');"#;
        let (_, edges) = extract_js(&test_path("utils.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "pnlDetails");
        assert_eq!(edges[0].kind, "manipulates_dom");
    }

    // ── Feature 4: $.ajax ───────────────────────────────────────────────

    #[test]
    fn ajax_call_with_method() {
        let js = r#"
            $.ajax({
                url: 'Services/MapData.asmx/GetPolygons',
                type: 'POST',
                success: function(data) {}
            });
        "#;
        let (_, edges) = extract_js(&test_path("map_utils.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/MapData.asmx");
        assert_eq!(edges[0].kind, "api_call");
        assert_eq!(edges[0].target_kind.as_deref(), Some("web_service"));
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("GetPolygons")
        );
        assert_eq!(
            meta.get("ajax_transport").map(|s| s.as_str()),
            Some("jquery_ajax")
        );
    }

    #[test]
    fn ajax_call_ashx_no_method() {
        let js = r#"$.ajax({ url: 'Handlers/Export.ashx', type: 'GET' });"#;
        let (_, edges) = extract_js(&test_path("export.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Handlers/Export.ashx");
        assert_eq!(edges[0].target_kind.as_deref(), Some("http_handler"));
    }

    // ── Feature 4: $.get / $.post ───────────────────────────────────────

    #[test]
    fn ajax_shorthand_get() {
        let js = r#"$.get('Services/LookupData.asmx/GetCities', function(data) {});"#;
        let (_, edges) = extract_js(&test_path("lookup.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/LookupData.asmx");
        assert_eq!(edges[0].kind, "api_call");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("GetCities")
        );
    }

    #[test]
    fn ajax_shorthand_post() {
        let js = r#"$.post('api/Patients/Save', data, onSuccess);"#;
        let (_, edges) = extract_js(&test_path("patient.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "api/Patients");
        assert_eq!(edges[0].kind, "api_call");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("Save")
        );
    }

    #[test]
    fn ajax_shorthand_getjson() {
        let js = r#"$.getJSON('Services/Config.svc/GetSettings', callback);"#;
        let (_, edges) = extract_js(&test_path("config.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/Config.svc");
        assert_eq!(edges[0].target_kind.as_deref(), Some("wcf_service"));
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("GetSettings")
        );
    }

    // ── Feature 4: fetch() ──────────────────────────────────────────────

    #[test]
    fn fetch_call_asmx() {
        let js = r#"fetch('Services/UserData.asmx/GetProfile').then(r => r.json());"#;
        let (_, edges) = extract_js(&test_path("profile.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/UserData.asmx");
        assert_eq!(edges[0].kind, "api_call");
    }

    // ── Feature 4: XMLHttpRequest.open ──────────────────────────────────

    #[test]
    fn xhr_open_call() {
        let js = r#"
            var xhr = new XMLHttpRequest();
            xhr.open('POST', 'Services/Report.asmx/Generate');
            xhr.send(data);
        "#;
        let (_, edges) = extract_js(&test_path("report.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/Report.asmx");
        assert_eq!(edges[0].kind, "api_call");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("Generate")
        );
    }

    // ── Feature 4: PageMethods ──────────────────────────────────────────

    #[test]
    fn page_methods_call() {
        let js = r#"PageMethods.GetPatientList(searchTerm, onSuccess, onFailure);"#;
        let (_, edges) = extract_js(&test_path("patients.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "GetPatientList");
        assert_eq!(edges[0].target_kind.as_deref(), Some("function"));
        assert_eq!(edges[0].kind, "api_call");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_transport").map(|s| s.as_str()),
            Some("page_methods")
        );
    }

    // ── URL splitting ───────────────────────────────────────────────────

    #[test]
    fn split_url_asmx_with_method() {
        let (path, method) = split_service_url("Services/MapData.asmx/GetPolygons");
        assert_eq!(path, "Services/MapData.asmx");
        assert_eq!(method.as_deref(), Some("GetPolygons"));
    }

    #[test]
    fn split_url_ashx_no_method() {
        let (path, method) = split_service_url("Handlers/Export.ashx");
        assert_eq!(path, "Handlers/Export.ashx");
        assert!(method.is_none());
    }

    #[test]
    fn split_url_with_query_string() {
        let (path, method) = split_service_url("Services/Data.asmx/GetRows?filter=active&page=1");
        assert_eq!(path, "Services/Data.asmx");
        assert_eq!(method.as_deref(), Some("GetRows"));
    }

    #[test]
    fn split_url_relative_dot_dot() {
        let (path, method) = split_service_url("../Services/Auth.asmx/Login");
        assert_eq!(path, "Services/Auth.asmx");
        assert_eq!(method.as_deref(), Some("Login"));
    }

    #[test]
    fn split_url_tilde_prefix() {
        let (path, method) = split_service_url("~/Services/Data.svc/GetItems");
        assert_eq!(path, "Services/Data.svc");
        assert_eq!(method.as_deref(), Some("GetItems"));
    }

    #[test]
    fn split_url_api_route() {
        let (path, method) = split_service_url("api/Patients/Search");
        assert_eq!(path, "api/Patients");
        assert_eq!(method.as_deref(), Some("Search"));
    }

    // ── Edge case: skipped files ────────────────────────────────────────

    #[test]
    fn oversized_file_returns_empty() {
        let huge = "x".repeat(MAX_JS_SOURCE_BYTES + 1);
        let (syms, edges) = extract_js(&test_path("huge.js"), &huge);
        assert!(syms.is_empty());
        assert!(edges.is_empty());
    }

    // ── Deduplication ───────────────────────────────────────────────────

    #[test]
    fn dedup_same_target_multiple_lines() {
        let js = r#"
            var a = $("[id$='txtName']");
            var b = $("[id$='txtName']");
        "#;
        let (_, edges) = extract_js(&test_path("dup.js"), js);
        // Same (source, target, kind) → deduplicated to 1
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "txtName");
    }

    // ── Mixed scenario ──────────────────────────────────────────────────

    #[test]
    fn mixed_dom_and_ajax_edges() {
        let js = r#"
            $(document).ready(function() {
                var grid = $("[id$='gvResults']");
                __doPostBack('btnSearch', '');
                $.ajax({
                    url: 'Services/Search.asmx/FindRecords',
                    type: 'POST',
                    data: JSON.stringify({ q: query }),
                    success: function(data) {
                        PageMethods.FormatResults(data, onDone, onFail);
                    }
                });
            });
        "#;
        let (_, edges) = extract_js(&test_path("search_page.js"), js);
        // Should have: gvResults (manipulates_dom), btnSearch (triggers_postback),
        // Search.asmx (api_call), FormatResults (api_call)
        assert_eq!(edges.len(), 4);

        let dom_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "manipulates_dom")
            .collect();
        assert_eq!(dom_edges.len(), 1);
        assert_eq!(dom_edges[0].target_name, "gvResults");

        let postback_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "triggers_postback")
            .collect();
        assert_eq!(postback_edges.len(), 1);
        assert_eq!(postback_edges[0].target_name, "btnSearch");

        let api_edges: Vec<_> = edges.iter().filter(|e| e.kind == "api_call").collect();
        assert_eq!(api_edges.len(), 2);

        let asmx_edge = api_edges
            .iter()
            .find(|e| e.target_name == "Services/Search.asmx");
        assert!(asmx_edge.is_some());

        let pm_edge = api_edges.iter().find(|e| e.target_name == "FormatResults");
        assert!(pm_edge.is_some());
    }

    // ── Data URIs and skip patterns ─────────────────────────────────────

    #[test]
    fn skip_data_uri_in_ajax() {
        let js = r#"$.ajax({ url: 'data:text/plain;base64,abc' });"#;
        let (_, edges) = extract_js(&test_path("skip.js"), js);
        assert!(edges.is_empty());
    }

    #[test]
    fn skip_javascript_uri_in_fetch() {
        let js = r#"fetch('javascript:void(0)');"#;
        let (_, edges) = extract_js(&test_path("skip.js"), js);
        assert!(edges.is_empty());
    }
}
