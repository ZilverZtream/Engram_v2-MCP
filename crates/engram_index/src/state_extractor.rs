/// Global State access detector for ASP.NET WebForms apps.
///
/// Detects access to shared state stores in C# and VB.NET code:
///   - `Session["Key"]` / `Session("Key")`
///   - `ViewState["Key"]` / `ViewState("Key")`
///   - `Application["Key"]` / `Application("Key")`
///   - `Cache["Key"]` / `Cache("Key")`
///   - `HttpContext.Current.Items["Key"]`
///   - `HttpContext.Current.Session["Key"]`
///
/// Emits:
///   - `ExtractedSymbol` of kind `global_state` for each unique state key
///   - `ExtractedEdge` of kind `reads_state` or `writes_state`
///
/// Write vs Read determination:
///   If the state access is on the LHS of an assignment (i.e. followed by `= <non-equals>`
///   on the same line), it is a **write**; otherwise a **read**.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

/// C# state access: Session["Key"], ViewState["Key"], Application["Key"],
/// Cache["Key"], HttpContext.Current.Items["Key"], HttpContext.Current.Session["Key"]
static CS_STATE_RE: OnceLock<Regex> = OnceLock::new();

/// VB.NET state access: Session("Key"), ViewState("Key"), etc.
static VB_STATE_RE: OnceLock<Regex> = OnceLock::new();

/// Simple function/method extractor: recognizes C# method declarations.
/// Used to determine the "enclosing function" for a state access.
static CS_METHOD_RE: OnceLock<Regex> = OnceLock::new();

/// VB method declarations (Sub/Function).
static VB_METHOD_RE: OnceLock<Regex> = OnceLock::new();

/// Extract state accesses from a C# or VB.NET source file.
///
/// `rel_path` is the project-relative path to the file.
/// `language` should be `"csharp"` or `"vbnet"`.
///
/// Returns `(symbols, edges)` where symbols are unique global_state nodes
/// and edges are reads_state/writes_state relationships.
pub fn extract_state_accesses(
    rel_path: &RelPath,
    source: &str,
    language: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut edges: Vec<ExtractedEdge> = Vec::new();
    let mut seen_keys: HashSet<(String, String)> = HashSet::new(); // (state_type, key)
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();

    // Build line offsets for line number computation.
    let lines: Vec<&str> = source.lines().collect();

    // Build a map of line_index → enclosing function name.
    let method_map = build_method_map(&lines, language);

    let state_re = match language {
        "csharp" => cs_state_regex(),
        "vbnet" => vb_state_regex(),
        _ => return (symbols, edges),
    };

    for (line_idx, line) in lines.iter().enumerate() {
        // Skip comments.
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("'") || trimmed.starts_with("<!--") {
            continue;
        }

        for cap in state_re.captures_iter(line) {
            let full_match = cap.get(0).map_or("", |m| m.as_str());
            let state_type_raw = cap.get(1).map_or("", |m| m.as_str());
            let key = cap.get(2).map_or("", |m| m.as_str());

            if key.is_empty() {
                continue;
            }

            // Normalize state_type: HttpContext.Current.Session → Session, etc.
            let state_type = normalize_state_type(state_type_raw);

            // Determine read vs write: is this on the LHS of an assignment?
            let is_write =
                if let Some(match_end) = line.find(full_match).map(|s| s + full_match.len()) {
                    let rest = &line[match_end..];
                    // Check if what follows is ` = <non-equals>` (assignment, not comparison)
                    let rest_trimmed = rest.trim_start();
                    rest_trimmed.starts_with('=') && !rest_trimmed.starts_with("==")
                } else {
                    false
                };

            let edge_kind = if is_write {
                "writes_state"
            } else {
                "reads_state"
            };

            // Find the enclosing function.
            let enclosing = method_map
                .get(&line_idx)
                .cloned()
                .unwrap_or_else(|| rel_path.as_str().to_string());

            let target_id = format!("state:{}:{}", state_type, key);

            edges.push(ExtractedEdge {
                source_name: enclosing,
                source_kind: "function".to_string(),
                source_start_line: line_idx as u32,
                source_language: language.to_string(),
                target_name: target_id,
                target_kind: Some("global_state".to_string()),
                target_start_line: None,
                kind: edge_kind.to_string(),
                metadata: Some({
                    let mut m = HashMap::new();
                    m.insert("state_type".to_string(), state_type.clone());
                    m.insert("state_key".to_string(), key.to_string());
                    m
                }),
            });

            // Emit unique state symbol.
            if seen_keys.insert((state_type.clone(), key.to_string())) {
                let mut meta = HashMap::new();
                meta.insert("state_type".to_string(), state_type.clone());
                meta.insert("state_key".to_string(), key.to_string());

                symbols.push(ExtractedSymbol {
                    name: format!("{}:{}", state_type, key),
                    kind: "global_state".to_string(),
                    start_line: line_idx as u32,
                    end_line: line_idx as u32,
                    metadata: Some(meta),
                });
            }
        }
    }

    (symbols, edges)
}

fn cs_state_regex() -> &'static Regex {
    CS_STATE_RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(Session|ViewState|Application|Cache|HttpContext\.Current\.Items|HttpContext\.Current\.Session)\s*\[\s*"([^"]+)"\s*\]"#,
        )
        .expect("Invalid regex")
    })
}

fn vb_state_regex() -> &'static Regex {
    VB_STATE_RE.get_or_init(|| {
        Regex::new(r#"(?i)(Session|ViewState|Application|Cache)\s*\(\s*"([^"]+)"\s*\)"#)
            .expect("Invalid regex")
    })
}

/// Normalize state type: e.g., `HttpContext.Current.Session` → `Session`.
fn normalize_state_type(raw: &str) -> String {
    if raw.contains('.') {
        // Extract the last segment: HttpContext.Current.Items → Items, etc.
        raw.rsplit('.').next().unwrap_or(raw).to_string()
    } else {
        raw.to_string()
    }
}

/// Build a mapping from line_index → enclosing method/function name.
///
/// This is a simple heuristic: scan for method declarations and assign subsequent
/// lines to the most recent method until the next one is found.
fn build_method_map(lines: &[&str], language: &str) -> HashMap<usize, String> {
    let method_re: &Regex = match language {
        "csharp" => CS_METHOD_RE.get_or_init(|| {
            // Matches: access_modifier? [static|async|override|virtual]* return_type MethodName(
            Regex::new(
                r"(?:public|private|protected|internal)?\s*(?:static\s+|async\s+|override\s+|virtual\s+)*\w+(?:<[^>]+>)?\s+(\w+)\s*\(",
            )
            .expect("Invalid regex")
        }),
        "vbnet" => VB_METHOD_RE.get_or_init(|| {
            Regex::new(r"(?i)(?:Public|Private|Protected|Friend)?\s*(?:Shared\s+|Overrides\s+)?(?:Sub|Function)\s+(\w+)\s*\(")
                .expect("Invalid regex")
            }),
        _ => return HashMap::new(),
    };

    let mut map = HashMap::new();
    let mut current_method: Option<String> = None;

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = method_re.captures(line) {
            let method_name = cap.get(1).map_or("", |m| m.as_str());
            // Skip common false positives (new, if, for, etc.).
            if ![
                "new", "if", "for", "while", "foreach", "switch", "using", "catch", "return",
                "var", "int", "string", "bool", "void",
            ]
            .contains(&method_name)
            {
                current_method = Some(method_name.to_string());
            }
        }
        if let Some(ref m) = current_method {
            map.insert(i, m.clone());
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_csharp_session_read() {
        let code = r#"
public class OrderPage : Page {
    protected void Page_Load(object sender, EventArgs e) {
        var userId = Session["UserId"];
        var cart = Session["ShoppingCart"];
    }
}
"#;
        let rel = RelPath::new("OrderPage.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 2, "Should find 2 unique state keys");
        assert_eq!(edges.len(), 2, "Should find 2 state accesses");

        for e in &edges {
            assert_eq!(e.kind, "reads_state");
        }
    }

    #[test]
    fn test_csharp_session_write() {
        let code = r#"
protected void Login(object sender, EventArgs e) {
    Session["UserId"] = user.Id;
    Session["UserName"] = user.Name;
    var role = Session["Role"];
}
"#;
        let rel = RelPath::new("Login.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();

        assert_eq!(writes.len(), 2, "Should find 2 writes");
        assert_eq!(reads.len(), 1, "Should find 1 read");
        assert_eq!(syms.len(), 3, "Should find 3 unique state keys");
    }

    #[test]
    fn test_csharp_viewstate() {
        let code = r#"
ViewState["SortColumn"] = "Name";
var col = ViewState["SortColumn"];
"#;
        let rel = RelPath::new("Grid.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1, "Should find 1 unique state key");
        assert_eq!(edges.len(), 2, "Should find 2 accesses");

        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].target_name.contains("ViewState"));
    }

    #[test]
    fn test_vbnet_session() {
        let code = r#"
Public Sub Page_Load(sender As Object, e As EventArgs)
    Session("UserId") = currentUser.Id
    Dim cart = Session("Cart")
End Sub
"#;
        let rel = RelPath::new("Default.aspx.vb");
        let (syms, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 2);
        assert_eq!(edges.len(), 2);

        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(reads.len(), 1);
    }

    #[test]
    fn test_httpcontext_normalization() {
        let code = r#"
HttpContext.Current.Session["Token"] = auth.Token;
HttpContext.Current.Items["RequestId"] = Guid.NewGuid();
"#;
        let rel = RelPath::new("Global.asax.cs");
        let (syms, _edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 2);

        let session_sym = syms
            .iter()
            .find(|s| s.name == "Session:Token")
            .expect("Session:Token");
        let items_sym = syms
            .iter()
            .find(|s| s.name == "Items:RequestId")
            .expect("Items:RequestId");

        let meta = session_sym.metadata.as_ref().expect("meta");
        assert_eq!(meta.get("state_type").expect("state_type"), "Session");

        let meta2 = items_sym.metadata.as_ref().expect("meta");
        assert_eq!(meta2.get("state_type").expect("state_type"), "Items");
    }
}
