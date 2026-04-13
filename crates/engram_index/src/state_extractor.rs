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

/// C# programmatic cache access: HttpRuntime.Cache.Insert/Add/Get/Remove
static CS_CACHE_API_RE: OnceLock<Regex> = OnceLock::new();

/// VB.NET programmatic cache access: HttpRuntime.Cache.Insert/Add/Get/Remove
static VB_CACHE_API_RE: OnceLock<Regex> = OnceLock::new();

/// Response.Cache output cache control: Response.Cache.SetCacheability etc.
static RESPONSE_CACHE_RE: OnceLock<Regex> = OnceLock::new();

/// SqlCacheDependency usage
static SQL_CACHE_DEP_RE: OnceLock<Regex> = OnceLock::new();

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
                source_start_line: line_idx as u32 + 1,
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
                    start_line: line_idx as u32 + 1,
                    end_line: line_idx as u32 + 1,
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
                    source_start_line: line_idx as u32 + 1,
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
                        start_line: line_idx as u32 + 1,
                        end_line: line_idx as u32 + 1,
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

// ── State Affinity Analysis ─────────────────────────────────────────────────

/// Analyze state access patterns to identify state key affinities.
///
/// Groups state keys by the methods that access them and emits `state_affinity`
/// edges between keys that are co-accessed within the same method. This data
/// feeds the State-to-API transformation, where clustered state keys suggest
/// natural API endpoint boundaries.
///
/// Also emits `json_schema_hint` metadata for ViewState keys (suggesting
/// the frontend state shape needed to replace them).
pub fn analyze_state_affinity(
    edges: &[ExtractedEdge],
    _rel_path: &RelPath,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut affinity_edges = Vec::new();
    let mut affinity_syms = Vec::new();

    // Step 1: Build method → [state_keys] map
    // Each edge has source_name = enclosing method, target_name = "state:Type:Key"
    let mut method_to_keys: HashMap<String, Vec<(&str, &str)>> = HashMap::new();
    // Track access patterns per key: which methods read/write it
    let mut key_access_methods: HashMap<String, HashSet<String>> = HashMap::new();
    let mut key_access_patterns: HashMap<String, HashSet<&str>> = HashMap::new();

    for edge in edges {
        // Only consider resolved state edges (reads_state / writes_state)
        if edge.kind != "reads_state" && edge.kind != "writes_state" {
            continue;
        }
        // target_name format: "state:Session:UserId" or similar
        if !edge.target_name.starts_with("state:") {
            continue;
        }

        let method = &edge.source_name;
        let state_key = edge.target_name.as_str();

        method_to_keys
            .entry(method.clone())
            .or_default()
            .push((state_key, edge.kind));

        key_access_methods
            .entry(state_key.to_string())
            .or_default()
            .insert(method.clone());

        key_access_patterns
            .entry(state_key.to_string())
            .or_default()
            .insert(edge.kind);
    }

    // Step 2: For each method with 2+ state keys, emit affinity edges
    let mut affinity_counts: HashMap<(String, String), (u32, HashSet<String>)> = HashMap::new();

    for (method, keys) in &method_to_keys {
        if keys.len() < 2 {
            continue;
        }
        // Deduplicate keys within this method
        let unique_keys: Vec<&str> = {
            let mut ks: Vec<&str> = keys.iter().map(|(k, _)| *k).collect();
            ks.sort_unstable();
            ks.dedup();
            ks
        };

        for i in 0..unique_keys.len() {
            for j in (i + 1)..unique_keys.len() {
                let (a, b) = if unique_keys[i] < unique_keys[j] {
                    (unique_keys[i].to_string(), unique_keys[j].to_string())
                } else {
                    (unique_keys[j].to_string(), unique_keys[i].to_string())
                };
                let entry = affinity_counts.entry((a, b)).or_default();
                entry.0 += 1;
                entry.1.insert(method.clone());
            }
        }
    }

    // Step 3: Emit affinity edges for co-accessed pairs
    for ((key_a, key_b), (count, methods)) in &affinity_counts {
        // Determine combined access pattern
        let pat_a = key_access_patterns.get(key_a);
        let pat_b = key_access_patterns.get(key_b);
        let a_writes = pat_a.is_some_and(|p| p.contains("writes_state"));
        let b_writes = pat_b.is_some_and(|p| p.contains("writes_state"));
        let access_pattern = match (a_writes, b_writes) {
            (true, true) => "write-write",
            (true, false) | (false, true) => "read-write",
            (false, false) => "read-read",
        };

        let mut meta = HashMap::with_capacity(3);
        meta.insert("method_count".into(), count.to_string());
        meta.insert("access_pattern".into(), access_pattern.into());
        meta.insert(
            "co_accessing_methods".into(),
            methods
                .iter()
                .take(10) // cap metadata size
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        );

        affinity_edges.push(ExtractedEdge {
            source_name: key_a.clone(),
            source_kind: "global_state",
            source_start_line: 0,
            source_language: "text",
            target_name: key_b.clone(),
            target_kind: Some("global_state"),
            target_start_line: None,
            kind: "state_affinity",
            metadata: Some(meta),
        });
    }

    // Step 4: Emit json_schema_hint symbols for ViewState keys
    for (key, methods) in &key_access_methods {
        if !key.contains(":ViewState:") {
            continue;
        }
        let pats = key_access_patterns.get(key);
        let is_read_only = pats.is_none_or(|p| !p.contains("writes_state"));

        let mut meta = HashMap::with_capacity(3);
        meta.insert(
            "json_schema_hint".into(),
            r#"{"type":"string"}"#.into(), // conservative default
        );
        meta.insert(
            "mutability".into(),
            if is_read_only {
                "read_only"
            } else {
                "read_write"
            }
            .into(),
        );
        meta.insert("accessor_count".into(), methods.len().to_string());

        // Suggest API endpoint based on key name
        let key_name = key.rsplit(':').next().unwrap_or(key);
        let suggested_controller = format!(
            "{}Controller",
            key_name.chars().next().map_or(String::new(), |c| {
                c.to_uppercase().to_string() + &key_name[c.len_utf8()..]
            })
        );
        meta.insert("suggested_api_endpoint".into(), suggested_controller);

        affinity_syms.push(ExtractedSymbol {
            name: format!("viewstate_schema:{}", key_name),
            kind: "global_state",
            start_line: 0,
            end_line: 0,
            metadata: Some(meta),
        });
    }

    // Step 5: Emit endpoint suggestions for Session key clusters
    // Group Session keys by their affinity cluster
    let mut session_clusters: HashMap<String, Vec<String>> = HashMap::new();
    for ((key_a, key_b), (count, _)) in &affinity_counts {
        if *count >= 2 && (key_a.contains(":Session:") || key_b.contains(":Session:")) {
            // Use the first key as cluster anchor
            let anchor = key_a.clone();
            session_clusters
                .entry(anchor.clone())
                .or_default()
                .push(key_b.clone());
            session_clusters.entry(anchor).or_default();
        }
    }

    for (anchor, related) in &session_clusters {
        if related.is_empty() {
            continue;
        }
        let anchor_name = anchor.rsplit(':').next().unwrap_or(anchor);
        let suggested = format!("{}Controller", capitalize_first(anchor_name));

        let mut meta = HashMap::with_capacity(3);
        meta.insert("suggested_api_endpoint".into(), suggested);
        meta.insert("cluster_size".into(), (related.len() + 1).to_string());
        meta.insert(
            "cluster_keys".into(),
            std::iter::once(anchor.as_str())
                .chain(related.iter().map(|s| s.as_str()))
                .take(10)
                .collect::<Vec<_>>()
                .join(", "),
        );

        affinity_syms.push(ExtractedSymbol {
            name: format!("session_cluster:{}", anchor_name),
            kind: "global_state",
            start_line: 0,
            end_line: 0,
            metadata: Some(meta),
        });
    }

    (affinity_syms, affinity_edges)
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

// ── Programmatic Cache API Detection (Phase 33) ──────────────────────────────

/// A detected programmatic cache API usage.
#[derive(Debug, Clone)]
pub struct CacheApiUsage {
    pub file: String,
    pub line: usize,
    pub api_type: CacheApiType,
    pub key: Option<String>,
}

/// Type of cache API detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheApiType {
    /// HttpRuntime.Cache.Insert / Cache.Insert / Cache.Add
    CacheInsert,
    /// HttpRuntime.Cache.Get / Cache.Get / Cache["key"]
    CacheGet,
    /// HttpRuntime.Cache.Remove
    CacheRemove,
    /// Response.Cache.SetCacheability / SetExpires / SetMaxAge etc.
    ResponseCache,
    /// new SqlCacheDependency(...)
    SqlCacheDependency,
}

fn cs_cache_api_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &CS_CACHE_API_RE,
        r#"(?i)(?:HttpRuntime\.Cache|Cache)\s*\.\s*(Insert|Add|Get|Remove)\s*\(\s*(?:"([^"]+)"|([A-Za-z_]\w*))"#,
        "cache_api_cs",
    )
}

fn vb_cache_api_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &VB_CACHE_API_RE,
        r#"(?i)(?:HttpRuntime\.Cache|Cache)\s*\.\s*(Insert|Add|Get|Remove)\s*\(\s*(?:"([^"]+)"|([A-Za-z_]\w*))"#,
        "cache_api_vb",
    )
}

fn response_cache_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &RESPONSE_CACHE_RE,
        r#"(?i)Response\.Cache\s*\.\s*(SetCacheability|SetExpires|SetMaxAge|SetSlidingExpiration|SetValidUntilExpires|SetNoStore|SetNoServerCaching|SetAllowResponseInBrowserHistory|VaryByHeaders|VaryByParams)"#,
        "response_cache",
    )
}

fn sql_cache_dep_regex() -> Option<&'static Regex> {
    get_compiled_regex(
        &SQL_CACHE_DEP_RE,
        r#"(?i)(?:new\s+)?SqlCacheDependency\s*\(\s*"([^"]+)"\s*,\s*"([^"]+)""#,
        "sql_cache_dep",
    )
}

/// Extract all programmatic cache API usages from a source file.
///
/// Detects HttpRuntime.Cache operations, Response.Cache directives,
/// and SqlCacheDependency instantiation.
pub fn extract_cache_api_usages(
    file_path: &str,
    source: &str,
    language: &str,
) -> Vec<CacheApiUsage> {
    let mut results = Vec::new();

    // Cache.Insert/Add/Get/Remove
    let cache_re = match language {
        "csharp" => cs_cache_api_regex(),
        "vbnet" => vb_cache_api_regex(),
        _ => None,
    };
    if let Some(re) = cache_re {
        for (line_idx, line) in source.lines().enumerate() {
            for caps in re.captures_iter(line) {
                let method = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let key = caps
                    .get(2)
                    .or_else(|| caps.get(3))
                    .map(|m| m.as_str().to_string());
                let api_type = match method.to_lowercase().as_str() {
                    "insert" | "add" => CacheApiType::CacheInsert,
                    "get" => CacheApiType::CacheGet,
                    "remove" => CacheApiType::CacheRemove,
                    _ => CacheApiType::CacheGet,
                };
                results.push(CacheApiUsage {
                    file: file_path.to_string(),
                    line: line_idx + 1,
                    api_type,
                    key,
                });
            }
        }
    }

    // Response.Cache.Set*
    if let Some(re) = response_cache_regex() {
        for (line_idx, line) in source.lines().enumerate() {
            if re.is_match(line) {
                results.push(CacheApiUsage {
                    file: file_path.to_string(),
                    line: line_idx + 1,
                    api_type: CacheApiType::ResponseCache,
                    key: None,
                });
            }
        }
    }

    // SqlCacheDependency
    if let Some(re) = sql_cache_dep_regex() {
        for (line_idx, line) in source.lines().enumerate() {
            for caps in re.captures_iter(line) {
                let db = caps.get(1).map(|m| m.as_str()).unwrap_or("?");
                let table = caps.get(2).map(|m| m.as_str()).unwrap_or("?");
                results.push(CacheApiUsage {
                    file: file_path.to_string(),
                    line: line_idx + 1,
                    api_type: CacheApiType::SqlCacheDependency,
                    key: Some(format!("{db}.{table}")),
                });
            }
        }
    }

    results
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    // ── State Affinity Analysis ──────────────────────────────────────────

    #[test]
    fn test_state_affinity_same_method() {
        // Two Session keys read in the same method should produce a state_affinity edge
        let code = r#"
public class Dashboard : Page {
    protected void Page_Load(object sender, EventArgs e) {
        var user = Session["UserId"];
        var prefs = Session["UserPrefs"];
    }
}
"#;
        let rel = RelPath::new("Dashboard.aspx.cs");
        let (_, edges) = extract_state_accesses(&rel, code, "csharp");

        // Now run affinity analysis
        let (_, affinity_edges) = analyze_state_affinity(&edges, &rel);

        assert!(
            !affinity_edges.is_empty(),
            "expected state_affinity edges for co-accessed keys"
        );
        assert!(affinity_edges.iter().all(|e| e.kind == "state_affinity"));
        let first = &affinity_edges[0];
        let meta = first.metadata.as_ref().unwrap();
        assert_eq!(meta.get("method_count").unwrap(), "1");
        assert!(meta.get("access_pattern").unwrap() == "read-read");
    }

    #[test]
    fn test_state_affinity_viewstate_schema_hint() {
        let code = r#"
public class Form : Page {
    protected void Page_Load(object sender, EventArgs e) {
        ViewState["EditMode"] = true;
        var mode = ViewState["EditMode"];
    }
}
"#;
        let rel = RelPath::new("Form.aspx.cs");
        let (_, edges) = extract_state_accesses(&rel, code, "csharp");
        let (syms, _) = analyze_state_affinity(&edges, &rel);

        let vs_syms: Vec<_> = syms
            .iter()
            .filter(|s| s.name.starts_with("viewstate_schema:"))
            .collect();
        assert!(
            !vs_syms.is_empty(),
            "expected ViewState schema hint symbols"
        );
        let meta = vs_syms[0].metadata.as_ref().unwrap();
        assert!(meta.contains_key("json_schema_hint"));
        assert!(meta.contains_key("mutability"));
    }

    #[test]
    fn test_state_affinity_no_single_key() {
        // A method with only one state key should NOT produce affinity edges
        let code = r#"
public class Simple : Page {
    protected void Page_Load(object sender, EventArgs e) {
        var user = Session["UserId"];
    }
}
"#;
        let rel = RelPath::new("Simple.aspx.cs");
        let (_, edges) = extract_state_accesses(&rel, code, "csharp");
        let (_, affinity_edges) = analyze_state_affinity(&edges, &rel);
        assert!(
            affinity_edges.is_empty(),
            "single key should not produce affinity edges"
        );
    }

    // ── New tests: ViewState read/write ────────────────────────────────────

    #[test]
    fn test_viewstate_key_read() {
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    var col = ViewState["SortColumn"];
}
"#;
        let rel = RelPath::new("Grid.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "ViewState:SortColumn");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "reads_state");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["state_type"], "ViewState");
    }

    #[test]
    fn test_viewstate_key_write() {
        let code = r#"
protected void Sort_Click(object sender, EventArgs e) {
    ViewState["SortColumn"] = "Name";
    ViewState["SortDir"] = "ASC";
}
"#;
        let rel = RelPath::new("Grid.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 2);
        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        assert_eq!(
            writes.len(),
            2,
            "Both ViewState assignments should be writes"
        );
    }

    #[test]
    fn test_viewstate_write_then_read_same_key() {
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    ViewState["PageIndex"] = 0;
    var idx = ViewState["PageIndex"];
}
"#;
        let rel = RelPath::new("Pager.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        // Same key written then read — should be ONE unique symbol
        assert_eq!(syms.len(), 1, "One unique ViewState key");
        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        let reads: Vec<_> = edges.iter().filter(|e| e.kind == "reads_state").collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(reads.len(), 1);
    }

    // ── New tests: Session with string literal key ─────────────────────────

    #[test]
    fn test_session_key_string_literal_extracted() {
        let code = r#"
protected void Login() {
    Session["UserID"] = currentUser.Id;
}
"#;
        let rel = RelPath::new("Login.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Session:UserID");
        assert_eq!(edges[0].kind, "writes_state");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["state_key"], "UserID");
    }

    #[test]
    fn test_session_key_variable_emits_unresolved() {
        // Session[userKey] where userKey is not a local const → unresolved
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    var id = Session[userKey];
}
"#;
        let rel = RelPath::new("Page.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 0, "Unknown identifier → no resolved symbol");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "unresolved_state_read");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["identifier"], "userKey");
        assert_eq!(meta["unresolved"], "true");
    }

    // ── New tests: Application state ──────────────────────────────────────

    #[test]
    fn test_application_state_read() {
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    var cfg = Application["Config"];
}
"#;
        let rel = RelPath::new("Base.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Application:Config");
        assert_eq!(edges[0].kind, "reads_state");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["state_type"], "Application");
    }

    #[test]
    fn test_application_state_write() {
        let code = r#"
void Application_Start() {
    Application["Startup"] = DateTime.Now;
    Application["MaxUsers"] = 100;
}
"#;
        let rel = RelPath::new("Global.asax.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 2);
        let writes: Vec<_> = edges.iter().filter(|e| e.kind == "writes_state").collect();
        assert_eq!(writes.len(), 2);
    }

    // ── New tests: Cache via bracket syntax ───────────────────────────────

    #[test]
    fn test_cache_read_access() {
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    var data = Cache["ProductList"];
}
"#;
        let rel = RelPath::new("Products.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Cache:ProductList");
        assert_eq!(edges[0].kind, "reads_state");
    }

    #[test]
    fn test_cache_write_access() {
        let code = r#"
protected void PopulateCache() {
    Cache["ProductList"] = products;
}
"#;
        let rel = RelPath::new("Products.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(edges[0].kind, "writes_state");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["state_type"], "Cache");
    }

    // ── New tests: HttpContext.Current.Session / Items ─────────────────────

    #[test]
    fn test_httpcontext_current_session_key() {
        let code = r#"
var token = HttpContext.Current.Session["AuthToken"];
"#;
        let rel = RelPath::new("Global.asax.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        // Normalized to Session:AuthToken
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Session:AuthToken");
        assert_eq!(edges[0].kind, "reads_state");
    }

    #[test]
    fn test_httpcontext_current_items_write() {
        let code = r#"
HttpContext.Current.Items["RequestId"] = Guid.NewGuid().ToString();
"#;
        let rel = RelPath::new("Module.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Items:RequestId");
        assert_eq!(edges[0].kind, "writes_state");
    }

    // ── New tests: Cookies ─────────────────────────────────────────────────

    #[test]
    fn test_request_cookie_read_is_always_read() {
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    var pref = Request.Cookies["UserPref"];
}
"#;
        let rel = RelPath::new("Prefs.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Cookies:UserPref");
        assert_eq!(edges[0].kind, "reads_state");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["cookie_direction"], "Request");
    }

    #[test]
    fn test_response_cookie_write_is_always_write() {
        let code = r#"
protected void SetSession() {
    Response.Cookies["session_id"] = Guid.NewGuid().ToString();
}
"#;
        let rel = RelPath::new("Auth.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 1);
        assert_eq!(edges[0].kind, "writes_state");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["cookie_direction"], "Response");
    }

    #[test]
    fn test_vbnet_request_cookie_read() {
        let code = r#"
Public Sub Page_Load(sender As Object, e As EventArgs)
    Dim pref = Request.Cookies("ThemePref")
End Sub
"#;
        let rel = RelPath::new("Page.aspx.vb");
        let (syms, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Cookies:ThemePref");
        assert_eq!(edges[0].kind, "reads_state");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["cookie_direction"], "Request");
    }

    #[test]
    fn test_vbnet_response_cookie_write() {
        let code = r#"
Public Sub SetCookie()
    Response.Cookies("lang") = "en-US"
End Sub
"#;
        let rel = RelPath::new("Lang.aspx.vb");
        let (syms, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 1);
        assert_eq!(edges[0].kind, "writes_state");
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["cookie_direction"], "Response");
    }

    // ── New tests: VB.NET ViewState ───────────────────────────────────────

    #[test]
    fn test_vbnet_viewstate_read() {
        let code = r#"
Public Sub Page_Load(sender As Object, e As EventArgs)
    Dim col = ViewState("SortColumn")
End Sub
"#;
        let rel = RelPath::new("Grid.aspx.vb");
        let (syms, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "ViewState:SortColumn");
        assert_eq!(edges[0].kind, "reads_state");
    }

    #[test]
    fn test_vbnet_viewstate_write() {
        let code = r#"
Public Sub SaveState()
    ViewState("CurrentPage") = 3
End Sub
"#;
        let rel = RelPath::new("Pager.aspx.vb");
        let (syms, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 1);
        assert_eq!(edges[0].kind, "writes_state");
    }

    // ── New tests: mixed state types in one method ─────────────────────────

    #[test]
    fn test_multiple_state_types_in_one_method() {
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    var userId = Session["UserId"];
    ViewState["SortColumn"] = "Name";
    var cfg = Application["Config"];
    var cached = Cache["Products"];
}
"#;
        let rel = RelPath::new("Complex.aspx.cs");
        let (syms, _edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(syms.len(), 4, "Four unique state keys across four stores");

        let session_syms: Vec<_> = syms
            .iter()
            .filter(|s| s.name.starts_with("Session:"))
            .collect();
        let viewstate_syms: Vec<_> = syms
            .iter()
            .filter(|s| s.name.starts_with("ViewState:"))
            .collect();
        let app_syms: Vec<_> = syms
            .iter()
            .filter(|s| s.name.starts_with("Application:"))
            .collect();
        let cache_syms: Vec<_> = syms
            .iter()
            .filter(|s| s.name.starts_with("Cache:"))
            .collect();

        assert_eq!(session_syms.len(), 1);
        assert_eq!(viewstate_syms.len(), 1);
        assert_eq!(app_syms.len(), 1);
        assert_eq!(cache_syms.len(), 1);
    }

    // ── New tests: comment lines skipped ──────────────────────────────────

    #[test]
    fn test_csharp_comment_lines_skipped() {
        let code = r#"
protected void Load() {
    // Session["IgnoredKey"] = "this should not be detected";
    var x = Session["RealKey"];
}
"#;
        let rel = RelPath::new("Page.aspx.cs");
        let (syms, _edges) = extract_state_accesses(&rel, code, "csharp");

        // Only RealKey should be detected, not IgnoredKey
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Session:RealKey");
    }

    #[test]
    fn test_vbnet_comment_lines_skipped() {
        let code = r#"
Public Sub Load()
    ' Session("IgnoredKey") = "should not be detected"
    Dim x = Session("RealKey")
End Sub
"#;
        let rel = RelPath::new("Page.aspx.vb");
        let (syms, _edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Session:RealKey");
    }

    // ── New tests: unique symbol deduplication ─────────────────────────────

    #[test]
    fn test_same_key_accessed_multiple_times_produces_one_symbol() {
        let code = r#"
protected void Page_Load(object sender, EventArgs e) {
    var id = Session["UserId"];
    if (Session["UserId"] != null) {
        Session["UserId"] = Session["UserId"].ToString();
    }
}
"#;
        let rel = RelPath::new("Page.aspx.cs");
        let (syms, edges) = extract_state_accesses(&rel, code, "csharp");

        // Only ONE unique symbol despite multiple accesses
        assert_eq!(syms.len(), 1, "Same key should produce only one symbol");
        // But multiple edges
        assert!(
            edges.len() >= 2,
            "Multiple accesses should produce multiple edges"
        );
    }

    // ── New tests: state affinity analysis ────────────────────────────────

    #[test]
    fn test_affinity_with_mixed_read_write_pattern() {
        let code = r#"
public class UserPage : Page {
    protected void Page_Load(object sender, EventArgs e) {
        Session["UserId"] = 1;
        var prefs = Session["Prefs"];
    }
}
"#;
        let rel = RelPath::new("UserPage.aspx.cs");
        let (_, edges) = extract_state_accesses(&rel, code, "csharp");
        let (_, affinity_edges) = analyze_state_affinity(&edges, &rel);

        assert!(
            !affinity_edges.is_empty(),
            "Write+read pair should produce affinity edge"
        );
        let edge = &affinity_edges[0];
        let meta = edge.metadata.as_ref().unwrap();
        assert_eq!(
            meta["access_pattern"], "read-write",
            "One write and one read should produce read-write pattern"
        );
    }

    #[test]
    fn test_affinity_write_write_pattern() {
        let code = r#"
public class Login : Page {
    protected void LogIn(object sender, EventArgs e) {
        Session["UserId"] = user.Id;
        Session["Token"] = auth.Token;
    }
}
"#;
        let rel = RelPath::new("Login.aspx.cs");
        let (_, edges) = extract_state_accesses(&rel, code, "csharp");
        let (_, affinity_edges) = analyze_state_affinity(&edges, &rel);

        assert!(
            !affinity_edges.is_empty(),
            "Two writes should produce affinity edge"
        );
        let edge = &affinity_edges[0];
        let meta = edge.metadata.as_ref().unwrap();
        assert_eq!(meta["access_pattern"], "write-write");
    }

    #[test]
    fn test_affinity_edges_have_state_affinity_kind() {
        let code = r#"
public class Page1 : Page {
    protected void Page_Load(object sender, EventArgs e) {
        var a = Session["A"];
        var b = Session["B"];
    }
}
"#;
        let rel = RelPath::new("P.aspx.cs");
        let (_, edges) = extract_state_accesses(&rel, code, "csharp");
        let (_, affinity_edges) = analyze_state_affinity(&edges, &rel);

        for edge in &affinity_edges {
            assert_eq!(
                edge.kind, "state_affinity",
                "All affinity edges should have kind 'state_affinity'"
            );
        }
    }

    // ── New tests: cache API usages ───────────────────────────────────────

    #[test]
    fn test_cache_insert_detected() {
        let code = r#"
Cache.Insert("Products", data, null, absoluteExpiration, Cache.NoSlidingExpiration);
"#;
        let usages = extract_cache_api_usages("Products.aspx.cs", code, "csharp");
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].api_type, CacheApiType::CacheInsert);
        assert_eq!(usages[0].key.as_deref(), Some("Products"));
    }

    #[test]
    fn test_cache_get_detected() {
        let code = r#"
var products = Cache.Get("Products");
"#;
        let usages = extract_cache_api_usages("Products.aspx.cs", code, "csharp");
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].api_type, CacheApiType::CacheGet);
        assert_eq!(usages[0].key.as_deref(), Some("Products"));
    }

    #[test]
    fn test_cache_remove_detected() {
        let code = r#"
Cache.Remove("Products");
"#;
        let usages = extract_cache_api_usages("Products.aspx.cs", code, "csharp");
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].api_type, CacheApiType::CacheRemove);
    }

    #[test]
    fn test_response_cache_detected() {
        let code = r#"
Response.Cache.SetCacheability(HttpCacheability.Public);
Response.Cache.SetExpires(DateTime.Now.AddMinutes(30));
"#;
        let usages = extract_cache_api_usages("Page.aspx.cs", code, "csharp");
        let response_cache: Vec<_> = usages
            .iter()
            .filter(|u| u.api_type == CacheApiType::ResponseCache)
            .collect();
        assert_eq!(
            response_cache.len(),
            2,
            "Both Response.Cache calls should be detected"
        );
    }

    #[test]
    fn test_sql_cache_dependency_detected() {
        let code = r#"
var dep = new SqlCacheDependency("MyDB", "Products");
"#;
        let usages = extract_cache_api_usages("Page.aspx.cs", code, "csharp");
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].api_type, CacheApiType::SqlCacheDependency);
        assert_eq!(usages[0].key.as_deref(), Some("MyDB.Products"));
    }

    #[test]
    fn test_http_runtime_cache_insert() {
        let code = r#"
HttpRuntime.Cache.Insert("key1", value, null, DateTime.Now.AddHours(1), Cache.NoSlidingExpiration);
"#;
        let usages = extract_cache_api_usages("Page.aspx.cs", code, "csharp");
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].api_type, CacheApiType::CacheInsert);
        assert_eq!(usages[0].key.as_deref(), Some("key1"));
    }

    // ── New tests: unsupported language returns empty ──────────────────────

    #[test]
    fn test_unsupported_language_returns_empty() {
        let code = r#"
Session["key"] = "value";
"#;
        let rel = RelPath::new("Unknown.jsx");
        let (syms, edges) = extract_state_accesses(&rel, code, "javascript");
        assert!(
            syms.is_empty(),
            "Unsupported language should produce no symbols"
        );
        assert!(
            edges.is_empty(),
            "Unsupported language should produce no edges"
        );
    }

    // ── New tests: enclosing method name captured ─────────────────────────

    #[test]
    fn test_enclosing_method_name_in_edge_source() {
        let code = r#"
public class MyPage : Page {
    protected void Page_Load(object sender, EventArgs e) {
        var id = Session["UserId"];
    }
}
"#;
        let rel = RelPath::new("My.aspx.cs");
        let (_, edges) = extract_state_accesses(&rel, code, "csharp");

        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].source_name, "Page_Load",
            "Edge source should be the enclosing method name"
        );
    }

    #[test]
    fn test_vbnet_enclosing_method_captured() {
        let code = r#"
Public Class BasePage
    Public Sub SaveUser(sender As Object, e As EventArgs)
        Session("UserName") = "Alice"
    End Sub
End Class
"#;
        let rel = RelPath::new("Base.aspx.vb");
        let (_, edges) = extract_state_accesses(&rel, code, "vbnet");

        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].source_name, "SaveUser",
            "Edge source should be the enclosing VB method name"
        );
    }
}
