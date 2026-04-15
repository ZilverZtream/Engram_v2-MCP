use std::path::Path;
use std::sync::LazyLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

#[derive(Debug, Clone)]
pub struct ExtractedSymbol {
    pub name: String,
    pub kind: String, // "function" | "class" | "struct" | "impl" | …
    pub start_line: u32,
    pub end_line: u32,
    pub metadata: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Clone)]
pub struct ExtractedEdge {
    pub source_name: String,
    pub source_kind: String,
    pub source_start_line: u32,
    pub source_language: String,
    pub target_name: String,
    pub target_kind: Option<String>,
    pub target_start_line: Option<u32>,
    pub kind: String, // "calls" | "contains" | "imports" | …
    pub metadata: Option<std::collections::HashMap<String, String>>,
}

/// Fully-qualified name table: maps `short_name` → `fqn`.
/// Built during Pass 1 of two-pass call graph extraction.
type FqnTable = std::collections::HashMap<String, String>;

/// Check if a function name matches a WebForms page lifecycle method.
/// Returns `(lifecycle_stage, sequence_number)` if it matches.
fn webforms_lifecycle_info(name: &str) -> Option<(&'static str, u32)> {
    match name.to_lowercase().as_str() {
        "page_preinit" => Some(("PreInit", 1)),
        "page_init" => Some(("Init", 2)),
        "page_initcomplete" => Some(("InitComplete", 3)),
        "page_preload" => Some(("PreLoad", 4)),
        "page_load" => Some(("Load", 5)),
        "page_loadcomplete" => Some(("LoadComplete", 6)),
        "page_prerender" => Some(("PreRender", 7)),
        "page_prerendercomplete" => Some(("PreRenderComplete", 8)),
        "page_savestatecomplete" => Some(("SaveStateComplete", 9)),
        "page_render" | "render" => Some(("Render", 10)),
        "page_unload" => Some(("Unload", 11)),
        // Override forms: OnInit, OnLoad, OnPreRender, OnUnload
        "oninit" => Some(("Init", 2)),
        "onload" => Some(("Load", 5)),
        "onprerender" => Some(("PreRender", 7)),
        "onunload" => Some(("Unload", 11)),
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Compiled Tree-Sitter queries — initialized once via LazyLock (Opt 8).
// ──────────────────────────────────────────────────────────────────────────

struct CompiledQueries {
    rust: Option<Query>,
    python: Option<Query>,
    go: Option<Query>,
    java: Option<Query>,
    ts: Option<Query>,
    js: Option<Query>,
    cs: Option<Query>,
    c: Option<Query>,
    cpp: Option<Query>,
    /// Pass-1 namespace query for C#.
    cs_ns: Option<Query>,
    /// Pass-1 namespace query for Java.
    java_ns: Option<Query>,
    /// Pass-1 namespace query for Go (package declaration).
    go_ns: Option<Query>,
    /// Pass-1 module query for Rust (mod items).
    rust_mod: Option<Query>,
}

// SAFETY: tree_sitter::Query is Send + Sync (immutable compiled pattern data).
unsafe impl Send for CompiledQueries {}
unsafe impl Sync for CompiledQueries {}

static QUERIES: LazyLock<CompiledQueries> = LazyLock::new(|| {
    let rust_lang = tree_sitter_rust::LANGUAGE.into();
    let python_lang = tree_sitter_python::LANGUAGE.into();
    let go_lang = tree_sitter_go::LANGUAGE.into();
    let java_lang = tree_sitter_java::LANGUAGE.into();
    let ts_lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    let js_lang = tree_sitter_javascript::LANGUAGE.into();
    let cs_lang = tree_sitter_c_sharp::LANGUAGE.into();
    let c_lang = tree_sitter_c::LANGUAGE.into();
    let cpp_lang = tree_sitter_cpp::LANGUAGE.into();

    let rust = Query::new(
        &rust_lang,
        r#"
        (function_item name: (identifier) @name) @func
        (struct_item name: (type_identifier) @name) @struct
        (impl_item type: (type_identifier) @name) @impl
        (call_expression function: (identifier) @call.name)
        (call_expression function: (field_expression field: (field_identifier) @call.name))
        "#,
    )
    .ok();

    let python = Query::new(
        &python_lang,
        r#"
        (function_definition name: (identifier) @name) @func
        (class_definition name: (identifier) @name) @class
        (call function: (identifier) @call.name)
        (call function: (attribute attribute: (identifier) @call.name))
        "#,
    )
    .ok();

    let go = Query::new(
        &go_lang,
        r#"
        (function_declaration name: (identifier) @name) @func
        (method_declaration name: (field_identifier) @name) @func
        (type_declaration (type_spec name: (type_identifier) @name)) @class
        (call_expression function: (identifier) @call.name)
        (call_expression function: (selector_expression field: (field_identifier) @call.name))
        "#,
    )
    .ok();

    let java = Query::new(
        &java_lang,
        r#"
        (method_declaration name: (identifier) @name) @func
        (class_declaration name: (identifier) @name) @class
        (interface_declaration name: (identifier) @name) @class
        (method_invocation name: (identifier) @call.name)
        "#,
    )
    .ok();

    let ts = Query::new(
        &ts_lang,
        r#"
        (function_declaration name: (identifier) @name) @func
        (class_declaration name: (type_identifier) @name) @class
        (interface_declaration name: (type_identifier) @name) @class
        (call_expression function: (identifier) @call.name)
        (call_expression function: (member_expression property: (property_identifier) @call.name))
        "#,
    )
    .ok();

    let js = Query::new(
        &js_lang,
        r#"
        (function_declaration name: (identifier) @name) @func
        (class_declaration name: (identifier) @name) @class
        (call_expression function: (identifier) @call.name)
        (call_expression function: (member_expression property: (property_identifier) @call.name))
        "#,
    )
    .ok();

    let cs = Query::new(
        &cs_lang,
        r#"
        (method_declaration name: (identifier) @name) @func
        (class_declaration name: (identifier) @name) @class
        (interface_declaration name: (identifier) @name) @class
        (struct_declaration name: (identifier) @name) @class
        (enum_declaration name: (identifier) @name) @class
        (record_declaration name: (identifier) @name) @class
        (field_declaration (variable_declaration (variable_declarator (identifier) @name))) @field
        (invocation_expression function: (identifier) @call.name)
        (invocation_expression function: (member_access_expression name: (identifier) @call.name))

        ; SQL extraction
        (object_creation_expression
            type: (identifier) @type_name (#eq? @type_name "SqlCommand")
            arguments: (argument_list (argument (string_literal) @sql.literal))
        ) @sql.cmd

        (assignment_expression
            left: (member_access_expression name: (identifier) @prop_name (#eq? @prop_name "CommandText"))
            right: (string_literal) @sql.literal
        ) @sql.assign
        "#,
    )
    .map_err(|e| {
        tracing::error!(error = %e, "failed to compile C# Tree-sitter query; disabling C# parsing patterns");
        e
    })
    .ok();

    let c = Query::new(
        &c_lang,
        r#"
        (function_definition declarator: (function_declarator declarator: (identifier) @name)) @func
        (struct_specifier name: (type_identifier) @name) @struct
        (call_expression function: (identifier) @call.name)
        "#,
    )
    .ok();

    let cpp = Query::new(
        &cpp_lang,
        r#"
        (function_definition declarator: (function_declarator declarator: (identifier) @name)) @func
        (function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @func
        (class_specifier name: (type_identifier) @name) @class
        (call_expression function: (identifier) @call.name)
        (call_expression function: (field_expression field: (field_identifier) @call.name))
        (call_expression function: (qualified_identifier name: (identifier) @call.name))
        (call_expression function: (qualified_identifier name: (field_identifier) @call.name))
        "#,
    )
    .ok();

    // ── Pass-1 namespace queries ──────────────────────────────────────────
    let cs_ns = Query::new(
        &cs_lang,
        r#"
        (namespace_declaration name: (_) @ns)
        (class_declaration name: (identifier) @class)
        (interface_declaration name: (identifier) @class)
        (method_declaration name: (identifier) @method)
        "#,
    )
    .ok();

    let java_ns = Query::new(
        &java_lang,
        r#"
        (package_declaration (scoped_identifier) @ns)
        (class_declaration name: (identifier) @class)
        (interface_declaration name: (identifier) @class)
        (method_declaration name: (identifier) @method)
        "#,
    )
    .ok();

    let go_ns = Query::new(
        &go_lang,
        r#"
        (package_clause (package_identifier) @ns)
        (type_declaration (type_spec name: (type_identifier) @class))
        (function_declaration name: (identifier) @method)
        (method_declaration name: (field_identifier) @method)
        "#,
    )
    .ok();

    let rust_mod = Query::new(
        &rust_lang,
        r#"
        (mod_item name: (identifier) @ns)
        (struct_item name: (type_identifier) @struct)
        (impl_item type: (type_identifier) @class)
        (function_item name: (identifier) @method)
        "#,
    )
    .ok();

    CompiledQueries {
        rust,
        python,
        go,
        java,
        ts,
        js,
        cs,
        c,
        cpp,
        cs_ns,
        java_ns,
        go_ns,
        rust_mod,
    }
});

/// Map a file extension to a `&'static str` for source_language fields.
fn ext_to_static(ext: &str) -> &'static str {
    match ext {
        "rs" => "rs",
        "py" => "py",
        "go" => "go",
        "java" => "java",
        "ts" => "ts",
        "tsx" => "tsx",
        "js" => "js",
        "jsx" => "jsx",
        "cs" => "cs",
        "c" => "c",
        "h" => "h",
        "cpp" => "cpp",
        "hpp" => "hpp",
        "cc" => "cc",
        "cxx" => "cxx",
        "hh" => "hh",
        "vb" => "vb",
        _ => "unknown",
    }
}

// ──────────────────────────────────────────────────────────────────────────
// SymbolExtractor — zero-sized; queries live in the static LazyLock.
// ──────────────────────────────────────────────────────────────────────────

pub struct SymbolExtractor {
    // All queries live in `QUERIES` (LazyLock).  This struct is kept for API
    // compatibility; it is zero-sized after optimisation.
    _priv: (),
}

impl Default for SymbolExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolExtractor {
    pub fn new() -> Self {
        // Force initialisation of the lazy static on first construction so any
        // query-compilation panic happens early rather than mid-extraction.
        let _ = &*QUERIES;
        Self { _priv: () }
    }

    /// Pass 1: Build an FQN symbol table for a source file.
    ///
    /// Returns a map from unqualified name → fully-qualified name.
    /// e.g. `"DoSomething"` → `"MyNamespace.MyClass.DoSomething"`
    fn build_fqn_table(&self, ext: &str, lang: &tree_sitter::Language, content: &str) -> FqnTable {
        let query_opt = match ext {
            "cs" => &QUERIES.cs_ns,
            "java" => &QUERIES.java_ns,
            "go" => &QUERIES.go_ns,
            "rs" => &QUERIES.rust_mod,
            _ => return FqnTable::new(),
        };
        let Some(query) = query_opt else {
            return FqnTable::new();
        };

        let mut parser = Parser::new();
        parser.set_language(lang).ok();
        let Some(tree) = parser.parse(content, None) else {
            return FqnTable::new();
        };

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), content.as_bytes());

        // Track nested namespace/class scopes using node byte ranges.
        // Each scope is (end_byte, name) — we prune scopes that have ended.
        let mut ns_stack: Vec<(usize, String)> = Vec::new();
        let mut class_stack: Vec<(usize, String)> = Vec::new();
        let mut table = FqnTable::new();

        while let Some(m) = matches.next() {
            for cap in m.captures {
                let tag = query.capture_names()[cap.index as usize];
                let text = &content[cap.node.start_byte()..cap.node.end_byte()];
                let node_start = cap.node.start_byte();

                // Prune any scopes that have ended before this node starts
                ns_stack.retain(|(end, _)| *end > node_start);
                class_stack.retain(|(end, _)| *end > node_start);

                match tag {
                    "ns" => {
                        let end_byte = cap
                            .node
                            .parent()
                            .map(|p| p.end_byte())
                            .unwrap_or(usize::MAX);
                        ns_stack.push((end_byte, text.to_string()));
                    }
                    "class" => {
                        let current_class = text.to_string();
                        let namespace: String = ns_stack
                            .iter()
                            .map(|(_, n)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(".");
                        let fqn = if namespace.is_empty() {
                            current_class.clone()
                        } else {
                            format!("{}.{}", namespace, current_class)
                        };
                        table.insert(current_class.clone(), fqn);
                        let end_byte = cap
                            .node
                            .parent()
                            .map(|p| p.end_byte())
                            .unwrap_or(usize::MAX);
                        class_stack.push((end_byte, current_class));
                    }
                    "method" => {
                        let short = text.to_string();
                        let namespace: String = ns_stack
                            .iter()
                            .map(|(_, n)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(".");
                        let current_class =
                            class_stack.last().map(|(_, c)| c.as_str()).unwrap_or("");
                        let fqn = if current_class.is_empty() {
                            if namespace.is_empty() {
                                short.clone()
                            } else {
                                format!("{}.{}", namespace, short)
                            }
                        } else if namespace.is_empty() {
                            format!("{}.{}", current_class, short)
                        } else {
                            format!("{}.{}.{}", namespace, current_class, short)
                        };
                        table.insert(short, fqn);
                    }
                    _ => {}
                }
            }
        }

        table
    }

    pub fn extract(
        &self,
        path: &Path,
        content: &str,
    ) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let static_ext = ext_to_static(ext);
        let (lang, query_opt): (tree_sitter::Language, Option<&Query>) = match ext {
            "rs" => (tree_sitter_rust::LANGUAGE.into(), QUERIES.rust.as_ref()),
            "py" => (tree_sitter_python::LANGUAGE.into(), QUERIES.python.as_ref()),
            "go" => (tree_sitter_go::LANGUAGE.into(), QUERIES.go.as_ref()),
            "java" => (tree_sitter_java::LANGUAGE.into(), QUERIES.java.as_ref()),
            "ts" | "tsx" => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                QUERIES.ts.as_ref(),
            ),
            "js" | "jsx" => (
                tree_sitter_javascript::LANGUAGE.into(),
                QUERIES.js.as_ref(),
            ),
            "cs" => (tree_sitter_c_sharp::LANGUAGE.into(), QUERIES.cs.as_ref()),
            "c" | "h" => (tree_sitter_c::LANGUAGE.into(), QUERIES.c.as_ref()),
            "cpp" | "hpp" | "cc" | "cxx" | "hh" => {
                (tree_sitter_cpp::LANGUAGE.into(), QUERIES.cpp.as_ref())
            }
            // "vb" is handled upstream by `vb_extractor::extract_vb`; never reaches here.
            _ => return (vec![], vec![]),
        };

        let Some(query) = query_opt else {
            return (vec![], vec![]);
        };

        let mut parser = Parser::new();
        let lang_set = parser.set_language(&lang);
        tracing::debug!("set_language result for ext {}: {:?}", ext, lang_set);

        let Some(tree) = parser.parse(content, None) else {
            tracing::error!("Tree-sitter FAILED to parse content for ext: {}", ext);
            return (vec![], vec![]);
        };
        tracing::debug!(
            "Parsed tree for ext: {}, root kind: {}",
            ext,
            tree.root_node().kind()
        );

        // ── Pass 1: build FQN table ───────────────────────────────────────────
        let fqn_table = self.build_fqn_table(ext, &lang, content);
        tracing::debug!("FQN table built, len={}", fqn_table.len());

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), content.as_bytes());

        let mut symbols = Vec::new();
        let mut edges = Vec::new();

        // For edge extraction, we need to know which symbol we are currently inside.
        // Simplified: find innermost symbol that encloses the call.
        // (start, end, name, kind, start_line, fqn)
        type SymbolRange = (usize, usize, String, &'static str, u32, Option<String>);
        let mut symbol_ranges: Vec<SymbolRange> = Vec::new();

        while let Some(match_) = matches.next() {
            for capture in match_.captures {
                let node = capture.node;
                let tag = query.capture_names()[capture.index as usize];
                tracing::trace!(
                    "Match found: tag={}, kind={}, text='{}'",
                    tag,
                    node.kind(),
                    &content[node.start_byte()..node.end_byte()]
                );
                if tag == "name" {
                    continue;
                }

                if tag.starts_with("call") {
                    let callee_short = &content[node.start_byte()..node.end_byte()];
                    // Pass 2: resolve callee to FQN if known.
                    let callee_fqn = fqn_table.get(callee_short).cloned();

                    // Find parent symbol
                    if let Some((_, _, parent_name, parent_kind, parent_line, parent_fqn)) =
                        symbol_ranges.iter().rev().find(|(s, e, _, _, _, _)| {
                            *s <= node.start_byte() && *e >= node.end_byte()
                        })
                    {
                        let mut meta = std::collections::HashMap::new();
                        let (target_name, target_kind): (String, Option<String>) =
                            if let Some(fqn) = callee_fqn {
                                (fqn, Some("function".to_string()))
                            } else {
                                meta.insert("unresolved".into(), "true".into());
                                (callee_short.to_string(), None)
                            };

                        edges.push(ExtractedEdge {
                            source_name: parent_fqn.as_ref().unwrap_or(parent_name).clone(),
                            source_kind: parent_kind.to_string(),
                            source_start_line: *parent_line,
                            source_language: static_ext.to_string(),
                            target_name,
                            target_kind,
                            target_start_line: None,
                            kind: "calls".to_string(),
                            metadata: if meta.is_empty() { None } else { Some(meta) },
                        });
                    }
                    continue;
                }

                if tag.starts_with("sql.literal") {
                    let mut sql_text = content[node.start_byte()..node.end_byte()].to_string();
                    // Strip quotes
                    if sql_text.len() >= 2
                        && ((sql_text.starts_with('"') && sql_text.ends_with('"'))
                            || (sql_text.starts_with('\'') && sql_text.ends_with('\'')))
                    {
                        sql_text = sql_text[1..sql_text.len() - 1].to_string();
                    }

                    // Find parent symbol
                    let (src_name, src_kind, src_line): (String, &'static str, u32) =
                        if let Some((_, _, parent_name, parent_kind, parent_line, parent_fqn)) =
                            symbol_ranges.iter().rev().find(|(s, e, _, _, _, _)| {
                                *s <= node.start_byte() && *e >= node.end_byte()
                            })
                        {
                            (
                                parent_fqn.as_ref().unwrap_or(parent_name).clone(),
                                *parent_kind,
                                *parent_line,
                            )
                        } else {
                            ("file".to_string(), "file", 0)
                        };

                    let (target_id, target_kind) = classify_cs_sql(&sql_text);

                    let mut meta = std::collections::HashMap::new();
                    meta.insert("sql_snippet".into(), sql_text.chars().take(200).collect());

                    edges.push(ExtractedEdge {
                        source_name: src_name,
                        source_kind: src_kind.to_string(),
                        source_start_line: src_line,
                        source_language: static_ext.to_string(),
                        target_name: target_id,
                        target_kind: Some(target_kind.to_string()),
                        target_start_line: None,
                        kind: "sql_calls".to_string(),
                        metadata: Some(meta),
                    });
                    continue;
                }

                if tag == "import" {
                    let imported = &content[node.start_byte()..node.end_byte()];
                    edges.push(ExtractedEdge {
                        source_name: "file".to_string(),
                        source_kind: "file".to_string(),
                        source_start_line: 0,
                        source_language: static_ext.to_string(),
                        target_name: imported.to_string(),
                        target_kind: None,
                        target_start_line: None,
                        kind: "imports".to_string(),
                        metadata: None,
                    });
                    continue;
                }

                let is_designer = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.contains(".designer."))
                    .unwrap_or(false);

                let kind: &'static str = match tag {
                    "func" => "function",
                    "class" => "class",
                    "struct" => "class",
                    "impl" => "impl",
                    "field" => {
                        if is_designer {
                            "control_ref"
                        } else {
                            "field"
                        }
                    }
                    _ => "symbol",
                };

                // Find the name sibling/child in THIS match
                let mut name = "anonymous".to_string();
                for sibling_capture in match_.captures {
                    if query.capture_names()[sibling_capture.index as usize] == "name" {
                        name = content
                            [sibling_capture.node.start_byte()..sibling_capture.node.end_byte()]
                            .to_string();
                        break;
                    }
                }

                let start_line = (node.start_position().row + 1) as u32;

                let mut meta = std::collections::HashMap::new();
                if let Some(fqn) = fqn_table.get(&name) {
                    meta.insert("fqn".into(), fqn.clone());
                }
                if is_designer && kind == "control_ref" {
                    meta.insert("is_designer".into(), "true".into());
                }

                // Tag WebForms lifecycle methods with stage + sequence metadata.
                if kind == "function"
                    && let Some((stage, seq)) = webforms_lifecycle_info(&name)
                {
                    meta.insert("lifecycle_stage".into(), stage.into());
                    meta.insert("lifecycle_sequence".into(), seq.to_string());
                }

                symbols.push(ExtractedSymbol {
                    name: name.clone(),
                    kind: kind.to_string(),
                    start_line,
                    end_line: (node.end_position().row + 1) as u32,
                    metadata: if meta.is_empty() { None } else { Some(meta) },
                });

                // Check for parent (contains relationship)
                if let Some((_, _, parent_name, parent_kind, parent_line, parent_fqn)) =
                    symbol_ranges
                        .iter()
                        .rev()
                        .find(|(s, e, _, _, _, _)| *s <= node.start_byte() && *e >= node.end_byte())
                {
                    let target_fqn = fqn_table.get(&name).cloned();
                    edges.push(ExtractedEdge {
                        source_name: parent_fqn.as_ref().unwrap_or(parent_name).clone(),
                        source_kind: parent_kind.to_string(),
                        source_start_line: *parent_line,
                        source_language: static_ext.to_string(),
                        target_name: target_fqn.unwrap_or_else(|| name.clone()),
                        target_kind: Some(kind.to_string()),
                        target_start_line: Some(start_line),
                        kind: "contains".to_string(),
                        metadata: None,
                    });
                }

                if matches!(tag, "func" | "class" | "struct" | "impl") {
                    symbol_ranges.push((
                        node.start_byte(),
                        node.end_byte(),
                        name.clone(),
                        kind,
                        start_line,
                        fqn_table.get(&name).cloned(),
                    ));
                }
            }
        }

        (symbols, edges)
    }
}

/// Classify a SQL string from C# extraction as stored-proc or inline.
///
/// Handles EXEC/EXECUTE prefix (common in legacy .NET), single-token names,
/// and falls back to inline SQL with blake3 hash.
fn classify_cs_sql(sql: &str) -> (String, &'static str) {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return ("sql:inline:empty".into(), "inline_sql");
    }

    // Check for EXEC/EXECUTE prefix (case-insensitive)
    let upper = trimmed.as_bytes();
    if upper.len() >= 5 {
        let starts_exec = upper[..4].eq_ignore_ascii_case(b"EXEC");
        if starts_exec {
            let rest = if upper.len() >= 8 && upper[..7].eq_ignore_ascii_case(b"EXECUTE") {
                trimmed[7..].trim_start()
            } else if upper.len() >= 5 && (upper[4] == b' ' || upper[4] == b'\t') {
                trimmed[5..].trim_start()
            } else {
                ""
            };
            if !rest.is_empty() {
                let proc_name = rest.split_whitespace().next().unwrap_or(rest);
                let clean: String = proc_name
                    .chars()
                    .filter(|&c| c != '[' && c != ']')
                    .collect();
                if !clean.is_empty() {
                    return (format!("sql:stored_proc:{clean}"), "stored_proc");
                }
            }
        }
    }

    // Single identifier → stored proc
    if !trimmed.contains(char::is_whitespace) && trimmed.len() > 2 {
        (format!("sql:stored_proc:{trimmed}"), "stored_proc")
    } else {
        let h = blake3::hash(trimmed.as_bytes()).to_hex().to_string();
        (format!("sql:inline:{}", &h[..12]), "inline_sql")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Compile-time proof that `CompiledQueries` is Send + Sync.
    #[test]
    fn compiled_queries_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<CompiledQueries>();
        assert_sync::<CompiledQueries>();
    }

    #[test]
    fn test_csharp_unresolved_call() {
        let code = r#"
namespace MyApp {
    public class MyClass {
        public void MyMethod() {
            UnknownMethod();
        }
    }
}
"#;
        let extractor = SymbolExtractor::new();
        let (_symbols, edges) = extractor.extract(Path::new("test.cs"), code);

        // Find edge from MyMethod to UnknownMethod
        let edge = edges
            .iter()
            .find(|e| e.target_name == "UnknownMethod")
            .expect("Should find edge to UnknownMethod");

        assert_eq!(edge.target_kind, None);
        assert_eq!(edge.metadata.as_ref().unwrap()["unresolved"], "true");
    }
}
