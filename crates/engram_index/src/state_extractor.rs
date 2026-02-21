/// Global State access detector for ASP.NET WebForms apps.
///
/// Detects access to shared state stores in C# and VB.NET code:
///   - `Session["Key"]` / `Session("Key")`
///   - `ViewState["Key"]` / `ViewState("Key")`
///   - `Application["Key"]` / `Application("Key")`
///   - `Cache["Key"]` / `Cache("Key")`
///   - `HttpContext.Current.Items["Key"]`
///   - `HttpContext.Current.Session["Key"]`
///   - `Request.Cookies["Key"]` / `Request.Cookies("Key")`
///   - `Response.Cookies["Key"]` / `Response.Cookies("Key")`
///
/// Supports both string literal keys and identifier/constant references:
///   - `Session["UserId"]` — literal key "UserId"
///   - `Session[UserKey]`  — resolved via local constant table if
///     `Const UserKey As String = "USER_ID"` is declared in the same file.
///   - If the identifier cannot be resolved locally, emits an
///     `unresolved_state_read` / `unresolved_state_write` edge to the variable
///     name for downstream graph-level resolution.
///
/// Emits:
///   - `ExtractedSymbol` of kind `global_state` for each unique state key
///   - `ExtractedEdge` of kind `reads_state` or `writes_state`
///
/// Write vs Read determination:
///   If the state access is on the LHS of an assignment (i.e. followed by `= <non-equals>`
///   on the same line), it is a **write**; otherwise a **read**.
///   Exception: `Request.Cookies` is always a **read**; `Response.Cookies` is always a **write**.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

/// C# state access: Session["Key"] or Session[Identifier]
static CS_STATE_RE: OnceLock<Regex> = OnceLock::new();

/// VB.NET state access: Session("Key") or Session(Identifier)
static VB_STATE_RE: OnceLock<Regex> = OnceLock::new();

/// Simple function/method extractor: recognizes C# method declarations.
/// Used to determine the "enclosing function" for a state access.
static CS_METHOD_RE: OnceLock<Regex> = OnceLock::new();

/// VB method declarations (Sub/Function).
static VB_METHOD_RE: OnceLock<Regex> = OnceLock::new();

/// C# constant declaration: `const string Key = "Value";`
/// Also matches `static readonly string Key = "Value";`
static CS_CONST_RE: OnceLock<Regex> = OnceLock::new();

/// VB.NET constant declaration: `Const Key As String = "Value"`
static VB_CONST_RE: OnceLock<Regex> = OnceLock::new();

/// C# cookie access: Request.Cookies["Key"] or Response.Cookies["Key"]
static CS_COOKIE_RE: OnceLock<Regex> = OnceLock::new();

/// VB.NET cookie access: Request.Cookies("Key") or Response.Cookies("Key")
static VB_COOKIE_RE: OnceLock<Regex> = OnceLock::new();

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
    language: &'static str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut edges: Vec<ExtractedEdge> = Vec::new();
    let mut seen_keys: HashSet<(String, String)> = HashSet::new(); // (state_type, key)
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();

    // Build line offsets for line number computation.
    let lines: Vec<&str> = source.lines().collect();

    // Build a map of line_index → enclosing function name.
    let method_map = build_method_map(&lines, language);

    // Build a local constant symbol table for identifier resolution.
    let const_table = build_const_table(source, language);

    let state_re = match language {
        "csharp" => {
            let Some(re) = cs_state_regex() else {
                return (symbols, edges);
            };
            re
        }
        "vbnet" => {
            let Some(re) = vb_state_regex() else {
                return (symbols, edges);
            };
            re
        }
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
            let literal_key = cap.get(2).map_or("", |m| m.as_str());
            let ident_key = cap.get(3).map_or("", |m| m.as_str());

            // Determine the key: either a direct string literal or an identifier
            // that needs resolution via the constant table.
            let (key, is_unresolved) = if !literal_key.is_empty() {
                (literal_key.to_string(), false)
            } else if !ident_key.is_empty() {
                // Try to resolve the identifier from the local constant table.
                // For VB, use case-insensitive lookup.
                let lookup_name = if language == "vbnet" {
                    ident_key.to_lowercase()
                } else {
                    ident_key.to_string()
                };
                if let Some(resolved) = const_table.get(&lookup_name) {
                    (resolved.clone(), false)
                } else {
                    // Unresolvable locally — emit with the variable name for
                    // downstream graph-level resolution.
                    (ident_key.to_string(), true)
                }
            } else {
                continue;
            };

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

            // For unresolved identifiers, emit a distinct edge kind so downstream
            // graph passes can join them to globally-defined constants.
            let edge_kind = if is_unresolved {
                if is_write {
                    "unresolved_state_write"
                } else {
                    "unresolved_state_read"
                }
            } else if is_write {
                "writes_state"
            } else {
                "reads_state"
            };

            // Find the enclosing function.
            let enclosing = method_map
                .get(&line_idx)
                .cloned()
                .unwrap_or_else(|| rel_path.as_str().to_string());

            let target_id = if is_unresolved {
                // For unresolved identifiers, target the variable name directly
                // so a graph-level pass can link it when the constant is found elsewhere.
                format!("unresolved_state:{}:{}", state_type, key)
            } else {
                format!("state:{}:{}", state_type, key)
            };

            let mut meta = HashMap::new();
            meta.insert("state_type".to_string(), state_type.clone());
            meta.insert("state_key".to_string(), key.clone());
            if is_unresolved {
                meta.insert("unresolved".to_string(), "true".to_string());
                meta.insert("identifier".to_string(), ident_key.to_string());
            }

            edges.push(ExtractedEdge {
                source_name: enclosing,
                source_kind: "function",
                source_start_line: line_idx as u32,
                source_language: language,
                target_name: target_id,
                target_kind: Some("global_state"),
                target_start_line: None,
                kind: edge_kind,
                metadata: Some(meta),
            });

            // Emit unique state symbol (only for resolved keys).
            if !is_unresolved && seen_keys.insert((state_type.clone(), key.clone())) {
                let mut meta = HashMap::new();
                meta.insert("state_type".to_string(), state_type.clone());
                meta.insert("state_key".to_string(), key.clone());

                symbols.push(ExtractedSymbol {
                    name: format!("{}:{}", state_type, key),
                    kind: "global_state",
                    start_line: line_idx as u32,
                    end_line: line_idx as u32,
                    metadata: Some(meta),
                });
            }
        }
    }

    // ── Second pass: Cookie access detection ──────────────────────────────
    let cookie_re = match language {
        "csharp" => cs_cookie_regex(),
        "vbnet" => vb_cookie_regex(),
        _ => None,
    };

    if let Some(cookie_re) = cookie_re {
        for (line_idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("'") || trimmed.starts_with("<!--")
            {
                continue;
            }

            for cap in cookie_re.captures_iter(line) {
                let direction_raw = cap.get(1).map_or("", |m| m.as_str());
                let literal_key = cap.get(2).map_or("", |m| m.as_str());
                let ident_key = cap.get(3).map_or("", |m| m.as_str());

                let (key, is_unresolved) = if !literal_key.is_empty() {
                    (literal_key.to_string(), false)
                } else if !ident_key.is_empty() {
                    let lookup_name = if language == "vbnet" {
                        ident_key.to_lowercase()
                    } else {
                        ident_key.to_string()
                    };
                    if let Some(resolved) = const_table.get(&lookup_name) {
                        (resolved.clone(), false)
                    } else {
                        (ident_key.to_string(), true)
                    }
                } else {
                    continue;
                };

                if key.is_empty() {
                    continue;
                }

                // Request.Cookies → always a read, Response.Cookies → always a write.
                let is_request = direction_raw.eq_ignore_ascii_case("Request");
                let is_write = !is_request;

                let state_type = "Cookies".to_string();

                let edge_kind = if is_unresolved {
                    if is_write {
                        "unresolved_state_write"
                    } else {
                        "unresolved_state_read"
                    }
                } else if is_write {
                    "writes_state"
                } else {
                    "reads_state"
                };

                let enclosing = method_map
                    .get(&line_idx)
                    .cloned()
                    .unwrap_or_else(|| rel_path.as_str().to_string());

                let target_id = if is_unresolved {
                    format!("unresolved_state:{}:{}", state_type, key)
                } else {
                    format!("state:{}:{}", state_type, key)
                };

                let mut meta = HashMap::new();
                meta.insert("state_type".to_string(), state_type.clone());
                meta.insert("state_key".to_string(), key.clone());
                meta.insert(
                    "cookie_direction".to_string(),
                    if is_request {
                        "Request".to_string()
                    } else {
                        "Response".to_string()
                    },
                );
                if is_unresolved {
                    meta.insert("unresolved".to_string(), "true".to_string());
                    meta.insert("identifier".to_string(), ident_key.to_string());
                }

                edges.push(ExtractedEdge {
                    source_name: enclosing,
                    source_kind: "function",
                    source_start_line: line_idx as u32,
                    source_language: language,
                    target_name: target_id,
                    target_kind: Some("global_state"),
                    target_start_line: None,
                    kind: edge_kind,
                    metadata: Some(meta),
                });

                if !is_unresolved && seen_keys.insert((state_type.clone(), key.clone())) {
                    let mut meta = HashMap::new();
                    meta.insert("state_type".to_string(), state_type.clone());
                    meta.insert("state_key".to_string(), key.clone());

                    symbols.push(ExtractedSymbol {
                        name: format!("{}:{}", state_type, key),
                        kind: "global_state",
                        start_line: line_idx as u32,
                        end_line: line_idx as u32,
                        metadata: Some(meta),
                    });
                }
            }
        }
    }

    (symbols, edges)
}

fn cs_state_regex() -> Option<&'static Regex> {
    // Group 1: state store name
    // Group 2: string literal key (if quoted)
    // Group 3: bare identifier key (if unquoted)
    get_compiled_regex(
        &CS_STATE_RE,
        r#"(?i)(Session|ViewState|Application|Cache|HttpContext\.Current\.Items|HttpContext\.Current\.Session)\s*\[\s*(?:"([^"]+)"|([A-Za-z_]\w*))\s*\]"#,
        "state_cs",
    )
}

fn vb_state_regex() -> Option<&'static Regex> {
    // Group 1: state store name
    // Group 2: string literal key (if quoted)
    // Group 3: bare identifier key (if unquoted)
    get_compiled_regex(
        &VB_STATE_RE,
        r#"(?i)(Session|ViewState|Application|Cache)\s*\(\s*(?:"([^"]+)"|([A-Za-z_]\w*))\s*\)"#,
        "state_vb",
    )
}

fn cs_cookie_regex() -> Option<&'static Regex> {
    // Group 1: Request or Response
    // Group 2: string literal key (if quoted)
    // Group 3: bare identifier key (if unquoted)
    get_compiled_regex(
        &CS_COOKIE_RE,
        r#"(?i)(Request|Response)\.Cookies\s*\[\s*(?:"([^"]+)"|([A-Za-z_]\w*))\s*\]"#,
        "cookie_cs",
    )
}

fn vb_cookie_regex() -> Option<&'static Regex> {
    // Group 1: Request or Response
    // Group 2: string literal key (if quoted)
    // Group 3: bare identifier key (if unquoted)
    get_compiled_regex(
        &VB_COOKIE_RE,
        r#"(?i)(Request|Response)\.Cookies\s*\(\s*(?:"([^"]+)"|([A-Za-z_]\w*))\s*\)"#,
        "cookie_vb",
    )
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

/// Build a local constant symbol table by scanning the file for `const` declarations.
///
/// Returns a map of `variable_name → string_literal_value`.
/// For VB.NET, keys are stored in lowercase for case-insensitive lookup.
///
/// Supports:
///   - C#: `const string Key = "Value";` and `static readonly string Key = "Value";`
///   - VB:  `Const Key As String = "Value"`
fn build_const_table(source: &str, language: &str) -> HashMap<String, String> {
    let mut table = HashMap::new();

    let re = match language {
        "csharp" => get_compiled_regex(
            &CS_CONST_RE,
            r#"(?:const\s+string|static\s+readonly\s+string)\s+([A-Za-z_]\w*)\s*=\s*"([^"]+)""#,
            "state_cs_const",
        ),
        "vbnet" => get_compiled_regex(
            &VB_CONST_RE,
            r#"(?i)Const\s+([A-Za-z_]\w*)\s+(?:As\s+\w+\s+)?=\s*"([^"]+)""#,
            "state_vb_const",
        ),
        _ => return table,
    };

    let Some(re) = re else {
        return table;
    };

    for cap in re.captures_iter(source) {
        let name = cap.get(1).map_or("", |m| m.as_str());
        let value = cap.get(2).map_or("", |m| m.as_str());
        if name.is_empty() || value.is_empty() {
            continue;
        }
        // For VB.NET, store with lowercase key for case-insensitive lookup.
        let key = if language == "vbnet" {
            name.to_lowercase()
        } else {
            name.to_string()
        };
        table.insert(key, value.to_string());
    }

    table
}

/// Build a mapping from line_index → enclosing method/function name.
///
/// This is a simple heuristic: scan for method declarations and assign subsequent
/// lines to the most recent method until the next one is found.
fn build_method_map(lines: &[&str], language: &str) -> HashMap<usize, String> {
    let method_re: &Regex = match language {
        "csharp" => {
            let Some(re) = get_compiled_regex(
                &CS_METHOD_RE,
                r"(?:public|private|protected|internal)?\s*(?:static\s+|async\s+|override\s+|virtual\s+)*\w+(?:<[^>]+>)?\s+(\w+)\s*\(",
                "state_cs_method",
            ) else {
                return HashMap::new();
            };
            re
        }
        "vbnet" => {
            let Some(re) = get_compiled_regex(
                &VB_METHOD_RE,
                r"(?i)(?:Public|Private|Protected|Friend)?\s*(?:Shared\s+|Overrides\s+)?(?:Sub|Function)\s+(\w+)\s*\(",
                "state_vb_method",
            ) else {
                return HashMap::new();
            };
            re
        }
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

    // ── P12: Constant/Identifier resolution tests ─────────────────────────

    #[test]
    fn test_csharp_const_resolution() {
        let code = r#"
public class SessionHelper {
    const string UserKey = "USER_ID";
    const string RoleKey = "ROLE";

    protected void Page_Load(object sender, EventArgs e) {
        Session[UserKey] = currentUser.Id;
        var role = Session[RoleKey];
    }
}
"#;
        let rel = RelPath::new("SessionHelper.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        // Both constants should be resolved
        assert_eq!(syms.len(), 2, "Should find 2 unique resolved state keys");

        let user_sym = syms.iter().find(|s| s.name == "Session:USER_ID");
        assert!(user_sym.is_some(), "Should resolve UserKey to USER_ID");

        let role_sym = syms.iter().find(|s| s.name == "Session:ROLE");
        assert!(role_sym.is_some(), "Should resolve RoleKey to ROLE");

        // Check edge kinds
        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(writes.len(), 1, "Session[UserKey] = ... is a write");
        assert_eq!(reads.len(), 1, "var role = Session[RoleKey] is a read");
    }

    #[test]
    fn test_csharp_static_readonly_resolution() {
        let code = r#"
static readonly string CartKey = "SHOPPING_CART";
Session[CartKey] = new Cart();
"#;
        let rel = RelPath::new("Cart.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Session:SHOPPING_CART");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "writes_state");
    }

    #[test]
    fn test_vbnet_const_resolution() {
        let code = r#"
Public Class BasePage
    Const UserKey As String = "USER_ID"
    Const CartKey As String = "CART"

    Public Sub Page_Load(sender As Object, e As EventArgs)
        Session(UserKey) = currentUser.Id
        Dim cart = Session(CartKey)
    End Sub
End Class
"#;
        let rel = RelPath::new("BasePage.aspx.vb");
        let (syms, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 2, "Should resolve both VB constants");

        let user_sym = syms.iter().find(|s| s.name == "Session:USER_ID");
        assert!(user_sym.is_some(), "Should resolve UserKey → USER_ID");

        let cart_sym = syms.iter().find(|s| s.name == "Session:CART");
        assert!(cart_sym.is_some(), "Should resolve CartKey → CART");

        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(reads.len(), 1);
    }

    #[test]
    fn test_vbnet_const_case_insensitive_resolution() {
        let code = r#"
Const USERKEY As String = "USER_ID"
Session(userkey) = 42
"#;
        let rel = RelPath::new("CaseTest.aspx.vb");
        let (syms, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 1, "Should resolve case-insensitively");
        assert_eq!(syms[0].name, "Session:USER_ID");
        assert_eq!(edges[0].kind, "writes_state");
    }

    #[test]
    fn test_unresolved_identifier_emits_unresolved_edge() {
        let code = r#"
protected void DoWork() {
    Session[SomeExternalKey] = 42;
    var x = Session[AnotherKey];
}
"#;
        let rel = RelPath::new("Unknown.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        // No symbols should be emitted for unresolved identifiers
        assert_eq!(
            syms.len(),
            0,
            "Unresolved identifiers should not produce symbols"
        );

        // Should have 2 unresolved edges
        let unresolved_writes: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "unresolved_state_write")
            .collect();
        let unresolved_reads: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "unresolved_state_read")
            .collect();
        assert_eq!(
            unresolved_writes.len(),
            1,
            "Should emit unresolved_state_write"
        );
        assert_eq!(
            unresolved_reads.len(),
            1,
            "Should emit unresolved_state_read"
        );

        // Verify the target contains the variable name
        assert!(
            unresolved_writes[0].target_name.contains("SomeExternalKey"),
            "Unresolved write should reference the variable name"
        );
        assert!(
            unresolved_reads[0].target_name.contains("AnotherKey"),
            "Unresolved read should reference the variable name"
        );

        // Verify metadata marks them as unresolved
        assert_eq!(
            unresolved_writes[0]
                .metadata
                .as_ref()
                .and_then(|m| m.get("unresolved"))
                .map(|s| s.as_str()),
            Some("true")
        );
    }

    #[test]
    fn test_mixed_literal_and_identifier_access() {
        let code = r#"
const string UserKey = "USER_ID";

Session["DirectKey"] = 1;
Session[UserKey] = 2;
Session[UnknownKey] = 3;
"#;
        let rel = RelPath::new("Mixed.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        // "DirectKey" (literal) + "USER_ID" (resolved) = 2 symbols
        assert_eq!(syms.len(), 2, "Should have 2 resolved symbols");

        // 3 total edges: 2 writes_state + 1 unresolved_state_write
        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        let unresolved: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "unresolved_state_write")
            .collect();
        assert_eq!(writes.len(), 2, "DirectKey and UserKey are resolved writes");
        assert_eq!(unresolved.len(), 1, "UnknownKey is unresolved");
    }

    #[test]
    fn test_build_const_table_csharp() {
        let source = r#"
const string Key1 = "VALUE1";
static readonly string Key2 = "VALUE2";
const int NotAString = 42;
"#;
        let table = build_const_table(source, "csharp");
        assert_eq!(table.get("Key1").map(|s| s.as_str()), Some("VALUE1"));
        assert_eq!(table.get("Key2").map(|s| s.as_str()), Some("VALUE2"));
        assert!(
            !table.contains_key("NotAString"),
            "int const should not match"
        );
    }

    #[test]
    fn test_build_const_table_vbnet() {
        let source = r#"
Const MyKey As String = "MY_VALUE"
Const NumberKey As Integer = 42
"#;
        let table = build_const_table(source, "vbnet");
        // VB lookups are stored lowercase
        assert_eq!(table.get("mykey").map(|s| s.as_str()), Some("MY_VALUE"));
        assert!(
            !table.contains_key("numberkey"),
            "Integer const should not match"
        );
    }

    // ── Cookie access tests ──────────────────────────────────────────────

    #[test]
    fn test_csharp_request_cookie_read() {
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    var sessionId = Request.Cookies["SessionId"];
    var theme = Request.Cookies["Theme"];
}
"#;
        let rel = RelPath::new("Page.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 2, "Should find 2 unique cookie keys");

        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(reads.len(), 2, "Request.Cookies are always reads");

        for e in &reads {
            let meta = e.metadata.as_ref().unwrap();
            assert_eq!(meta["state_type"], "Cookies");
            assert_eq!(meta["cookie_direction"], "Request");
        }
    }

    #[test]
    fn test_csharp_response_cookie_write() {
        let code = r#"
protected void SetCookie() {
    Response.Cookies["Theme"] = "dark";
    Response.Cookies["Lang"] = "en";
}
"#;
        let rel = RelPath::new("Settings.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 2);

        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        assert_eq!(writes.len(), 2, "Response.Cookies are always writes");

        for e in &writes {
            let meta = e.metadata.as_ref().unwrap();
            assert_eq!(meta["state_type"], "Cookies");
            assert_eq!(meta["cookie_direction"], "Response");
        }
    }

    #[test]
    fn test_vbnet_cookie_access() {
        let code = r#"
Public Sub Page_Load(sender As Object, e As EventArgs)
    Dim pref = Request.Cookies("UserPref")
    Response.Cookies("Theme") = "dark"
End Sub
"#;
        let rel = RelPath::new("Default.aspx.vb");
        let (syms, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 2);

        let reads: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "reads_state" && e.target_name.contains("Cookies"))
            .collect();
        let writes: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "writes_state" && e.target_name.contains("Cookies"))
            .collect();
        assert_eq!(reads.len(), 1, "Request.Cookies is a read");
        assert_eq!(writes.len(), 1, "Response.Cookies is a write");
    }

    #[test]
    fn test_cookie_identifier_resolution() {
        let code = r#"
const string CookieKey = "AUTH_TOKEN";
var token = Request.Cookies[CookieKey];
"#;
        let rel = RelPath::new("Auth.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Cookies:AUTH_TOKEN");

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "reads_state");
        assert!(edges[0].target_name.contains("Cookies:AUTH_TOKEN"));
    }
}
