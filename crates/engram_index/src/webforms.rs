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
/// Emits `ExtractedSymbol` (controls + page) and `ExtractedEdge` (all of the above).
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use regex::Regex;
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
            let mut meta = std::collections::HashMap::new();
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

            let mut meta = std::collections::HashMap::new();
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
            let mut symbol_meta = std::collections::HashMap::new();
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
                let mut meta = std::collections::HashMap::new();
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

    (symbols, edges)
}

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
}
