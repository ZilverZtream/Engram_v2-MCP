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

/// True when a trimmed line starts with a comment marker in any of the
/// languages this indexer handles (VB `'`, C-family `//`/`/*`/`*`,
/// scripting `#`, SQL/Lua `--`, XML `<!--`, asm/ini `;`, VB legacy `REM`).
fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with('\'')
        || trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with('*')
        || trimmed.starts_with("/*")
        || trimmed.starts_with("--")
        || trimmed.starts_with("<!--")
        || trimmed.starts_with(';')
        || trimmed.to_ascii_lowercase().starts_with("rem ")
}

/// Detect "must update in N places" sync-contract comments. These are
/// machine-readable maintenance contracts: a trigger line announcing that
/// some logic is duplicated across N sites, followed by an enumerated
/// list of those sites. Emitted as `sync_contract` symbols so the review
/// pipeline can assert when a diff touches a subset of the listed sites
/// (the classic failure: two of three copies updated, the third ships
/// stale — observed live as a missing CR-exclusion in a marker-import
/// delete path whose contract comment named all three sites).
pub fn detect_sync_contracts(text: &str) -> Vec<ExtractedSymbol> {
    use std::sync::LazyLock;
    static TRIGGER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:must|keep|update|sync|change)\b.{0,80}?\b(\d{1,2})\s+places")
            .expect("valid regex")
    });
    static SITE: LazyLock<regex::Regex> = LazyLock::new(|| {
        // An enumerated comment line: optional comment markers, then
        // `1.` / `2)` / `-` / `*`, then the site reference.
        regex::Regex::new(r#"^(?:['/#*;<!\- ]|rem )*\s*(?:\d{1,2}[.)]|[-*•])\s+(.{3,200})$"#)
            .expect("valid regex")
    });

    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if !is_comment_line(t) || !TRIGGER.is_match(t) {
            i += 1;
            continue;
        }
        let declared: usize = TRIGGER
            .captures(t)
            .and_then(|c| c[1].parse().ok())
            .unwrap_or(0);
        // Gather the enumerated site list from the following comment lines.
        let mut sites: Vec<String> = Vec::new();
        let mut j = i + 1;
        while j < lines.len() && j <= i + 12 {
            let lt = lines[j].trim_start();
            if !is_comment_line(lt) {
                break;
            }
            if let Some(cap) = SITE.captures(lt) {
                sites.push(cap[1].trim().to_string());
            } else if !sites.is_empty() {
                break; // enumeration ended
            }
            j += 1;
        }
        if sites.len() >= 2 {
            let mut meta = std::collections::HashMap::new();
            meta.insert("sites".to_string(), sites.join("||"));
            meta.insert("declared_places".to_string(), declared.to_string());
            out.push(ExtractedSymbol {
                name: format!("sync-contract ({} sites)", sites.len()),
                kind: "sync_contract".to_string(),
                start_line: (i + 1) as u32,
                end_line: j as u32,
                metadata: Some(meta),
            });
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

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
    /// Pass-1 namespace/class query for TypeScript.
    ts_ns: Option<Query>,
    /// Pass-1 class query for JavaScript.
    js_ns: Option<Query>,
}

// SAFETY: tree_sitter::Query is Send + Sync (immutable compiled pattern data).
unsafe impl Send for CompiledQueries {}
unsafe impl Sync for CompiledQueries {}

/// C symbol/call query. Named const so tests can prove it compiles against
/// the pinned tree-sitter-c grammar — `Query::new(...).ok()` otherwise
/// swallows pattern errors into `None` and silently disables ALL C
/// extraction (which is exactly what happened to the C++ query).
///
/// Declarator nesting note: in tree-sitter C/C++, `int *f()` parses as
/// pointer_declarator WRAPPING function_declarator, not the other way round.
const C_QUERY_SRC: &str = r#"
        (function_definition declarator: (function_declarator declarator: (identifier) @name)) @func
        (function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))) @func
        (function_definition declarator: (function_declarator declarator: (parenthesized_declarator (pointer_declarator declarator: (identifier) @name)))) @func
        (struct_specifier name: (type_identifier) @name) @struct
        (call_expression function: (identifier) @call.name)
        "#;

/// C++ symbol/call query. Every pattern is validated against
/// tree-sitter-cpp 0.23 node-types: `reference_declarator` has NO
/// `declarator:` field (children only), and `template_method`'s `name`
/// field admits field_identifier/operator_name but never `identifier` —
/// the two mistakes that made the previous query fail to compile, turning
/// C++ extraction off entirely.
const CPP_QUERY_SRC: &str = r#"
        (function_definition declarator: (function_declarator declarator: (identifier) @name)) @func
        (function_definition declarator: (function_declarator declarator: (type_identifier) @name)) @func
        (function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @func
        (function_definition declarator: (function_declarator declarator: (operator_name) @name)) @func
        (function_definition declarator: (function_declarator declarator: (destructor_name (identifier) @name))) @func
        (function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))) @func
        (function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (field_identifier) @name))) @func
        (function_definition declarator: (reference_declarator (function_declarator declarator: (identifier) @name))) @func
        (function_definition declarator: (reference_declarator (function_declarator declarator: (field_identifier) @name))) @func
        (function_definition declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @name))) @func
        (function_definition declarator: (function_declarator declarator: (qualified_identifier name: (destructor_name (identifier) @name)))) @func
        (function_definition declarator: (function_declarator declarator: (qualified_identifier name: (operator_name) @name))) @func
        (function_definition declarator: (function_declarator declarator: (qualified_identifier name: (template_method name: (field_identifier) @name)))) @func
        (function_definition declarator: (function_declarator declarator: (qualified_identifier name: (qualified_identifier name: (identifier) @name)))) @func
        (function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @name)))) @func
        (function_definition declarator: (reference_declarator (function_declarator declarator: (qualified_identifier name: (identifier) @name)))) @func
        (class_specifier name: (type_identifier) @name) @class
        (struct_specifier name: (type_identifier) @name) @struct
        (call_expression function: (identifier) @call.name)
        (call_expression function: (field_expression field: (field_identifier) @call.name))
        (call_expression function: (qualified_identifier name: (identifier) @call.name))
        (call_expression function: (qualified_identifier name: (field_identifier) @call.name))
        "#;

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
        (function_signature name: (identifier) @name) @func
        (method_definition name: (property_identifier) @name) @func
        (method_signature name: (property_identifier) @name) @func
        (variable_declarator
            name: (identifier) @name
            value: (arrow_function)) @func
        (variable_declarator
            name: (identifier) @name
            value: (function_expression)) @func
        (public_field_definition
            name: (property_identifier) @name
            value: (arrow_function)) @func
        (class_declaration name: (type_identifier) @name) @class
        (abstract_class_declaration name: (type_identifier) @name) @class
        (interface_declaration name: (type_identifier) @name) @class
        (enum_declaration name: (identifier) @name) @class
        (type_alias_declaration name: (type_identifier) @name) @class
        (internal_module name: (identifier) @name) @class
        (call_expression function: (identifier) @call.name)
        (call_expression function: (member_expression property: (property_identifier) @call.name))
        "#,
    )
    .ok();

    let js = Query::new(
        &js_lang,
        r#"
        (function_declaration name: (identifier) @name) @func
        (method_definition name: (property_identifier) @name) @func
        (variable_declarator
            name: (identifier) @name
            value: (arrow_function)) @func
        (variable_declarator
            name: (identifier) @name
            value: (function_expression)) @func
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

    let c = Query::new(&c_lang, C_QUERY_SRC)
        .map_err(|e| {
            tracing::error!(error = %e, "failed to compile C Tree-sitter query; C extraction DISABLED");
            e
        })
        .ok();

    let cpp = Query::new(&cpp_lang, CPP_QUERY_SRC)
        .map_err(|e| {
            tracing::error!(error = %e, "failed to compile C++ Tree-sitter query; C++ extraction DISABLED");
            e
        })
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

    let ts_ns = Query::new(
        &ts_lang,
        r#"
        (internal_module name: (identifier) @ns)
        (class_declaration name: (type_identifier) @class)
        (abstract_class_declaration name: (type_identifier) @class)
        (interface_declaration name: (type_identifier) @class)
        (enum_declaration name: (identifier) @class)
        (type_alias_declaration name: (type_identifier) @class)
        (function_declaration name: (identifier) @method)
        (function_signature name: (identifier) @method)
        (method_definition name: (property_identifier) @method)
        (method_signature name: (property_identifier) @method)
        (variable_declarator
            name: (identifier) @method
            value: (arrow_function))
        (variable_declarator
            name: (identifier) @method
            value: (function_expression))
        (public_field_definition
            name: (property_identifier) @method
            value: (arrow_function))
        "#,
    )
    .ok();

    let js_ns = Query::new(
        &js_lang,
        r#"
        (class_declaration name: (identifier) @class)
        (function_declaration name: (identifier) @method)
        (method_definition name: (property_identifier) @method)
        (variable_declarator
            name: (identifier) @method
            value: (arrow_function))
        (variable_declarator
            name: (identifier) @method
            value: (function_expression))
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
        ts_ns,
        js_ns,
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
            "ts" | "tsx" => &QUERIES.ts_ns,
            "js" | "jsx" => &QUERIES.js_ns,
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
            "js" | "jsx" => (tree_sitter_javascript::LANGUAGE.into(), QUERIES.js.as_ref()),
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

                // TODO-13: parameter count for arity-aware call resolution.
                // Grammars expose the list under `parameters` (C#, Rust,
                // Python, ...) or `parameter_list`; count named children,
                // skipping comments.
                if kind == "function"
                    && let Some(params) = node
                        .child_by_field_name("parameters")
                        .or_else(|| node.child_by_field_name("parameter_list"))
                {
                    let mut cursor = params.walk();
                    let count = params
                        .named_children(&mut cursor)
                        .filter(|c| !c.kind().contains("comment"))
                        .count();
                    meta.insert("arity".into(), count.to_string());
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
    use tree_sitter::{Query, QueryCursor};

    /// Compile-time proof that `CompiledQueries` is Send + Sync.
    #[test]
    fn compiled_queries_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<CompiledQueries>();
        assert_sync::<CompiledQueries>();
    }

    #[test]
    fn csharp_methods_record_arity() {
        let code = r#"
namespace App {
    public class Svc {
        public void Save() {}
        public void Save(int id, string name) {}
        public int Sum(int a, int b, int c) { return a + b + c; }
    }
}"#;
        let extractor = SymbolExtractor::new();
        let (symbols, _) = extractor.extract(std::path::Path::new("svc.cs"), code);
        let arities: Vec<String> = symbols
            .iter()
            .filter(|s| s.kind == "function")
            .filter_map(|s| {
                s.metadata
                    .as_ref()
                    .and_then(|m| m.get("arity"))
                    .cloned()
                    .map(|a| format!("{}:{}", s.name, a))
            })
            .collect();
        assert!(arities.contains(&"Save:0".to_string()), "got {arities:?}");
        assert!(arities.contains(&"Save:2".to_string()), "got {arities:?}");
        assert!(arities.contains(&"Sum:3".to_string()), "got {arities:?}");
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

    fn count_tag_captures(
        language: &tree_sitter::Language,
        query_src: &str,
        code: &str,
        tags: &[&str],
    ) -> std::collections::HashMap<String, usize> {
        let query = Query::new(language, query_src).expect("query should compile");
        let mut parser = Parser::new();
        parser
            .set_language(language)
            .expect("language should be set");
        let tree = parser.parse(code, None).expect("code should parse");

        let mut counts: std::collections::HashMap<String, usize> =
            tags.iter().map(|t| ((*t).to_string(), 0usize)).collect();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), code.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let tag = query.capture_names()[cap.index as usize];
                if counts.contains_key(tag) {
                    *counts.get_mut(tag).expect("tag key should exist") += 1;
                }
            }
        }
        counts
    }

    #[test]
    fn ts_query_captures_modern_declarations() {
        let snippet1 = r#"
class Foo {
    bar() { return 1; }
    async baz(): Promise<number> { return 2; }
    private qux = (x: number) => x + 1;
}
"#;
        let snippet2 = r#"
const greet = (name: string) => `Hello ${name}`;
const oldStyle = function(x: number) { return x * 2; };
"#;
        let snippet3 = r#"
interface IFoo { bar(): void; }
enum Color { Red, Green }
type Handler = (e: Event) => void;
namespace Q { export function init(): void {} }
"#;

        let ts_lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();

        let ts_query = r#"
        (function_declaration name: (identifier) @name) @func
        (function_signature name: (identifier) @name) @func
        (method_definition name: (property_identifier) @name) @func
        (method_signature name: (property_identifier) @name) @func
        (variable_declarator
            name: (identifier) @name
            value: (arrow_function)) @func
        (variable_declarator
            name: (identifier) @name
            value: (function_expression)) @func
        (public_field_definition
            name: (property_identifier) @name
            value: (arrow_function)) @func
        (class_declaration name: (type_identifier) @name) @class
        (abstract_class_declaration name: (type_identifier) @name) @class
        (interface_declaration name: (type_identifier) @name) @class
        (enum_declaration name: (identifier) @name) @class
        (type_alias_declaration name: (type_identifier) @name) @class
        (internal_module name: (identifier) @name) @class
        "#;

        let counts1 = count_tag_captures(&ts_lang, ts_query, snippet1, &["func", "class"]);
        assert_eq!(*counts1.get("class").unwrap(), 1);
        assert_eq!(*counts1.get("func").unwrap(), 3);

        let counts2 = count_tag_captures(&ts_lang, ts_query, snippet2, &["func"]);
        assert_eq!(*counts2.get("func").unwrap(), 2);

        let counts3 = count_tag_captures(&ts_lang, ts_query, snippet3, &["func", "class"]);
        assert_eq!(*counts3.get("class").unwrap(), 4);
        assert_eq!(*counts3.get("func").unwrap(), 2);
    }

    #[test]
    fn ts_and_js_methods_receive_class_fqn() {
        let extractor = SymbolExtractor::new();

        let ts_code = r#"
class Foo {
    bar() {}
}
"#;
        let (ts_symbols, _) = extractor.extract(Path::new("sample.ts"), ts_code);
        let ts_method = ts_symbols
            .iter()
            .find(|s| s.kind == "function" && s.name == "bar")
            .expect("TS method should be extracted");
        assert_eq!(
            ts_method
                .metadata
                .as_ref()
                .and_then(|m| m.get("fqn"))
                .map(String::as_str),
            Some("Foo.bar")
        );

        let js_code = r#"
class Baz {
    qux() {}
}
"#;
        let (js_symbols, _) = extractor.extract(Path::new("sample.js"), js_code);
        let js_method = js_symbols
            .iter()
            .find(|s| s.kind == "function" && s.name == "qux")
            .expect("JS method should be extracted");
        assert_eq!(
            js_method
                .metadata
                .as_ref()
                .and_then(|m| m.get("fqn"))
                .map(String::as_str),
            Some("Baz.qux")
        );
    }
}

#[cfg(test)]
mod sync_contract_tests {
    use super::detect_sync_contracts;

    #[test]
    fn detects_vb_three_place_contract() {
        // The live MarkerImport.vb shape, verbatim structure.
        let text = "\
Some code
    ' NOTE! If the logic for checking vital data on marker change we must update it in 3 places:
    ' 1. _io.import.MarkerImport.GetMarkersToDeleteFromProject()
    ' 2. _io.installationsobjekt.DeleteImportedMapMarker()
    ' 3. _integration.gis.vetrofibermap.Feature.EventProcessing.CheckForVitalDataOnMarker()
    Dim x = 1
";
        let c = detect_sync_contracts(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].kind, "sync_contract");
        assert_eq!(c[0].start_line, 2);
        let meta = c[0].metadata.as_ref().unwrap();
        assert_eq!(meta.get("declared_places").unwrap(), "3");
        let sites: Vec<&str> = meta.get("sites").unwrap().split("||").collect();
        assert_eq!(sites.len(), 3);
        assert!(sites[0].contains("GetMarkersToDeleteFromProject"));
        assert!(sites[2].contains("CheckForVitalDataOnMarker"));
    }

    #[test]
    fn detects_c_style_and_ignores_single_site_lists() {
        let text = "\
// keep this validation in sync in 2 places:
// 1. src/api/validate.ts
// 2. src/worker/validate.ts
code();
// must update in 4 places:
// 1. only-one-site-listed
code();
";
        let c = detect_sync_contracts(text);
        assert_eq!(c.len(), 1, "single-site enumerations are not contracts");
        let sites: Vec<&str> = c[0]
            .metadata
            .as_ref()
            .unwrap()
            .get("sites")
            .unwrap()
            .split("||")
            .collect();
        assert_eq!(sites, vec!["src/api/validate.ts", "src/worker/validate.ts"]);
    }

    #[test]
    fn ignores_prose_without_enumeration_and_code_lines() {
        let text = "\
' we must update the docs in 3 places eventually
Dim a = 1
Dim b = 2
";
        assert!(detect_sync_contracts(text).is_empty());
        // trigger phrase in CODE (not a comment) must not fire
        let code = "var msg = \"must update in 3 places\";\nvar x = 1;\nvar y = 2;\n";
        assert!(detect_sync_contracts(code).is_empty());
    }
}

#[cfg(test)]
mod query_compile_tests {
    use super::*;

    /// The C and C++ query consts must compile against the pinned grammars.
    /// Failure here = the corresponding language's extraction is silently
    /// disabled in production (Query::new(...).ok() → None → zero symbols).
    #[test]
    fn c_query_compiles() {
        let lang = tree_sitter_c::LANGUAGE.into();
        if let Err(e) = Query::new(&lang, C_QUERY_SRC) {
            panic!("C query failed to compile — C extraction would be DISABLED: {e}");
        }
    }

    #[test]
    fn cpp_query_compiles() {
        let lang = tree_sitter_cpp::LANGUAGE.into();
        if let Err(e) = Query::new(&lang, CPP_QUERY_SRC) {
            panic!("C++ query failed to compile — C++ extraction would be DISABLED: {e}");
        }
    }

    /// Every language's compiled query must be present. Catches any future
    /// grammar bump or query edit that silently kills extraction for a
    /// language via the .ok() swallow.
    #[test]
    fn all_compiled_queries_are_present() {
        let q = &*QUERIES;
        assert!(q.rust.is_some(), "rust query failed to compile");
        assert!(q.python.is_some(), "python query failed to compile");
        assert!(q.go.is_some(), "go query failed to compile");
        assert!(q.java.is_some(), "java query failed to compile");
        assert!(q.ts.is_some(), "typescript query failed to compile");
        assert!(q.js.is_some(), "javascript query failed to compile");
        assert!(q.cs.is_some(), "c# query failed to compile");
        assert!(q.c.is_some(), "c query failed to compile");
        assert!(q.cpp.is_some(), "c++ query failed to compile");
        assert!(q.cs_ns.is_some(), "c# namespace query failed to compile");
        assert!(
            q.java_ns.is_some(),
            "java namespace query failed to compile"
        );
        assert!(q.go_ns.is_some(), "go namespace query failed to compile");
        assert!(q.rust_mod.is_some(), "rust mod query failed to compile");
        assert!(q.ts_ns.is_some(), "typescript ns query failed to compile");
        assert!(q.js_ns.is_some(), "javascript ns query failed to compile");
    }
}
