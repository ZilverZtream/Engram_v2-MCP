/// WebForms event wiring extractor.
///
/// Parses .aspx / .ascx / .master files for:
///   - `<%@ Page Inherits="Namespace.ClassName" CodeBehind="Foo.aspx.cs" %>`
///   - `<%@ Control Inherits="..." %>`
///   - Control IDs: `ID="btnSubmit"` → node of kind "control"
///   - Event attributes: `OnClick="ButtonClick"` → edge from control to handler
///
/// P0.7 additions:
///   - Code-behind path normalization for both `.cs` and `.vb` (Windows `\` + Linux `/`).
///   - Emits three edge kinds for code-behind wiring:
///       1. `codebehind`  — markup file → code-behind file  (with normalized `rel_path` in meta)
///       2. `inherits`    — markup file → code-behind class (FQN in meta)
///       3. `cb_defines`  — code-behind file → code-behind class (links file node to class node)
///
/// P11 additions (legacy WebForms deep extraction):
///   4. `<%@ Register %>` directive parsing — captures Src/TagPrefix/TagName, emits
///      `registers_control` edges from the page to the referenced `.ascx` user control.
///   5. User control tag resolution — when `<uc1:Menu>` is found, resolves the tag prefix
///      against a Register table and emits a `registers_control` edge to the `.ascx` file.
///   6. Data-source controls — `<asp:SqlDataSource>`, `<asp:ObjectDataSource>`, etc.
///      Emits `sql_calls` edges for inline SQL (SelectCommand, etc.) and `event_wiring`
///      edges for SelectMethod/InsertMethod bindings.
///   7. Data-binding expressions — `<%# Eval("FieldName") %>`, `<%# Bind("FieldName") %>`.
///      Emits `data_binding` edges from the page to a `binding_field:FieldName` virtual node.
///
/// Emits `ExtractedSymbol` (controls + page) and `ExtractedEdge` (all of the above).
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

static DIRECTIVE_RE: OnceLock<Regex> = OnceLock::new();
static INHERITS_RE: OnceLock<Regex> = OnceLock::new();
static CODEBEHIND_RE: OnceLock<Regex> = OnceLock::new();
static CONTROL_RE: OnceLock<Regex> = OnceLock::new();
static ID_RE: OnceLock<Regex> = OnceLock::new();
static HTML_CONTROL_RE: OnceLock<Regex> = OnceLock::new();
static EVENT_ATTR_RE: OnceLock<Regex> = OnceLock::new();
static REGISTER_RE: OnceLock<Regex> = OnceLock::new();
static DATASOURCE_RE: OnceLock<Regex> = OnceLock::new();
static DATA_BINDING_RE: OnceLock<Regex> = OnceLock::new();
static TAGPREFIX_RE: OnceLock<Regex> = OnceLock::new();
static TAGNAME_RE: OnceLock<Regex> = OnceLock::new();
static SRC_RE: OnceLock<Regex> = OnceLock::new();
static ASSEMBLY_RE: OnceLock<Regex> = OnceLock::new();
static NAMESPACE_RE: OnceLock<Regex> = OnceLock::new();
static DS_SELECT_CMD_RE: OnceLock<Regex> = OnceLock::new();
static DS_INSERT_CMD_RE: OnceLock<Regex> = OnceLock::new();
static DS_UPDATE_CMD_RE: OnceLock<Regex> = OnceLock::new();
static DS_DELETE_CMD_RE: OnceLock<Regex> = OnceLock::new();
static DS_SELECT_METHOD_RE: OnceLock<Regex> = OnceLock::new();
static DS_INSERT_METHOD_RE: OnceLock<Regex> = OnceLock::new();
static DS_UPDATE_METHOD_RE: OnceLock<Regex> = OnceLock::new();
static DS_DELETE_METHOD_RE: OnceLock<Regex> = OnceLock::new();
static DS_TYPE_NAME_RE: OnceLock<Regex> = OnceLock::new();

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

/// A registered user control prefix from `<%@ Register %>` directives.
#[derive(Debug)]
struct RegisterEntry {
    tag_prefix: String,
    tag_name: String,
    /// Resolved project-relative path to the .ascx file (if Src-based).
    src_rel_path: Option<String>,
    /// Assembly-based registration (TagPrefix + Namespace + Assembly).
    /// Stored for future assembly-resolution passes.
    #[allow(dead_code)]
    assembly: Option<String>,
    #[allow(dead_code)]
    namespace: Option<String>,
}

/// Extract symbols and edges from a WebForms markup file.
///
/// `project_root` is the root directory of the project.
/// `rel_path` is the project-relative path to the markup file.
pub fn extract_webforms(
    project_root: &Path,
    rel_path: &RelPath,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    // Track line numbers by character offset.
    let line_offsets: Vec<usize> = {
        let mut offsets = vec![0usize];
        for (i, c) in source.char_indices() {
            if c == '\n' {
                offsets.push(i + 1);
            }
        }
        offsets
    };

    let char_to_line = |char_pos: usize| -> u32 {
        match line_offsets.binary_search(&char_pos) {
            Ok(line) => line as u32,
            Err(line) => line.saturating_sub(1) as u32,
        }
    };

    let file_name = rel_path
        .file_name()
        .unwrap_or_else(|| rel_path.as_str())
        .to_string();

    // ── 0. Find Inherits FQN (Pass 0) ──────────────────────────────────────────
    // Match: <%@ (Page|Control|Master) ... %>
    let Some(directive_re) = get_compiled_regex(
        &DIRECTIVE_RE,
        r"(?i)<%@\s*(?:Page|Control|Master)\b([^%]*)%>",
        "webforms_directive",
    ) else {
        return (symbols, edges);
    };
    let Some(inherits_re) = get_compiled_regex(
        &INHERITS_RE,
        r#"(?i)Inherits\s*=\s*"([^"]+)""#,
        "webforms_inherits",
    ) else {
        return (symbols, edges);
    };
    let Some(codebehind_re) = get_compiled_regex(
        &CODEBEHIND_RE,
        r#"(?i)Code(?:Behind|File)\s*=\s*"([^"]+)""#,
        "webforms_codebehind",
    ) else {
        return (symbols, edges);
    };

    let mut page_inherits_fqn: Option<String> = None;
    if let Some(m) = directive_re.find(source)
        && let Some(cap) = inherits_re.captures(m.as_str())
    {
        page_inherits_fqn = Some(cap[1].trim().to_string());
    }

    // ── 1. Page / Control / Master directive ──────────────────────────────────
    for m in directive_re.find_iter(source) {
        let attrs = m.as_str();
        let line = char_to_line(m.start());

        // Extract Inherits FQN (present in both C# and VB code-behind directives).
        let inherits_cap = inherits_re.captures(attrs);
        // Extract CodeBehind / CodeFile path.
        let codebehind_cap = codebehind_re.captures(attrs);

        // Only emit page symbol + edges when we have at least one of the two.
        if inherits_cap.is_none() && codebehind_cap.is_none() {
            continue;
        }

        // Emit a "page" symbol for the markup file itself.
        symbols.push(ExtractedSymbol {
            name: file_name.clone(),
            kind: "page".into(),
            start_line: line,
            end_line: line,
            metadata: None,
        });

        // Compute the normalized code-behind relative path (P0.1 fix).
        let cb_rel_path: Option<String> = codebehind_cap.as_ref().map(|cap| {
            let raw = cap[1].trim();
            let p = Path::new(raw);

            if p.is_absolute() {
                // If it's absolute, try to make it relative to project_root if it's inside
                if let Some(rel) = RelPath::from_relative(project_root, p) {
                    return rel.as_str().to_string();
                }
                raw.replace('\\', "/")
            } else {
                // Resolve relative to the markup file's parent directory.
                if let Some(parent_abs) = project_root.join(rel_path.as_str()).parent() {
                    let abs_cb = parent_abs.join(raw);
                    // Lexically normalize to handle any ".." in raw path
                    let normalized_abs = lexically_normalize(&abs_cb);
                    if let Some(rel) = RelPath::from_relative(project_root, &normalized_abs) {
                        return rel.as_str().to_string();
                    }
                }
                raw.replace('\\', "/")
            }
        });

        // ── Edge 1: markup → codebehind file (codebehind) ───────────────────
        if let Some(ref cb_path) = cb_rel_path {
            let mut meta = HashMap::new();
            meta.insert("relative_path".into(), cb_path.clone());

            // Detect language from extension (.cs or .vb).
            let cb_lang = if cb_path.to_lowercase().ends_with(".vb") {
                "vb"
            } else {
                "csharp"
            };
            meta.insert("language".into(), cb_lang.into());

            edges.push(ExtractedEdge {
                source_name: "file".into(),
                source_kind: "page".into(),
                source_start_line: line,
                source_language: "aspx".into(),
                target_name: cb_path.clone(),
                target_kind: Some("file".into()),
                target_start_line: None,
                kind: "codebehind_file".into(),
                metadata: Some(meta),
            });
        }

        if let Some(cap) = &inherits_cap {
            let class_name_fqn = cap[1].trim().to_string();
            let simple_name = class_name_fqn
                .split('.')
                .next_back()
                .unwrap_or(&class_name_fqn)
                .to_string();

            let mut meta = HashMap::new();
            meta.insert("fqn".into(), class_name_fqn.clone());

            // ── Edge 2: markup → codebehind class (codebehind_class) ────────────────
            edges.push(ExtractedEdge {
                source_name: "file".into(),
                source_kind: "page".into(),
                source_start_line: line,
                source_language: "aspx".into(),
                target_name: simple_name.clone(),
                target_kind: Some("class".into()),
                target_start_line: None,
                kind: "codebehind_class".into(),
                metadata: Some(meta.clone()),
            });

            // ── Edge 3: codebehind file → codebehind class (cb_defines) ─────
            // This edge lets graph traversal go from the file node to the class
            // node without going through the markup.
            if let Some(ref cb_path) = cb_rel_path {
                edges.push(ExtractedEdge {
                    source_name: cb_path.clone(),
                    source_kind: "file".into(),
                    source_start_line: 0,
                    source_language: "aspx".into(),
                    target_name: simple_name,
                    target_kind: Some("class".into()),
                    target_start_line: None,
                    kind: "cb_defines".into(),
                    metadata: Some(meta),
                });
            }
        }
    }

    // ── 2. Server controls ────────────────────────────────────────────────────
    // Match tags with runat="server" and an ID attribute.
    let Some(control_re) = get_compiled_regex(
        &CONTROL_RE,
        r#"(?i)<(?:asp|ajaxToolkit|custom):[A-Za-z]+\b([^>]*runat\s*=\s*"server"[^>]*)/?>"#,
        "webforms_control",
    ) else {
        return (symbols, edges);
    };
    let Some(id_re) = get_compiled_regex(&ID_RE, r#"(?i)\bID\s*=\s*"([^"]+)""#, "webforms_id")
    else {
        return (symbols, edges);
    };

    // Also match plain HTML controls with runat="server".
    let Some(html_control_re) = get_compiled_regex(
        &HTML_CONTROL_RE,
        r#"(?i)<(?:input|select|textarea|button|form)\b([^>]*runat\s*=\s*"server"[^>]*)/?>"#,
        "webforms_html_control",
    ) else {
        return (symbols, edges);
    };

    // Single combined regex for all 22 event attributes — compiled once.
    let Some(event_attr_re) = get_compiled_regex(
        &EVENT_ATTR_RE,
        r#"(?i)\b(OnClick|OnCommand|OnTextChanged|OnSelectedIndexChanged|OnCheckedChanged|OnValueChanged|OnLoad|OnPreRender|OnInit|OnDataBound|OnRowCommand|OnRowEditing|OnRowUpdating|OnRowDeleting|OnRowCancelingEdit|OnPageIndexChanging|OnSorting|OnItemCommand|OnItemDataBound|OnServerClick|OnServerChange|OnServerValidate)\s*=\s*"([^"]+)""#,
        "webforms_event_attr",
    ) else {
        return (symbols, edges);
    };

    let extract_controls = |tag_attrs: &str,
                            tag_line: u32,
                            inherits_fqn: Option<&str>|
     -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
        let mut s = Vec::new();
        let mut e = Vec::new();
        if let Some(cap) = id_re.captures(tag_attrs) {
            let ctrl_id = cap[1].trim().to_string();
            let mut symbol_meta = HashMap::new();
            symbol_meta.insert("control_id".into(), ctrl_id.clone());

            s.push(ExtractedSymbol {
                name: ctrl_id.clone(),
                kind: "control".into(),
                start_line: tag_line,
                end_line: tag_line,
                metadata: Some(symbol_meta),
            });

            // Check for event wiring attributes using the single combined regex.
            for ecap in event_attr_re.captures_iter(tag_attrs) {
                let attr = ecap[1].to_string();
                let handler = ecap[2].trim().to_string();
                let mut meta = HashMap::new();
                meta.insert("event".into(), attr);
                if let Some(ifqn) = inherits_fqn {
                    meta.insert("fqn".into(), format!("{}.{}", ifqn, handler));
                }

                e.push(ExtractedEdge {
                    source_name: ctrl_id.clone(),
                    source_kind: "control".into(),
                    source_start_line: tag_line,
                    source_language: "aspx".into(),
                    target_name: handler,
                    target_kind: Some("function".into()),
                    target_start_line: None,
                    kind: "event_wiring".into(),
                    metadata: if meta.is_empty() { None } else { Some(meta) },
                });
            }
        }
        (s, e)
    };

    for m in control_re.find_iter(source) {
        let line = char_to_line(m.start());
        let (s, e) = extract_controls(m.as_str(), line, page_inherits_fqn.as_deref());
        symbols.extend(s);
        edges.extend(e);
    }

    for m in html_control_re.find_iter(source) {
        let line = char_to_line(m.start());
        let (s, e) = extract_controls(m.as_str(), line, page_inherits_fqn.as_deref());
        symbols.extend(s);
        edges.extend(e);
    }

    // ── 3. Register directives ──────────────────────────────────────────────────
    let register_table = extract_register_directives(project_root, rel_path, source, &char_to_line);
    for entry in &register_table {
        if let Some(ref src_path) = entry.src_rel_path {
            let line = 0u32; // Register directives are typically at the top
            let mut meta = HashMap::new();
            meta.insert("tag_prefix".into(), entry.tag_prefix.clone());
            meta.insert("tag_name".into(), entry.tag_name.clone());
            meta.insert("src_path".into(), src_path.clone());

            // Edge: page → user control file (registers_control)
            edges.push(ExtractedEdge {
                source_name: "file".into(),
                source_kind: "page".into(),
                source_start_line: line,
                source_language: "aspx".into(),
                target_name: src_path.clone(),
                target_kind: Some("file".into()),
                target_start_line: None,
                kind: "registers_control".into(),
                metadata: Some(meta),
            });
        }
    }

    // ── 4. User control tags resolved against Register table ────────────────────
    extract_user_control_tags(source, &register_table, rel_path, &char_to_line, &mut edges);

    // ── 5. DataSource controls ──────────────────────────────────────────────────
    extract_datasource_controls(
        source,
        rel_path,
        page_inherits_fqn.as_deref(),
        &char_to_line,
        &mut symbols,
        &mut edges,
    );

    // ── 6. Data-binding expressions: <%# Eval("...") %> and <%# Bind("...") %> ─
    extract_data_binding_expressions(
        source,
        rel_path,
        page_inherits_fqn.as_deref(),
        &char_to_line,
        &mut edges,
    );

    (symbols, edges)
}

// ── 3. Register Directive Extraction ────────────────────────────────────────

/// Parse `<%@ Register %>` directives and build a lookup table.
///
/// Two forms:
///   - `<%@ Register Src="~/Controls/Menu.ascx" TagPrefix="uc1" TagName="Menu" %>`
///   - `<%@ Register Assembly="AjaxControlToolkit" Namespace="..." TagPrefix="ajaxToolkit" %>`
fn extract_register_directives(
    project_root: &Path,
    rel_path: &RelPath,
    source: &str,
    char_to_line: &dyn Fn(usize) -> u32,
) -> Vec<RegisterEntry> {
    let Some(register_re) = get_compiled_regex(
        &REGISTER_RE,
        r"(?i)<%@\s*Register\b([^%]*)%>",
        "webforms_register",
    ) else {
        return Vec::new();
    };
    let Some(tagprefix_re) = get_compiled_regex(
        &TAGPREFIX_RE,
        r#"(?i)TagPrefix\s*=\s*"([^"]+)""#,
        "webforms_tagprefix",
    ) else {
        return Vec::new();
    };
    let Some(tagname_re) = get_compiled_regex(
        &TAGNAME_RE,
        r#"(?i)TagName\s*=\s*"([^"]+)""#,
        "webforms_tagname",
    ) else {
        return Vec::new();
    };
    let Some(src_re) = get_compiled_regex(
        &SRC_RE,
        r#"(?i)Src\s*=\s*"([^"]+)""#,
        "webforms_src",
    ) else {
        return Vec::new();
    };
    let Some(assembly_re) = get_compiled_regex(
        &ASSEMBLY_RE,
        r#"(?i)Assembly\s*=\s*"([^"]+)""#,
        "webforms_assembly",
    ) else {
        return Vec::new();
    };
    let Some(namespace_re) = get_compiled_regex(
        &NAMESPACE_RE,
        r#"(?i)Namespace\s*=\s*"([^"]+)""#,
        "webforms_namespace",
    ) else {
        return Vec::new();
    };

    let _ = char_to_line; // currently unused directly here; kept for future line-level tracking

    let mut entries = Vec::new();

    for m in register_re.find_iter(source) {
        let attrs = m.as_str();

        let tag_prefix = tagprefix_re
            .captures(attrs)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();
        let tag_name = tagname_re
            .captures(attrs)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();
        let src_raw = src_re.captures(attrs).map(|c| c[1].trim().to_string());
        let assembly = assembly_re
            .captures(attrs)
            .map(|c| c[1].trim().to_string());
        let namespace = namespace_re
            .captures(attrs)
            .map(|c| c[1].trim().to_string());

        if tag_prefix.is_empty() {
            continue;
        }

        let src_rel_path = src_raw.and_then(|raw| {
            resolve_aspnet_path(project_root, rel_path, &raw)
        });

        entries.push(RegisterEntry {
            tag_prefix,
            tag_name,
            src_rel_path,
            assembly,
            namespace,
        });
    }

    entries
}

/// Resolve an ASP.NET virtual path (~/...) or relative path to a project-relative path.
fn resolve_aspnet_path(project_root: &Path, markup_rel_path: &RelPath, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // Handle app-root paths: ~/Controls/Menu.ascx → Controls/Menu.ascx
    let effective = if let Some(stripped) = raw.strip_prefix("~/") {
        stripped.to_string()
    } else if raw.starts_with('/') {
        // Absolute virtual path: /Controls/Menu.ascx → Controls/Menu.ascx
        raw.trim_start_matches('/').to_string()
    } else {
        // Relative to current markup file's directory
        if let Some(parent_abs) = project_root.join(markup_rel_path.as_str()).parent() {
            let abs_path = parent_abs.join(raw);
            let normalized = lexically_normalize(&abs_path);
            if let Some(rel) = RelPath::from_relative(project_root, &normalized) {
                return Some(rel.as_str().to_string());
            }
        }
        raw.to_string()
    };

    Some(effective.replace('\\', "/"))
}

// ── 4. User Control Tag Resolution ──────────────────────────────────────────

/// Scan the markup for tags using registered prefixes (e.g. `<uc1:Menu ...>`)
/// and emit `registers_control` edges linking the parent page to the child .ascx.
fn extract_user_control_tags(
    source: &str,
    register_table: &[RegisterEntry],
    _rel_path: &RelPath,
    char_to_line: &dyn Fn(usize) -> u32,
    edges: &mut Vec<ExtractedEdge>,
) {
    if register_table.is_empty() {
        return;
    }

    // Build a lookup: (tag_prefix_lower, tag_name_lower) → src_rel_path
    let mut prefix_lookup: HashMap<(String, String), String> = HashMap::new();
    for entry in register_table {
        if let Some(ref src) = entry.src_rel_path {
            if !entry.tag_name.is_empty() {
                prefix_lookup.insert(
                    (entry.tag_prefix.to_lowercase(), entry.tag_name.to_lowercase()),
                    src.clone(),
                );
            }
        }
    }

    if prefix_lookup.is_empty() {
        return;
    }

    // Build a dynamic regex to match all registered tag prefixes.
    // E.g., for prefixes ["uc1", "ctrl"], build: (?i)<(uc1|ctrl):([A-Za-z]+)\b
    let prefixes: Vec<&str> = register_table
        .iter()
        .filter(|e| e.src_rel_path.is_some() && !e.tag_name.is_empty())
        .map(|e| e.tag_prefix.as_str())
        .collect();
    if prefixes.is_empty() {
        return;
    }

    let mut seen = std::collections::HashSet::new();
    let prefix_pattern = prefixes.join("|");
    let tag_pattern = format!(r"(?i)<({}):([A-Za-z]\w*)\b", regex::escape(&prefix_pattern));

    // Since the prefix pattern contains raw prefix strings (not regex meta), we can
    // use a simpler, less error-prone approach: iterate all registered entries and search
    // for each prefix:tagname pair directly.
    for entry in register_table {
        if let Some(ref src) = entry.src_rel_path {
            if entry.tag_name.is_empty() {
                continue;
            }
            // Build a specific regex for this prefix:tagname
            let pattern = format!(
                r"(?i)<{}:{}\b",
                regex::escape(&entry.tag_prefix),
                regex::escape(&entry.tag_name)
            );
            let Ok(tag_re) = Regex::new(&pattern) else {
                continue;
            };

            for m in tag_re.find_iter(source) {
                let line = char_to_line(m.start());
                let key = (entry.tag_prefix.to_lowercase(), entry.tag_name.to_lowercase());
                if !seen.insert(key) {
                    continue; // Only emit one edge per unique prefix:tagname pair
                }

                let mut meta = HashMap::new();
                meta.insert("tag_prefix".into(), entry.tag_prefix.clone());
                meta.insert("tag_name".into(), entry.tag_name.clone());
                meta.insert("src_path".into(), src.clone());

                edges.push(ExtractedEdge {
                    source_name: "file".into(),
                    source_kind: "page".into(),
                    source_start_line: line,
                    source_language: "aspx".into(),
                    target_name: src.clone(),
                    target_kind: Some("file".into()),
                    target_start_line: None,
                    kind: "registers_control".into(),
                    metadata: Some(meta),
                });
            }
        }
    }

    // Drop the unused variables
    drop(prefix_lookup);
    drop(tag_pattern);
}

// ── 5. DataSource Control Extraction ────────────────────────────────────────

/// Extract `<asp:SqlDataSource>`, `<asp:ObjectDataSource>`, `<asp:LinqDataSource>`,
/// `<asp:EntityDataSource>` controls from markup.
///
/// For SqlDataSource, emits:
///   - A "control" symbol for the data source
///   - `sql_calls` edges for SelectCommand, InsertCommand, UpdateCommand, DeleteCommand
///
/// For ObjectDataSource, emits:
///   - A "control" symbol for the data source
///   - `event_wiring` edges for SelectMethod, InsertMethod, UpdateMethod, DeleteMethod
///   - `codebehind_class` edge to the TypeName class
fn extract_datasource_controls(
    source: &str,
    _rel_path: &RelPath,
    page_inherits_fqn: Option<&str>,
    char_to_line: &dyn Fn(usize) -> u32,
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) {
    // Note: [^>] would break on <%$ ... %> ASP.NET expressions within attributes.
    // We use a pattern that handles both quoted attribute values and bare text:
    // (?:"[^"]*"|'[^']*'|[^>])* matches quoted strings (which may contain >) or non-> chars.
    let Some(datasource_re) = get_compiled_regex(
        &DATASOURCE_RE,
        r#"(?is)<asp:(SqlDataSource|ObjectDataSource|LinqDataSource|EntityDataSource)\b((?:"[^"]*"|'[^']*'|[^>])*)/?>"#,
        "webforms_datasource",
    ) else {
        return;
    };
    let Some(id_re) = get_compiled_regex(
        &ID_RE,
        r#"(?i)\bID\s*=\s*"([^"]+)""#,
        "webforms_id_ds",
    ) else {
        return;
    };

    // SQL command regexes for SqlDataSource
    let select_cmd_re = get_compiled_regex(
        &DS_SELECT_CMD_RE,
        r#"(?i)SelectCommand\s*=\s*"([^"]+)""#,
        "ds_select_cmd",
    );
    let insert_cmd_re = get_compiled_regex(
        &DS_INSERT_CMD_RE,
        r#"(?i)InsertCommand\s*=\s*"([^"]+)""#,
        "ds_insert_cmd",
    );
    let update_cmd_re = get_compiled_regex(
        &DS_UPDATE_CMD_RE,
        r#"(?i)UpdateCommand\s*=\s*"([^"]+)""#,
        "ds_update_cmd",
    );
    let delete_cmd_re = get_compiled_regex(
        &DS_DELETE_CMD_RE,
        r#"(?i)DeleteCommand\s*=\s*"([^"]+)""#,
        "ds_delete_cmd",
    );

    // Method regexes for ObjectDataSource
    let select_method_re = get_compiled_regex(
        &DS_SELECT_METHOD_RE,
        r#"(?i)SelectMethod\s*=\s*"([^"]+)""#,
        "ds_select_method",
    );
    let insert_method_re = get_compiled_regex(
        &DS_INSERT_METHOD_RE,
        r#"(?i)InsertMethod\s*=\s*"([^"]+)""#,
        "ds_insert_method",
    );
    let update_method_re = get_compiled_regex(
        &DS_UPDATE_METHOD_RE,
        r#"(?i)UpdateMethod\s*=\s*"([^"]+)""#,
        "ds_update_method",
    );
    let delete_method_re = get_compiled_regex(
        &DS_DELETE_METHOD_RE,
        r#"(?i)DeleteMethod\s*=\s*"([^"]+)""#,
        "ds_delete_method",
    );

    let type_name_re = get_compiled_regex(
        &DS_TYPE_NAME_RE,
        r#"(?i)TypeName\s*=\s*"([^"]+)""#,
        "ds_type_name",
    );

    for cap in datasource_re.captures_iter(source) {
        let ds_type = cap[1].to_string();
        let attrs = &cap[2];
        let line = char_to_line(cap.get(0).map_or(0, |m| m.start()));

        let ds_id = id_re
            .captures(attrs)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_else(|| format!("anon_{}", line));

        // Emit the data source as a control symbol
        let mut sym_meta = HashMap::new();
        sym_meta.insert("control_id".into(), ds_id.clone());
        sym_meta.insert("datasource_type".into(), ds_type.clone());

        symbols.push(ExtractedSymbol {
            name: ds_id.clone(),
            kind: "control".into(),
            start_line: line,
            end_line: line,
            metadata: Some(sym_meta),
        });

        match ds_type.as_str() {
            "SqlDataSource" => {
                // Extract SQL commands
                let cmd_regexes: &[(&str, Option<&Regex>)] = &[
                    ("SelectCommand", select_cmd_re),
                    ("InsertCommand", insert_cmd_re),
                    ("UpdateCommand", update_cmd_re),
                    ("DeleteCommand", delete_cmd_re),
                ];
                for (cmd_name, re_opt) in cmd_regexes {
                    if let Some(re) = re_opt {
                        if let Some(c) = re.captures(attrs) {
                            let sql = c[1].trim().to_string();
                            if sql.is_empty() {
                                continue;
                            }

                            let (sql_target, sql_kind) = classify_markup_sql(&sql);
                            let mut meta = HashMap::new();
                            meta.insert("command".into(), cmd_name.to_string());
                            meta.insert("sql_snippet".into(), truncate_sql(&sql));
                            meta.insert("sql_kind".into(), sql_kind.to_string());

                            edges.push(ExtractedEdge {
                                source_name: ds_id.clone(),
                                source_kind: "control".into(),
                                source_start_line: line,
                                source_language: "aspx".into(),
                                target_name: sql_target,
                                target_kind: Some(sql_kind.into()),
                                target_start_line: None,
                                kind: "sql_calls".into(),
                                metadata: Some(meta),
                            });
                        }
                    }
                }
            }
            "ObjectDataSource" | "LinqDataSource" | "EntityDataSource" => {
                // Extract TypeName (the backing class)
                if let Some(re) = type_name_re {
                    if let Some(c) = re.captures(attrs) {
                        let type_name = c[1].trim().to_string();
                        if !type_name.is_empty() {
                            let simple = type_name
                                .split('.')
                                .next_back()
                                .unwrap_or(&type_name)
                                .to_string();
                            let mut meta = HashMap::new();
                            meta.insert("fqn".into(), type_name.clone());
                            meta.insert("datasource_type".into(), ds_type.clone());

                            edges.push(ExtractedEdge {
                                source_name: ds_id.clone(),
                                source_kind: "control".into(),
                                source_start_line: line,
                                source_language: "aspx".into(),
                                target_name: simple,
                                target_kind: Some("class".into()),
                                target_start_line: None,
                                kind: "codebehind_class".into(),
                                metadata: Some(meta),
                            });
                        }
                    }
                }

                // Extract method bindings
                let method_regexes: &[(&str, Option<&Regex>)] = &[
                    ("SelectMethod", select_method_re),
                    ("InsertMethod", insert_method_re),
                    ("UpdateMethod", update_method_re),
                    ("DeleteMethod", delete_method_re),
                ];
                for (method_attr, re_opt) in method_regexes {
                    if let Some(re) = re_opt {
                        if let Some(c) = re.captures(attrs) {
                            let method = c[1].trim().to_string();
                            if method.is_empty() {
                                continue;
                            }

                            let mut meta = HashMap::new();
                            meta.insert("event".into(), method_attr.to_string());
                            if let Some(ifqn) = page_inherits_fqn {
                                meta.insert("fqn".into(), format!("{}.{}", ifqn, method));
                            }

                            edges.push(ExtractedEdge {
                                source_name: ds_id.clone(),
                                source_kind: "control".into(),
                                source_start_line: line,
                                source_language: "aspx".into(),
                                target_name: method,
                                target_kind: Some("function".into()),
                                target_start_line: None,
                                kind: "event_wiring".into(),
                                metadata: Some(meta),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Classify a SQL snippet from markup (SelectCommand, etc.) into a target name and kind.
fn classify_markup_sql(sql: &str) -> (String, &'static str) {
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();

    // Check for stored procedure patterns: EXEC sp_Name or just a single identifier
    if upper.starts_with("EXEC ") || upper.starts_with("EXECUTE ") {
        let after = if upper.starts_with("EXECUTE ") {
            &trimmed[8..]
        } else {
            &trimmed[5..]
        };
        let proc_name = after
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '[' || c == ']' || c == '"')
            .to_string();
        if !proc_name.is_empty() {
            return (format!("sql:stored_proc:{}", proc_name), "stored_proc");
        }
    }

    // Single identifier (no spaces except trailing whitespace) → likely a stored proc
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() == 1 {
        let proc_name = tokens[0]
            .trim_matches(|c| c == '[' || c == ']' || c == '"')
            .to_string();
        return (format!("sql:stored_proc:{}", proc_name), "stored_proc");
    }

    // Otherwise inline SQL — use a hash
    let hash = blake3::hash(trimmed.as_bytes());
    let short_hash = &hash.to_hex()[..16];
    (format!("sql:inline:{}", short_hash), "inline_sql")
}

/// Truncate a SQL string for metadata storage.
fn truncate_sql(sql: &str) -> String {
    const MAX_LEN: usize = 200;
    if sql.len() <= MAX_LEN {
        sql.to_string()
    } else {
        format!("{}...", &sql[..MAX_LEN])
    }
}

// ── 6. Data-Binding Expression Extraction ───────────────────────────────────

/// Extract `<%# Eval("FieldName") %>` and `<%# Bind("FieldName") %>` expressions.
///
/// Emits `data_binding` edges from the page to a `binding_field:FieldName` virtual node,
/// creating a graph edge from the UI markup directly to the model/schema field.
fn extract_data_binding_expressions(
    source: &str,
    _rel_path: &RelPath,
    page_inherits_fqn: Option<&str>,
    char_to_line: &dyn Fn(usize) -> u32,
    edges: &mut Vec<ExtractedEdge>,
) {
    let Some(binding_re) = get_compiled_regex(
        &DATA_BINDING_RE,
        r#"(?i)<%#\s*(?:Eval|Bind)\s*\(\s*"([^"]+)"\s*(?:,\s*"[^"]*")?\s*\)\s*%>"#,
        "webforms_data_binding",
    ) else {
        return;
    };

    let mut seen_fields = std::collections::HashSet::new();

    for cap in binding_re.captures_iter(source) {
        let field_name = cap[1].trim().to_string();
        if field_name.is_empty() {
            continue;
        }

        let line = char_to_line(cap.get(0).map_or(0, |m| m.start()));

        // Determine if it's Eval or Bind from the match text
        let full_match = cap.get(0).map_or("", |m| m.as_str()).to_uppercase();
        let binding_type = if full_match.contains("BIND") {
            "Bind"
        } else {
            "Eval"
        };

        // Only emit one edge per unique field name (deduplicate within a file)
        if !seen_fields.insert(field_name.clone()) {
            continue;
        }

        let target_name = format!("binding_field:{}", field_name);
        let mut meta = HashMap::new();
        meta.insert("field_name".into(), field_name.clone());
        meta.insert("binding_type".into(), binding_type.to_string());
        if let Some(ifqn) = page_inherits_fqn {
            meta.insert("page_fqn".into(), ifqn.to_string());
        }

        edges.push(ExtractedEdge {
            source_name: "file".into(),
            source_kind: "page".into(),
            source_start_line: line,
            source_language: "aspx".into(),
            target_name,
            target_kind: Some("binding_field".into()),
            target_start_line: None,
            kind: "data_binding".into(),
            metadata: Some(meta),
        });
    }
}

// ── Path Utilities ──────────────────────────────────────────────────────────

/// Simple lexical path normalization to handle ".." and "." components without hitting the disk.
fn lexically_normalize(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek() {
        let buf = std::path::PathBuf::from(c.as_os_str());
        components.next();
        buf
    } else {
        std::path::PathBuf::new()
    };

    for component in components {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                ret.pop();
            }
            Component::Normal(c) => {
                ret.push(c);
            }
            Component::RootDir => {
                ret.push(std::path::Component::RootDir);
            }
            Component::Prefix(..) => unreachable!(),
        }
    }
    ret
}

/// Returns true if the file extension suggests a WebForms markup file.
pub fn is_webforms_markup(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some("aspx") | Some("ascx") | Some("master")
    )
}

// ── P0.7 helpers ─────────────────────────────────────────────────────────────

/// Given an ASPX/ASCX path, return the expected code-behind paths to probe
/// (both `.cs` and `.vb` variants, in preference order).
///
/// This is useful when the markup directive lacks a `CodeBehind=` attribute but
/// the file system layout follows the default ASP.NET convention where the
/// code-behind lives next to the markup with `.cs` or `.vb` appended.
pub fn candidate_codebehind_paths(markup_path: &Path) -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();
    // Standard: Foo.aspx → Foo.aspx.cs, Foo.aspx.vb
    for ext in &["cs", "vb"] {
        let mut p = markup_path.to_path_buf();
        let new_name = format!(
            "{}.{}",
            markup_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(""),
            ext
        );
        p.set_file_name(new_name);
        candidates.push(p);
    }
    candidates
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ── P0.7: codebehind path normalization ───────────────────────────────────

    #[test]
    fn test_codebehind_cs_edges() {
        let markup = r#"<%@ Page Language="C#" AutoEventWireup="true"
            CodeBehind="Orders.aspx.cs"
            Inherits="MyApp.Web.OrdersPage" %>
        <asp:Button ID="btnSubmit" runat="server" OnClick="btnSubmit_Click" />
        "#;

        let project_root = Path::new("/");
        let rel_path = RelPath::new("App/Orders.aspx");
        let (syms, edges) = extract_webforms(project_root, &rel_path, markup);

        // Should have page symbol + control symbol
        assert!(syms.iter().any(|s| s.kind == "page"), "missing page symbol");
        assert!(
            syms.iter()
                .any(|s| s.name == "btnSubmit" && s.kind == "control"),
            "missing control symbol"
        );

        // Edge 1: codebehind_file (markup → file)
        let cb_edge = edges
            .iter()
            .find(|e| e.kind == "codebehind_file")
            .expect("codebehind_file edge");
        // Path should be normalized and resolved relative to markup directory.
        assert!(
            cb_edge.target_name.contains("Orders.aspx.cs"),
            "codebehind target should contain Orders.aspx.cs, got: {}",
            cb_edge.target_name
        );
        assert!(
            !cb_edge.target_name.contains('\\'),
            "path must use forward slashes, got: {}",
            cb_edge.target_name
        );
        assert_eq!(
            cb_edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("language"))
                .map(|s| s.as_str()),
            Some("csharp")
        );

        // Edge 2: codebehind_class (markup → class)
        let inh_edge = edges
            .iter()
            .find(|e| e.kind == "codebehind_class")
            .expect("codebehind_class edge");
        assert_eq!(inh_edge.target_name, "OrdersPage");
        assert_eq!(
            inh_edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("fqn"))
                .map(|s| s.as_str()),
            Some("MyApp.Web.OrdersPage")
        );

        // Edge 3: cb_defines (codebehind file → class)
        let cbd_edge = edges
            .iter()
            .find(|e| e.kind == "cb_defines")
            .expect("cb_defines edge");
        assert_eq!(cbd_edge.target_name, "OrdersPage");
        assert!(cbd_edge.source_name.contains("Orders.aspx.cs"));

        // Event wiring edge
        let ew_edge = edges
            .iter()
            .find(|e| e.kind == "event_wiring" && e.source_name == "btnSubmit")
            .expect("event_wiring edge");
        assert_eq!(ew_edge.target_name, "btnSubmit_Click");
    }

    #[test]
    fn test_codebehind_vb_edges() {
        let markup = r#"<%@ Page Language="VB" AutoEventWireup="false"
            CodeBehind="Reports\PrintReport.aspx.vb"
            Inherits="LegacyApp.Reports.PrintReportPage" %>
        "#;

        let project_root = Path::new("/");
        let rel_path = RelPath::new("Web/Reports/PrintReport.aspx");
        let (_, edges) = extract_webforms(project_root, &rel_path, markup);

        let cb_edge = edges
            .iter()
            .find(|e| e.kind == "codebehind_file")
            .expect("codebehind_file edge");

        // Backslash in directive must be normalized to forward slash.
        assert!(
            !cb_edge.target_name.contains('\\'),
            "backslash not normalized: {}",
            cb_edge.target_name
        );
        assert!(cb_edge.target_name.contains("PrintReport.aspx.vb"));
        assert_eq!(
            cb_edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("language"))
                .map(|s| s.as_str()),
            Some("vb")
        );

        // cb_defines should link the .vb file to the class
        let cbd_edge = edges
            .iter()
            .find(|e| e.kind == "cb_defines")
            .expect("cb_defines edge");
        assert_eq!(cbd_edge.target_name, "PrintReportPage");
    }

    #[test]
    fn test_candidate_codebehind_paths() {
        let paths = candidate_codebehind_paths(Path::new("/app/Views/Foo.aspx"));
        let names: Vec<_> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"Foo.aspx.cs"), "missing .cs candidate");
        assert!(names.contains(&"Foo.aspx.vb"), "missing .vb candidate");
    }

    #[test]
    fn test_webforms_extra_events() {
        let root = Path::new("C:/repo");
        let rel = RelPath::new("Grid.aspx");
        let source = r#"
<%@ Page Inherits="MyApp.Grid" %>
<asp:GridView ID="gvOrders" runat="server" OnRowCommand="gvOrders_RowCommand" OnSorting="gvOrders_Sorting" />
<asp:DropDownList ID="ddlStatus" runat="server" OnSelectedIndexChanged="ddlStatus_Changed" />
"#;
        let (_, edges) = extract_webforms(root, &rel, source);

        let row_cmd = edges
            .iter()
            .find(|e| {
                e.source_name == "gvOrders"
                    && e.metadata
                        .as_ref()
                        .and_then(|m| m.get("event"))
                        .map(|s| s.as_str())
                        == Some("OnRowCommand")
            })
            .unwrap();
        assert_eq!(row_cmd.target_name, "gvOrders_RowCommand");
        assert_eq!(
            row_cmd
                .metadata
                .as_ref()
                .unwrap()
                .get("fqn")
                .map(|s| s.as_str()),
            Some("MyApp.Grid.gvOrders_RowCommand")
        );

        let sort = edges
            .iter()
            .find(|e| {
                e.source_name == "gvOrders"
                    && e.metadata
                        .as_ref()
                        .and_then(|m| m.get("event"))
                        .map(|s| s.as_str())
                        == Some("OnSorting")
            })
            .unwrap();
        assert_eq!(sort.target_name, "gvOrders_Sorting");

        let changed = edges.iter().find(|e| e.source_name == "ddlStatus").unwrap();
        assert_eq!(changed.target_name, "ddlStatus_Changed");
    }

    #[test]
    fn test_no_codebehind_attribute_still_emits_inherits() {
        // Some legacy markup has Inherits= but no CodeBehind= attribute.
        let markup = r#"<%@ Page Inherits="OldApp.DefaultPage" %>"#;
        let project_root = Path::new("/");
        let rel_path = RelPath::new("Default.aspx");
        let (_, edges) = extract_webforms(project_root, &rel_path, markup);
        assert!(
            edges.iter().any(|e| e.kind == "codebehind_class"),
            "should emit codebehind_class edge even without CodeBehind="
        );
        assert!(
            !edges.iter().any(|e| e.kind == "codebehind"),
            "should NOT emit codebehind edge when attribute is absent"
        );
        assert!(
            !edges.iter().any(|e| e.kind == "cb_defines"),
            "should NOT emit cb_defines when CodeBehind= is absent"
        );
    }

    // ── P11: Register directive + user control tag tests ─────────────────────

    #[test]
    fn test_register_directive_src_based() {
        let markup = r#"
<%@ Page Inherits="MyApp.Default" %>
<%@ Register Src="~/Controls/Menu.ascx" TagPrefix="uc1" TagName="Menu" %>
<%@ Register Src="~/Controls/Footer.ascx" TagPrefix="uc2" TagName="Footer" %>
<uc1:Menu runat="server" ID="mainMenu" />
<uc2:Footer runat="server" ID="siteFooter" />
"#;
        let root = Path::new("C:/repo");
        let rel = RelPath::new("Default.aspx");
        let (_, edges) = extract_webforms(root, &rel, markup);

        // Should have registers_control edges for the Register directives
        let reg_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "registers_control")
            .collect();
        assert!(
            reg_edges.len() >= 2,
            "expected at least 2 registers_control edges, got {}",
            reg_edges.len()
        );

        // Verify Menu.ascx registration
        let menu_reg = reg_edges
            .iter()
            .find(|e| e.target_name.contains("Controls/Menu.ascx"))
            .expect("registers_control edge for Menu.ascx");
        assert_eq!(
            menu_reg
                .metadata
                .as_ref()
                .and_then(|m| m.get("tag_prefix"))
                .map(|s| s.as_str()),
            Some("uc1")
        );

        // Verify Footer.ascx registration
        assert!(
            reg_edges
                .iter()
                .any(|e| e.target_name.contains("Controls/Footer.ascx")),
            "missing registers_control edge for Footer.ascx"
        );
    }

    #[test]
    fn test_register_directive_tag_usage() {
        let markup = r#"
<%@ Page Inherits="MyApp.Products" %>
<%@ Register Src="~/UserControls/ProductCard.ascx" TagPrefix="pc" TagName="Card" %>
<div>
    <pc:Card runat="server" ID="card1" />
    <pc:Card runat="server" ID="card2" />
</div>
"#;
        let root = Path::new("C:/repo");
        let rel = RelPath::new("Products.aspx");
        let (_, edges) = extract_webforms(root, &rel, markup);

        let reg_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "registers_control")
            .collect();

        // Should find the register directive edge + one usage edge (deduplicated)
        assert!(
            reg_edges
                .iter()
                .any(|e| e.target_name.contains("UserControls/ProductCard.ascx")),
            "missing registers_control edge for ProductCard.ascx"
        );
    }

    // ── P11: DataSource control tests ────────────────────────────────────────

    #[test]
    fn test_sql_datasource_extraction() {
        let markup = r#"
<%@ Page Inherits="MyApp.Orders" %>
<asp:SqlDataSource ID="dsOrders" runat="server"
    ConnectionString="<%$ ConnectionStrings:MainDB %>"
    SelectCommand="SELECT * FROM Orders WHERE Status=@Status"
    DeleteCommand="EXEC sp_DeleteOrder @OrderId" />
"#;
        let root = Path::new("C:/repo");
        let rel = RelPath::new("Orders.aspx");
        let (syms, edges) = extract_webforms(root, &rel, markup);

        // Should find the datasource control symbol
        assert!(
            syms.iter().any(|s| s.name == "dsOrders" && s.kind == "control"),
            "missing dsOrders control symbol"
        );

        // Should find sql_calls edges
        let sql_edges: Vec<_> = edges.iter().filter(|e| e.kind == "sql_calls").collect();
        assert!(
            sql_edges.len() >= 2,
            "expected at least 2 sql_calls edges, got {}",
            sql_edges.len()
        );

        // The SELECT should produce an inline_sql edge
        let select_edge = sql_edges
            .iter()
            .find(|e| {
                e.metadata
                    .as_ref()
                    .and_then(|m| m.get("command"))
                    .map(|s| s.as_str())
                    == Some("SelectCommand")
            })
            .expect("sql_calls edge for SelectCommand");
        assert!(
            select_edge.target_name.starts_with("sql:inline:"),
            "SELECT should be inline_sql, got: {}",
            select_edge.target_name
        );

        // The DELETE should produce a stored_proc edge
        let delete_edge = sql_edges
            .iter()
            .find(|e| {
                e.metadata
                    .as_ref()
                    .and_then(|m| m.get("command"))
                    .map(|s| s.as_str())
                    == Some("DeleteCommand")
            })
            .expect("sql_calls edge for DeleteCommand");
        assert!(
            delete_edge.target_name.contains("sp_DeleteOrder"),
            "EXEC should resolve to stored proc, got: {}",
            delete_edge.target_name
        );
    }

    #[test]
    fn test_object_datasource_extraction() {
        let markup = r#"
<%@ Page Inherits="MyApp.Products" %>
<asp:ObjectDataSource ID="dsProducts" runat="server"
    TypeName="MyApp.Data.ProductRepository"
    SelectMethod="GetAllProducts"
    InsertMethod="InsertProduct"
    DeleteMethod="DeleteProduct" />
"#;
        let root = Path::new("C:/repo");
        let rel = RelPath::new("Products.aspx");
        let (syms, edges) = extract_webforms(root, &rel, markup);

        // Control symbol
        assert!(
            syms.iter()
                .any(|s| s.name == "dsProducts" && s.kind == "control"),
            "missing dsProducts control symbol"
        );

        // codebehind_class edge to TypeName
        let type_edge = edges
            .iter()
            .find(|e| e.kind == "codebehind_class" && e.source_name == "dsProducts")
            .expect("codebehind_class edge for ObjectDataSource TypeName");
        assert_eq!(type_edge.target_name, "ProductRepository");
        assert_eq!(
            type_edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("fqn"))
                .map(|s| s.as_str()),
            Some("MyApp.Data.ProductRepository")
        );

        // event_wiring edges for methods
        let method_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "event_wiring" && e.source_name == "dsProducts")
            .collect();
        assert_eq!(
            method_edges.len(),
            3,
            "expected 3 method wiring edges, got {}",
            method_edges.len()
        );
        assert!(
            method_edges
                .iter()
                .any(|e| e.target_name == "GetAllProducts"),
            "missing SelectMethod edge"
        );
        assert!(
            method_edges.iter().any(|e| e.target_name == "InsertProduct"),
            "missing InsertMethod edge"
        );
    }

    // ── P11: Data-binding expression tests ──────────────────────────────────

    #[test]
    fn test_data_binding_eval() {
        let markup = r#"
<%@ Page Inherits="MyApp.Products" %>
<asp:Label Text='<%# Eval("ProductName") %>' runat="server" />
<asp:Label Text='<%# Eval("UnitPrice", "{0:C}") %>' runat="server" />
<asp:TextBox Text='<%# Bind("Quantity") %>' runat="server" />
"#;
        let root = Path::new("C:/repo");
        let rel = RelPath::new("Products.aspx");
        let (_, edges) = extract_webforms(root, &rel, markup);

        let binding_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "data_binding")
            .collect();
        assert_eq!(
            binding_edges.len(),
            3,
            "expected 3 data_binding edges, got {}",
            binding_edges.len()
        );

        // Check ProductName binding
        let product_edge = binding_edges
            .iter()
            .find(|e| e.target_name == "binding_field:ProductName")
            .expect("data_binding edge for ProductName");
        assert_eq!(
            product_edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("binding_type"))
                .map(|s| s.as_str()),
            Some("Eval")
        );

        // Check Quantity Bind
        let qty_edge = binding_edges
            .iter()
            .find(|e| e.target_name == "binding_field:Quantity")
            .expect("data_binding edge for Quantity");
        assert_eq!(
            qty_edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("binding_type"))
                .map(|s| s.as_str()),
            Some("Bind")
        );

        // Check page_fqn in metadata
        assert_eq!(
            product_edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("page_fqn"))
                .map(|s| s.as_str()),
            Some("MyApp.Products")
        );
    }

    #[test]
    fn test_data_binding_deduplication() {
        let markup = r#"
<%@ Page Inherits="MyApp.Grid" %>
<%# Eval("Status") %>
<%# Eval("Status") %>
<%# Bind("Status") %>
"#;
        let root = Path::new("C:/repo");
        let rel = RelPath::new("Grid.aspx");
        let (_, edges) = extract_webforms(root, &rel, markup);

        let binding_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "data_binding")
            .collect();
        // "Status" should appear only once (deduplicated)
        assert_eq!(
            binding_edges.len(),
            1,
            "expected 1 deduplicated data_binding edge, got {}",
            binding_edges.len()
        );
    }
}
