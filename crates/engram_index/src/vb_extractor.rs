/// VB.NET symbol + call-graph extractor.
///
/// Covers P0.2 – P0.6:
///   P0.2  Real tree-sitter query loaded from `queries/vb.scm` (embedded at
///         compile time); regex fallback when the query fails to compile.
///   P0.3  Symbol extraction with correct FQN: `Namespace.Class.Method`.
///         `metadata["fqn"]` set on every symbol.
///   P0.4  SQL extraction — detects `New SqlCommand("…")`,
///         `cmd.CommandText = "…"`, and `EXEC`/`EXECUTE` patterns;
///         classifies stored-proc vs inline. Also emits `sql_exec` edges
///         for `ExecuteReader`/`ExecuteNonQuery`/`ExecuteScalar` calls.
///   P0.5  Call extraction — `invocation` targets and `Call` statement.
///   P0.6  VB `Handles` clause detection — regex over code-behind source,
///         emitting `event_wiring` edges linking control → handler method.
///         Supports `Me.Event`, `MyBase.Event`, and multi-event clauses.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

/// The VB.NET query source, embedded from `queries/vb.scm` at compile time.
const VB_QUERY_SRC: &str = include_str!("../queries/vb.scm");

/// Maximum source size we'll attempt tree-sitter parsing on (10 MiB).
const MAX_TREE_SITTER_SOURCE_BYTES: usize = 10 * 1024 * 1024;

/// Maximum source size we'll allow for whole-buffer regex fallback.
///
/// Regex fallback runs multiple multi-line patterns over the full source. Even with
/// Rust's linear-time regex engine, this can create severe CPU pressure on very
/// large files. Above this limit we fail closed (skip extraction) to keep indexing
/// latency and memory bounded.
const MAX_REGEX_FALLBACK_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// Maximum SQL snippet length stored in metadata.
const SQL_SNIPPET_MAX_LEN: usize = 200;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

static HANDLES_SUB_RE: OnceLock<Regex> = OnceLock::new();
static HANDLES_PAIR_RE: OnceLock<Regex> = OnceLock::new();
static SQL_CMD_RE: OnceLock<Regex> = OnceLock::new();
static SQL_TEXT_RE: OnceLock<Regex> = OnceLock::new();
static SQL_EXEC_RE: OnceLock<Regex> = OnceLock::new();
static SQL_EXEC_CALL_RE: OnceLock<Regex> = OnceLock::new();
static SQL_ADAPTER_RE: OnceLock<Regex> = OnceLock::new();
static SQL_PROC_TYPE_RE: OnceLock<Regex> = OnceLock::new();
static ADDHANDLER_RE: OnceLock<Regex> = OnceLock::new();
static REGEX_NS_RE: OnceLock<Regex> = OnceLock::new();
static REGEX_TYPE_RE: OnceLock<Regex> = OnceLock::new();
static REGEX_MEMBER_RE: OnceLock<Regex> = OnceLock::new();

// ADO.NET column access patterns
static ADO_ROW_RE: OnceLock<Regex> = OnceLock::new();
static ADO_ITEM_RE: OnceLock<Regex> = OnceLock::new();
static ADO_ORDINAL_RE: OnceLock<Regex> = OnceLock::new();

// Side-effect slicing regex for UI mutation detection
static UI_MUTATION_RE: OnceLock<Regex> = OnceLock::new();
// Field-based UI mutation detection (matches identifier.Property = ...)
static FIELD_ASSIGN_RE: OnceLock<Regex> = OnceLock::new();
// Tree-sitter enhanced CommandText assignment detection
static CMD_TEXT_ASSIGN_RE: OnceLock<Regex> = OnceLock::new();

// Server-to-client script injection patterns
static REGISTER_STARTUP_SCRIPT_RE: OnceLock<Regex> = OnceLock::new();
static REGISTER_CLIENT_SCRIPT_RE: OnceLock<Regex> = OnceLock::new();
static SCRIPT_MANAGER_RE: OnceLock<Regex> = OnceLock::new();
// Extracts function names from injected JS strings
static JS_FUNCTION_IN_SCRIPT_RE: OnceLock<Regex> = OnceLock::new();

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

// ── FQN lookup tables ───────────────────────────────────────────────────────

/// Two-tier FQN lookup: exact (case-sensitive) + lowercased (case-insensitive).
/// Both lookups are O(1).
pub struct FqnMaps {
    /// `start_byte → FQN` for precise definition tagging.
    pub by_node: HashMap<usize, String>,
    /// `short_name → FQN` for call resolution (case-sensitive).
    pub by_name: HashMap<String, String>,
    /// `short_name_lowered → FQN` for VB case-insensitive call resolution.
    pub by_name_ci: HashMap<String, String>,
    /// Lowercased field names from the current file, used for UI mutation detection.
    /// Includes both regular fields and control_ref fields from designer files.
    pub field_names: HashSet<String>,
}

impl FqnMaps {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            by_node: HashMap::with_capacity(cap),
            by_name: HashMap::with_capacity(cap),
            by_name_ci: HashMap::with_capacity(cap),
            field_names: HashSet::with_capacity(cap / 4),
        }
    }

    /// Insert a name → FQN mapping into both exact and CI maps.
    pub fn insert_name(&mut self, name: &str, fqn: String) {
        self.by_name_ci.insert(name.to_lowercase(), fqn.clone());
        self.by_name.insert(name.to_string(), fqn);
    }

    /// Insert a node start_byte → FQN mapping.
    pub fn insert_node(&mut self, start_byte: usize, fqn: String) {
        self.by_node.insert(start_byte, fqn);
    }

    /// Resolve a short call name to its FQN. O(1) for both paths.
    pub fn resolve(&self, short: &str) -> String {
        if let Some(fqn) = self.by_name.get(short) {
            return fqn.clone();
        }
        if let Some(fqn) = self.by_name_ci.get(&short.to_lowercase()) {
            return fqn.clone();
        }
        short.to_string()
    }
}

// ── Scope Stack ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ScopeEntry {
    start: usize,
    end: usize,
    _name: String,
    fqn: String,
    kind: &'static str,
    line: u32,
}

/// Remove scope entries whose end byte is at or before `pos`.
#[inline]
fn prune_scope_stack(stack: &mut Vec<ScopeEntry>, pos: usize) {
    while stack.last().is_some_and(|e| e.end <= pos) {
        stack.pop();
    }
}

/// Walk the scope stack backwards to find the innermost *enclosing* scope.
fn find_enclosing_scope(stack: &[ScopeEntry], pos: usize) -> (&str, &'static str, u32) {
    for entry in stack.iter().rev() {
        if pos >= entry.start && pos < entry.end {
            return (&entry.fqn, entry.kind, entry.line);
        }
    }
    ("file", "file", 0)
}

/// Find the best (innermost) enclosing scope from a flat list of all scopes.
fn find_best_enclosing_scope(scopes: &[ScopeEntry], pos: usize) -> (&str, &'static str, u32) {
    let mut best: Option<&ScopeEntry> = None;
    for s in scopes {
        if pos >= s.start && pos < s.end {
            if let Some(current_best) = best {
                // We want the innermost one (smallest range)
                if (s.end - s.start) < (current_best.end - current_best.start) {
                    best = Some(s);
                }
            } else {
                best = Some(s);
            }
        }
    }
    if let Some(s) = best {
        (&s.fqn, s.kind, s.line)
    } else {
        ("file", "file", 0)
    }
}

// ── WebForms Lifecycle Metadata ────────────────────────────────────────────

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

// ── Side-Effect Classification ─────────────────────────────────────────────

/// Classify side effects in a method body for codebehind refactoring.
///
/// Returns a comma-separated string of side-effect categories:
///   - `UI_Mutation`: assigns to control properties (e.g., `lblStatus.Text = "Saved"`)
///   - `DB_Access`: contains SQL/ADO.NET patterns
///   - `State_Access`: reads/writes Session/ViewState/Application/Cache
///
/// `known_fields` contains lowercased field names from the current file's
/// tree-sitter pass. This enables UI mutation detection for controls that
/// don't follow Hungarian notation (e.g., `searchBox` instead of `txtSearch`).
fn classify_side_effects(method_body: &str, known_fields: &HashSet<String>) -> Option<String> {
    let mut effects = Vec::new();
    let src = method_body.as_bytes();

    // UI_Mutation: control property assignments.
    // Heuristic 1: identifier with WebForms control prefix followed by .Property =
    // Common prefixes: btn, lbl, txt, ddl, grd, pnl, chk, rbl, rep, lst, img, hdn, lit, phd
    let ui_re = get_compiled_regex(
        &UI_MUTATION_RE,
        r"(?i)\b(?:btn|lbl|txt|ddl|grd|pnl|chk|rbl|rep|lst|img|hdn|lit|phd|rpt|fv|gv|dv|lv)[A-Za-z0-9_]+\.\w+\s*=",
        "ui_mutation",
    );
    if let Some(re) = ui_re {
        if re.is_match(method_body) {
            effects.push("UI_Mutation");
        }
    }

    // Heuristic 2: cross-reference assignments against known class fields.
    // Any `fieldName.Property = value` where fieldName is a known field in the
    // class is potentially a UI mutation (controls are declared as fields in
    // designer partial classes or WithEvents declarations).
    if !effects.contains(&"UI_Mutation") && !known_fields.is_empty() {
        let field_re = get_compiled_regex(
            &FIELD_ASSIGN_RE,
            r"(?i)\b([A-Za-z_][A-Za-z0-9_]*)\.\w+\s*=",
            "field_assign",
        );
        if let Some(re) = field_re {
            for cap in re.captures_iter(method_body) {
                let ident = &cap[1];
                // Skip common non-control identifiers: Me, MyBase, Response, Request, etc.
                let lower = ident.to_lowercase();
                if matches!(
                    lower.as_str(),
                    "me" | "mybase"
                        | "myclass"
                        | "response"
                        | "request"
                        | "server"
                        | "session"
                        | "application"
                        | "cache"
                        | "viewstate"
                        | "page"
                        | "context"
                ) {
                    continue;
                }
                if known_fields.contains(&lower) {
                    effects.push("UI_Mutation");
                    break;
                }
            }
        }
    }

    // DB_Access: reuse existing has_sql_keyword() check.
    if has_sql_keyword(method_body) {
        effects.push("DB_Access");
    }

    // State_Access: fast byte check for state store keywords.
    if ci_contains_fast(src, b"Session(")
        || ci_contains_fast(src, b"Session[")
        || ci_contains_fast(src, b"ViewState(")
        || ci_contains_fast(src, b"ViewState[")
        || ci_contains_fast(src, b"Application(")
        || ci_contains_fast(src, b"Application[")
        || ci_contains_fast(src, b"Cache(")
        || ci_contains_fast(src, b"Cache[")
    {
        effects.push("State_Access");
    }

    if effects.is_empty() {
        None
    } else {
        Some(effects.join(","))
    }
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Extract symbols and call-graph edges from a `.vb` source file.
///
/// Returns `(symbols, edges)`.
pub fn extract_vb(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    // Guard: empty or whitespace-only source
    if source.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Guard: extremely large files skip tree-sitter to avoid OOM
    if source.len() > MAX_TREE_SITTER_SOURCE_BYTES {
        tracing::warn!(
            "source file {} exceeds {} bytes, using regex fallback",
            path.display(),
            MAX_TREE_SITTER_SOURCE_BYTES,
        );
        return regex_extract(path, source);
    }

    let lang: tree_sitter::Language = arborium_vb::language().into();

    // 1. Compile Query (fail fast to regex if query is bad)
    let query = match Query::new(&lang, VB_QUERY_SRC) {
        Ok(q) => q,
        Err(e) => {
            if cfg!(test) && std::env::var("ENGRAM_REQUIRE_VB_TREESITTER").is_ok() {
                tracing::error!("ENGRAM_REQUIRE_VB_TREESITTER=1 but VB query compile failed: {e}");
            }
            tracing::warn!("vb.scm query compile failed: {e}");
            return regex_extract(path, source);
        }
    };

    // 2. Parse Tree (fail fast to regex if parse fails)
    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        if cfg!(test) && std::env::var("ENGRAM_REQUIRE_VB_TREESITTER").is_ok() {
            tracing::error!("ENGRAM_REQUIRE_VB_TREESITTER=1 but failed to set VB language");
        }
        tracing::warn!("tree-sitter: failed to set VB language");
        return regex_extract(path, source);
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => {
            if cfg!(test) && std::env::var("ENGRAM_REQUIRE_VB_TREESITTER").is_ok() {
                tracing::error!(
                    "ENGRAM_REQUIRE_VB_TREESITTER=1 but tree-sitter VB parse returned None"
                );
            }
            tracing::warn!("tree-sitter VB parse returned None, using regex fallback");
            return regex_extract(path, source);
        }
    };

    // ── Pass 1: Build FQN tables ──────────────────────────────────────────
    let fqn_maps = build_fqn_tables(&query, &tree, source);

    // ── Pass 2: Emit symbols + edges ──────────────────────────────────────
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    // All scopes encountered, used for post-processing SQL attribution
    let mut all_scopes: Vec<ScopeEntry> = Vec::new();

    // Scope stack borrows name/kind slices from `source` (zero-copy).
    let mut scope_stack: Vec<ScopeEntry> = Vec::new();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

    while let Some(m) = matches.next() {
        let mut node_main = None;
        let mut call_name_node: Option<tree_sitter::Node> = None;
        let mut import_node: Option<tree_sitter::Node> = None;
        let mut tag_main: &str = "";

        for cap in m.captures {
            let tag = query.capture_names()[cap.index as usize];
            match tag {
                "func" | "class" | "property" | "event" | "field" => {
                    if node_main.is_none() {
                        node_main = Some(cap.node);
                        tag_main = tag;
                    }
                }
                "call.name" => call_name_node = Some(cap.node),
                "import" => import_node = Some(cap.node),
                _ => {}
            }
        }

        // ── Case A: Imports ──────────────────────────────────────────────
        if let Some(n) = import_node {
            let ns_text = node_text(source, &n);
            if !ns_text.is_empty() {
                edges.push(ExtractedEdge {
                    source_name: "file".into(),
                    source_kind: "file",
                    source_start_line: 0,
                    source_language: "vb",
                    target_name: ns_text.to_string(),
                    target_kind: None,
                    target_start_line: None,
                    kind: "imports",
                    metadata: None,
                });
            }
        }

        // ── Case B: Calls ────────────────────────────────────────────────
        if let Some(n) = call_name_node {
            let callee_raw = node_text(source, &n);
            if !callee_raw.is_empty() {
                let mut callee_fqn = fqn_maps.resolve(callee_raw);

                // Handle member form: resolve dotted call targets.
                // Repo.Load       → resolve "Repo" → "App.Repo", produce "App.Repo.Load"
                // NS.Class.Method → try progressively longer prefixes
                if callee_fqn == callee_raw && callee_raw.contains('.') {
                    let parts: Vec<&str> = callee_raw.split('.').collect();
                    let mut resolved = false;

                    // Try resolving progressively longer prefixes:
                    // For A.B.C: try "A.B" then "A"
                    for split_at in (1..parts.len()).rev() {
                        let prefix = parts[..split_at].join(".");
                        let suffix = parts[split_at..].join(".");
                        let resolved_prefix = fqn_maps.resolve(&prefix);
                        if resolved_prefix != prefix {
                            callee_fqn = format!("{}.{}", resolved_prefix, suffix);
                            resolved = true;
                            break;
                        }
                    }

                    // Fallback: try resolving just the last part (member name)
                    if !resolved {
                        if let Some(&member) = parts.last() {
                            let resolved_member = fqn_maps.resolve(member);
                            if resolved_member != member {
                                callee_fqn = resolved_member;
                            }
                        }
                    }
                }

                let (src_name, src_kind, src_line) =
                    find_enclosing_scope(&scope_stack, n.start_byte());

                let mut meta = std::collections::HashMap::new();
                let (target_name, target_kind) =
                    if callee_fqn == callee_raw && !callee_raw.contains('.') {
                        meta.insert("unresolved".into(), "true".into());
                        (callee_raw.to_string(), None)
                    } else {
                        (callee_fqn, Some("function"))
                    };

                edges.push(ExtractedEdge {
                    source_name: src_name.to_string(),
                    source_kind: src_kind,
                    source_start_line: src_line,
                    source_language: "vb",
                    target_name,
                    target_kind,
                    target_start_line: None,
                    kind: "calls",
                    metadata: if meta.is_empty() { None } else { Some(meta) },
                });
            }
        }

        // ── Case C: Definitions (Class/Method/Property) ─────────────────
        if let Some(main_node) = node_main {
            // Find the name sibling/child in THIS match
            let mut name = String::new();
            for sibling_cap in m.captures {
                if query.capture_names()[sibling_cap.index as usize] == "name" {
                    name = node_text(source, &sibling_cap.node).to_string();
                    break;
                }
            }

            // Constructors have no @name child — synthesize ".ctor"
            if name.is_empty() && tag_main == "func" {
                if main_node.kind() == "constructor_declaration" {
                    name = ".ctor".to_string();
                }
            }

            if !name.is_empty() {
                let is_designer = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.contains(".designer."))
                    .unwrap_or(false);

                let kind = match tag_main {
                    "class" => "class",
                    "property" => "property",
                    "event" => "event",
                    "field" => {
                        if is_designer {
                            "control_ref"
                        } else {
                            "field"
                        }
                    }
                    _ => "function",
                };
                let actual_main_node = if kind == "class" || kind == "function" {
                    // For blocks, the declaration node is the parent of the identifier
                    main_node.parent().unwrap_or(main_node)
                } else {
                    // For declarations (property/event/field), the declaration node is also parent
                    main_node.parent().unwrap_or(main_node)
                };

                let start_line = (actual_main_node.start_position().row + 1) as u32;
                let end_line = (actual_main_node.end_position().row + 1) as u32;

                let fqn = fqn_maps
                    .by_node
                    .get(&main_node.start_byte())
                    .cloned()
                    .unwrap_or_else(|| name.to_string());

                let mut meta = HashMap::from([("fqn".into(), fqn.clone())]);
                if is_designer && kind == "control_ref" {
                    meta.insert("is_designer".into(), "true".into());
                }

                // Tag WebForms lifecycle methods with stage + sequence metadata.
                if kind == "function" {
                    if let Some((stage, seq)) = webforms_lifecycle_info(&name) {
                        meta.insert("lifecycle_stage".into(), stage.into());
                        meta.insert("lifecycle_sequence".into(), seq.to_string());
                    }
                }

                // Side-effect classification for codebehind methods.
                if kind == "function" {
                    if let Some(body) =
                        source.get(actual_main_node.start_byte()..actual_main_node.end_byte())
                    {
                        if let Some(effects) = classify_side_effects(body, &fqn_maps.field_names) {
                            meta.insert("side_effects".into(), effects);
                        }
                    }
                }

                symbols.push(ExtractedSymbol {
                    name: name.to_string(),
                    kind,
                    start_line,
                    end_line,
                    metadata: Some(meta),
                });

                // Prune closed scopes, then emit containment edge
                prune_scope_stack(&mut scope_stack, actual_main_node.start_byte());

                if let Some(parent) = scope_stack.last() {
                    edges.push(ExtractedEdge {
                        source_name: parent.fqn.clone(),
                        source_kind: parent.kind,
                        source_start_line: parent.line,
                        source_language: "vb",
                        target_name: fqn.clone(),
                        target_kind: Some(kind),
                        target_start_line: Some(start_line),
                        kind: "contains",
                        metadata: None,
                    });
                }

                let entry = ScopeEntry {
                    start: actual_main_node.start_byte(),
                    end: actual_main_node.end_byte(),
                    _name: name,
                    fqn,
                    kind,
                    line: start_line,
                };
                all_scopes.push(entry.clone());
                scope_stack.push(entry);
            }
        }
    }

    // ── Post-Processing (Regex augmentations) ───────────────────────────
    // Short-circuit: skip regex passes when the relevant keywords are absent.
    if has_handles_keyword(source) {
        edges.extend(extract_handles(&fqn_maps, source));
    }
    // AddHandler wiring (runtime event binding, common in dynamically-created controls)
    if ci_contains_fast(source.as_bytes(), b"AddHandler") {
        edges.extend(extract_addhandler(&fqn_maps, source));
    }
    // Tree-sitter enhanced SQL extraction: captures full concatenated
    // CommandText assignments (e.g., "SELECT ... " & variable & " ...").
    let ts_sql_results = extract_ts_command_text(&tree, source);
    let ts_cmd_positions: Vec<usize> = ts_sql_results.iter().map(|(_, pos)| *pos).collect();
    for (sql_text, pos) in &ts_sql_results {
        let (src_name, src_kind, src_line) = find_best_enclosing_scope(&all_scopes, *pos);
        let (target_id, target_kind_str) = classify_sql(sql_text);
        let snippet: String = sql_text.chars().take(SQL_SNIPPET_MAX_LEN).collect();
        let meta = HashMap::from([
            ("sql_snippet".into(), snippet),
            ("extraction".into(), "tree_sitter_concat".into()),
        ]);
        edges.push(ExtractedEdge {
            source_name: src_name.to_string(),
            source_kind: src_kind,
            source_start_line: src_line,
            source_language: "vb",
            target_name: target_id,
            target_kind: Some(target_kind_str),
            target_start_line: None,
            kind: "sql_calls",
            metadata: Some(meta),
        });
    }

    // Regex SQL extraction: handles SqlCommand constructors, EXEC, DataAdapter,
    // and simple CommandText assignments not covered by tree-sitter.
    if has_sql_keyword(source) {
        let sql_results = regex_extract_sql(source, &ts_cmd_positions);
        for (mut edge, pos) in sql_results {
            // Attribute to enclosing scope if possible
            let (src_name, src_kind, src_line) = find_best_enclosing_scope(&all_scopes, pos);
            edge.source_name = src_name.to_string();
            edge.source_kind = src_kind;
            edge.source_start_line = src_line;
            edges.push(edge);
        }
    }

    // ADO.NET column access detection (reads_column → binding_field:ColumnName).
    if has_ado_keyword(source) {
        edges.extend(extract_ado_column_access(source, &all_scopes));
    }

    // Server-to-client script injection detection
    if has_script_injection_keyword(source) {
        edges.extend(extract_script_injections(source, &all_scopes));
    }

    (symbols, edges)
}

// ── Fast keyword checks for short-circuiting ────────────────────────────────

/// Check if source contains any Handles-related keywords.
/// Uses a case-insensitive scan since VB.NET is case-insensitive.
#[inline]
fn has_handles_keyword(source: &str) -> bool {
    let src = source.as_bytes();
    let needle = b"handles";
    let needle_len = needle.len();
    if src.len() < needle_len {
        return false;
    }
    for i in 0..=(src.len() - needle_len) {
        if src[i..i + needle_len].eq_ignore_ascii_case(needle) {
            return true;
        }
    }
    false
}

/// Fast case-insensitive substring check for a single needle.
#[inline]
fn ci_contains_fast(haystack: &[u8], needle: &[u8]) -> bool {
    let nlen = needle.len();
    if haystack.len() < nlen {
        return false;
    }
    for i in 0..=(haystack.len() - nlen) {
        if haystack[i..i + nlen].eq_ignore_ascii_case(needle) {
            return true;
        }
    }
    false
}

/// Check if source contains any SQL-related keywords (case-insensitive for VB.NET).
#[inline]
fn has_sql_keyword(source: &str) -> bool {
    fn ci_contains(haystack: &[u8], needle: &[u8]) -> bool {
        let nlen = needle.len();
        if haystack.len() < nlen {
            return false;
        }
        for i in 0..=(haystack.len() - nlen) {
            if haystack[i..i + nlen].eq_ignore_ascii_case(needle) {
                return true;
            }
        }
        false
    }

    let src = source.as_bytes();
    ci_contains(src, b"SqlCommand")
        || ci_contains(src, b"OleDbCommand")
        || ci_contains(src, b"OdbcCommand")
        || ci_contains(src, b"CommandText")
        || ci_contains(src, b"ExecuteReader")
        || ci_contains(src, b"ExecuteNonQuery")
        || ci_contains(src, b"ExecuteScalar")
        || ci_contains(src, b"SqlDataAdapter")
        || ci_contains(src, b"OleDbDataAdapter")
        || ci_contains(src, b"EXEC ")
}

/// Fast check for ADO.NET data access keywords (DataRow, SqlDataReader, etc.).
#[inline]
fn has_ado_keyword(source: &str) -> bool {
    let src = source.as_bytes();
    ci_contains_fast(src, b"GetOrdinal")
        || ci_contains_fast(src, b"DataRow")
        || ci_contains_fast(src, b"DataReader")
        || ci_contains_fast(src, b"DataTable")
        || ci_contains_fast(src, b"SqlDataAdapter")
        || ci_contains_fast(src, b".Item(")
        || ci_contains_fast(src, b".Item[")
        || ci_contains_fast(src, b".Fields(")
        || ci_contains_fast(src, b".Fields[")
        || ci_contains_fast(src, b"row(\"")
        || ci_contains_fast(src, b"dr(\"")
        || ci_contains_fast(src, b"rdr(\"")
}

/// Fast check for script injection keywords.
#[inline]
fn has_script_injection_keyword(source: &str) -> bool {
    let src = source.as_bytes();
    ci_contains_fast(src, b"RegisterStartupScript")
        || ci_contains_fast(src, b"RegisterClientScript")
        || ci_contains_fast(src, b"ScriptManager")
}

/// Detect server-to-client script injection via `ClientScript.RegisterStartupScript`,
/// `ClientScript.RegisterClientScriptBlock`, and `ScriptManager.RegisterStartupScript`.
///
/// Emits `injects_script` edges from the enclosing VB method to the JavaScript
/// function being injected into the page.
fn extract_script_injections(source: &str, all_scopes: &[ScopeEntry]) -> Vec<ExtractedEdge> {
    let mut edges = Vec::new();

    // Pattern 1: ClientScript.RegisterStartupScript / RegisterClientScriptBlock
    // VB: Me.ClientScript.RegisterStartupScript(Me.GetType(), "key", "script", True)
    // C#: ClientScript.RegisterStartupScript(GetType(), "key", "script", true);
    // Also: Page.ClientScript.RegisterStartupScript(...)
    if let Some(re) = get_compiled_regex(
        &REGISTER_STARTUP_SCRIPT_RE,
        r#"(?i)(?:Me\.|Page\.)?ClientScript\.Register(?:Startup|Client)Script(?:Block)?\s*\([^,]+,\s*[^,]+,\s*"(?P<script>[^"]{1,500})""#,
        "register_startup_script",
    ) {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).unwrap();
            let script = cap.name("script").unwrap().as_str();
            let (src_name, src_kind, src_line) = find_best_enclosing_scope(all_scopes, m.start());
            emit_script_injection_edges(
                &mut edges,
                src_name,
                src_kind,
                src_line,
                script,
                "RegisterStartupScript",
            );
        }
    }

    // Pattern 2: ScriptManager.RegisterStartupScript
    // ScriptManager.RegisterStartupScript(Me, Me.GetType(), "key", "script", True)
    if let Some(re) = get_compiled_regex(
        &SCRIPT_MANAGER_RE,
        r#"(?i)ScriptManager\.Register(?:Startup|Client)Script\s*\([^,]+,\s*[^,]+,\s*[^,]+,\s*"(?P<script>[^"]{1,500})""#,
        "script_manager",
    ) {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).unwrap();
            let script = cap.name("script").unwrap().as_str();
            let (src_name, src_kind, src_line) = find_best_enclosing_scope(all_scopes, m.start());
            emit_script_injection_edges(
                &mut edges,
                src_name,
                src_kind,
                src_line,
                script,
                "ScriptManager",
            );
        }
    }

    // Pattern 3: RegisterClientScriptBlock (less common variant)
    if let Some(re) = get_compiled_regex(
        &REGISTER_CLIENT_SCRIPT_RE,
        r#"(?i)RegisterClientScriptBlock\s*\([^,]+,\s*"(?P<script>[^"]{1,500})""#,
        "register_client_script",
    ) {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).unwrap();
            let script = cap.name("script").unwrap().as_str();
            let (src_name, src_kind, src_line) = find_best_enclosing_scope(all_scopes, m.start());
            emit_script_injection_edges(
                &mut edges,
                src_name,
                src_kind,
                src_line,
                script,
                "RegisterClientScriptBlock",
            );
        }
    }

    edges
}

/// Parse a script string for function calls and emit `injects_script` edges.
fn emit_script_injection_edges(
    edges: &mut Vec<ExtractedEdge>,
    src_name: &str,
    src_kind: &'static str,
    src_line: u32,
    script: &str,
    injection_method: &str,
) {
    // Extract function names from the injected script
    let func_re = get_compiled_regex(
        &JS_FUNCTION_IN_SCRIPT_RE,
        r"(?:function\s+)?([A-Za-z_$][A-Za-z0-9_$]*)\s*\(",
        "js_function_in_script",
    );

    let mut found_functions = HashSet::new();

    if let Some(re) = func_re {
        for cap in re.captures_iter(script) {
            let func_name = cap.get(1).unwrap().as_str();
            // Skip common JS keywords and short noise
            if matches!(
                func_name,
                "if" | "for"
                    | "while"
                    | "function"
                    | "return"
                    | "var"
                    | "let"
                    | "const"
                    | "new"
                    | "typeof"
                    | "alert"
                    | "console"
                    | "window"
                    | "document"
            ) || func_name.len() < 2
            {
                continue;
            }
            if !found_functions.insert(func_name.to_string()) {
                continue;
            }

            let snippet: String = script.chars().take(100).collect();
            let mut meta = HashMap::with_capacity(3);
            meta.insert("injection_method".into(), injection_method.into());
            meta.insert("script_snippet".into(), snippet);
            meta.insert("target_function".into(), func_name.into());

            edges.push(ExtractedEdge {
                source_name: src_name.to_string(),
                source_kind: src_kind,
                source_start_line: src_line,
                source_language: "vb",
                target_name: func_name.to_string(),
                target_kind: Some("function"),
                target_start_line: None,
                kind: "injects_script",
                metadata: Some(meta),
            });
        }
    }

    // If no functions found, still emit an edge to the script as a whole
    if found_functions.is_empty() {
        let snippet: String = script.chars().take(100).collect();
        let mut meta = HashMap::with_capacity(2);
        meta.insert("injection_method".into(), injection_method.into());
        meta.insert("script_snippet".into(), snippet.clone());

        edges.push(ExtractedEdge {
            source_name: src_name.to_string(),
            source_kind: src_kind,
            source_start_line: src_line,
            source_language: "vb",
            target_name: format!("inline_script:{}", &snippet[..snippet.len().min(40)]),
            target_kind: Some("function"),
            target_start_line: None,
            kind: "injects_script",
            metadata: Some(meta),
        });
    }
}

/// Extract ADO.NET column access patterns, emitting `reads_column` edges
/// that target `binding_field:ColumnName` virtual nodes.
///
/// Detected patterns:
///   - `row("CustomerName")` / `row["CustomerName"]` — DataRow indexer
///   - `reader("ColumnName")` / `reader["ColumnName"]` — DataReader indexer
///   - `.Item("ColumnName")` / `.Fields("ColumnName")` — explicit member access
///   - `reader.GetString(reader.GetOrdinal("ColumnName"))` — ordinal pattern
fn extract_ado_column_access(source: &str, all_scopes: &[ScopeEntry]) -> Vec<ExtractedEdge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new(); // (scope_fqn, col_name) dedup

    // Build line offset → byte offset mapping for scope attribution.
    let line_offsets: Vec<usize> = std::iter::once(0)
        .chain(source.bytes().enumerate().filter_map(
            |(i, b)| {
                if b == b'\n' { Some(i + 1) } else { None }
            },
        ))
        .collect();

    // Pattern 1: row("Col") / row["Col"] / dr("Col") / reader("Col") etc.
    let row_re = get_compiled_regex(
        &ADO_ROW_RE,
        r#"(?i)\b(?:row|dr|datarow|reader|rdr|sdr)\s*[\(\[]\s*"([^"]+)"\s*[\)\]]"#,
        "ado_row",
    );

    // Pattern 2: .Item("Col") / .Fields("Col") / .Item["Col"]
    let item_re = get_compiled_regex(
        &ADO_ITEM_RE,
        r#"(?i)\.(?:Item|Fields)\s*[\(\[]\s*"([^"]+)"\s*[\)\]]"#,
        "ado_item",
    );

    // Pattern 3: GetOrdinal("Col")
    let ordinal_re = get_compiled_regex(
        &ADO_ORDINAL_RE,
        r#"(?i)GetOrdinal\s*\(\s*"([^"]+)"\s*\)"#,
        "ado_ordinal",
    );

    let patterns: Vec<(&str, Option<&Regex>)> = vec![
        ("ado_row_indexer", row_re),
        ("ado_item_member", item_re),
        ("ado_ordinal", ordinal_re),
    ];

    for (access_pattern, re_opt) in &patterns {
        let Some(re) = re_opt else { continue };
        for m in re.find_iter(source) {
            let byte_pos = m.start();
            let Some(cap) = re.captures(&source[byte_pos..]) else {
                continue;
            };
            let col_name = cap.get(1).map_or("", |c| c.as_str()).to_string();
            if col_name.is_empty() {
                continue;
            }

            let (src_name, src_kind, src_line) = find_best_enclosing_scope(all_scopes, byte_pos);

            // Deduplicate per (scope, column) pair.
            if !seen.insert((src_name.to_string(), col_name.clone())) {
                continue;
            }

            let mut meta = HashMap::new();
            meta.insert("column_name".into(), col_name.clone());
            meta.insert("access_pattern".into(), access_pattern.to_string());

            edges.push(ExtractedEdge {
                source_name: src_name.to_string(),
                source_kind: src_kind,
                source_start_line: src_line,
                source_language: "vb",
                target_name: format!("binding_field:{}", col_name),
                target_kind: Some("binding_field"),
                target_start_line: None,
                kind: "reads_column",
                metadata: Some(meta),
            });
        }
    }

    let _ = line_offsets; // reserved for future line attribution
    edges
}

// ── Pass 1: FQN Tables ──────────────────────────────────────────────────────

/// Builds FQN lookup tables in a single tree traversal.
///
/// Uses stacks for namespace and class context so nested declarations work.
fn build_fqn_tables(query: &Query, tree: &tree_sitter::Tree, source: &str) -> FqnMaps {
    let mut maps = FqnMaps::with_capacity(64);

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());

    let mut namespace_stack: Vec<(String, usize)> = Vec::new(); // (ns, end_byte)
    let mut class_stack: Vec<(String, usize)> = Vec::new(); // (class, end_byte)

    while let Some(m) = matches.next() {
        let Some(first_capture) = m.captures.first() else {
            continue;
        };
        let anchor = first_capture.node.start_byte();

        // Prune closed scopes
        while namespace_stack
            .last()
            .is_some_and(|(_, end)| *end <= anchor)
        {
            namespace_stack.pop();
        }
        while class_stack.last().is_some_and(|(_, end)| *end <= anchor) {
            class_stack.pop();
        }

        for cap in m.captures {
            let tag = query.capture_names()[cap.index as usize];
            let text = node_text(source, &cap.node);
            if text.is_empty() {
                continue;
            }

            match tag {
                "ns" => {
                    let end_byte = cap
                        .node
                        .parent()
                        .map(|p| p.end_byte())
                        .unwrap_or(usize::MAX);
                    namespace_stack.push((text.to_string(), end_byte));
                }
                "name" => {
                    // Check if this same node also has a kind tag in the same match
                    let mut node_kind_tag = "";
                    for other_cap in m.captures {
                        if other_cap.node == cap.node {
                            let other_tag = query.capture_names()[other_cap.index as usize];
                            if matches!(
                                other_tag,
                                "class" | "func" | "property" | "event" | "field"
                            ) {
                                node_kind_tag = other_tag;
                                break;
                            }
                        }
                    }

                    let node_kind = if !node_kind_tag.is_empty() {
                        // Map kind tag to block kind
                        match node_kind_tag {
                            "class" => "class_block",
                            "func" => "method_declaration",
                            "property" => "property_declaration",
                            "event" => "event_declaration",
                            "field" => "field_declaration",
                            _ => "",
                        }
                    } else {
                        cap.node.parent().map(|p| p.kind()).unwrap_or("")
                    };

                    let current_ns = build_dotted_namespace(&namespace_stack);

                    if is_type_block(node_kind) {
                        // For nested types: NS.OuterClass.InnerClass
                        let fqn = if let Some((outer, _)) = class_stack.last() {
                            let parent_fqn = make_fqn(&current_ns, outer, "");
                            make_fqn(&parent_fqn, text, "")
                        } else {
                            make_fqn(&current_ns, text, "")
                        };

                        if let Some(parent) = cap.node.parent() {
                            maps.insert_node(parent.start_byte(), fqn.clone());
                            // IMPORTANT: insert node itself too because extract_vb looks up by node.start_byte()
                            maps.insert_node(cap.node.start_byte(), fqn.clone());
                            class_stack.push((text.to_string(), parent.end_byte()));
                        }
                        maps.insert_name(text, fqn);
                    } else {
                        // Method/Sub/Function/Property/Field
                        let current_class =
                            class_stack.last().map(|(c, _)| c.as_str()).unwrap_or("");
                        let fqn = make_fqn(&current_ns, current_class, text);
                        if let Some(parent) = cap.node.parent() {
                            maps.insert_node(parent.start_byte(), fqn.clone());
                            maps.insert_node(cap.node.start_byte(), fqn.clone());
                        }
                        // Track field names for UI mutation detection (Fix #4).
                        if node_kind_tag == "field" {
                            maps.field_names.insert(text.to_lowercase());
                        }
                        maps.insert_name(text, fqn);
                    }
                }
                _ => {}
            }
        }
    }

    maps
}

/// Build the current namespace string from the stack (supports nesting).
fn build_dotted_namespace(stack: &[(String, usize)]) -> String {
    match stack.len() {
        0 => String::new(),
        1 => stack[0].0.clone(),
        _ => {
            let total: usize =
                stack.iter().map(|(ns, _)| ns.len()).sum::<usize>() + stack.len() - 1;
            let mut result = String::with_capacity(total);
            for (i, (ns, _)) in stack.iter().enumerate() {
                if i > 0 {
                    result.push('.');
                }
                result.push_str(ns);
            }
            result
        }
    }
}

fn is_type_block(kind: &str) -> bool {
    matches!(
        kind,
        "class_block" | "module_block" | "structure_block" | "interface_block" | "enum_block"
    )
}

/// Build a dotted FQN string from components, omitting empty parts.
/// Zero-allocation for single-component FQNs; pre-sized for multi.
fn make_fqn(ns: &str, class: &str, method: &str) -> String {
    match (ns.is_empty(), class.is_empty(), method.is_empty()) {
        (true, true, true) => String::new(),
        (false, true, true) => ns.to_string(),
        (true, false, true) => class.to_string(),
        (true, true, false) => method.to_string(),
        (false, false, true) => {
            let mut s = String::with_capacity(ns.len() + 1 + class.len());
            s.push_str(ns);
            s.push('.');
            s.push_str(class);
            s
        }
        (true, false, false) => {
            let mut s = String::with_capacity(class.len() + 1 + method.len());
            s.push_str(class);
            s.push('.');
            s.push_str(method);
            s
        }
        (false, true, false) => {
            let mut s = String::with_capacity(ns.len() + 1 + method.len());
            s.push_str(ns);
            s.push('.');
            s.push_str(method);
            s
        }
        (false, false, false) => {
            let mut s = String::with_capacity(ns.len() + 1 + class.len() + 1 + method.len());
            s.push_str(ns);
            s.push('.');
            s.push_str(class);
            s.push('.');
            s.push_str(method);
            s
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Safely extract the text slice covered by a tree-sitter node.
#[inline]
fn node_text<'a>(source: &'a str, node: &tree_sitter::Node) -> &'a str {
    source.get(node.start_byte()..node.end_byte()).unwrap_or("")
}

/// Case-insensitive prefix check without allocating an uppercase copy.
#[inline]
fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len()
        && haystack.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

/// Classify a SQL string as stored-proc or inline.
///
/// Heuristics (in priority order):
/// 1. Starts with `EXEC`/`EXECUTE` → stored proc (extract proc name)
/// 2. Single token, length > 2, no whitespace → stored proc name
/// 3. Everything else → inline SQL (hashed)
fn classify_sql(sql: &str) -> (String, &'static str) {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return ("sql:inline:empty".into(), "inline_sql");
    }

    // Check for EXEC/EXECUTE prefix without allocating an uppercase copy
    if starts_with_ci(trimmed, "EXECUTE ") {
        if let Some(proc) = extract_proc_name(&trimmed[8..]) {
            return (format!("sql:stored_proc:{proc}"), "stored_proc");
        }
    } else if starts_with_ci(trimmed, "EXEC ")
        && let Some(proc) = extract_proc_name(&trimmed[5..])
    {
        return (format!("sql:stored_proc:{proc}"), "stored_proc");
    }

    // Single identifier → stored proc
    if !trimmed.contains(char::is_whitespace) && trimmed.len() > 2 {
        (format!("sql:stored_proc:{trimmed}"), "stored_proc")
    } else {
        let h = blake3::hash(trimmed.as_bytes()).to_hex();
        (format!("sql:inline:{}", &h[..12]), "inline_sql")
    }
}

/// Extract and clean the stored procedure name after EXEC/EXECUTE.
/// Strips all SQL bracket-quoting characters ([ and ]) from the name,
/// so [dbo].sp_Proc becomes dbo.sp_Proc.
fn extract_proc_name(after_exec: &str) -> Option<String> {
    let rest = after_exec.trim_start();
    let raw = rest.split_whitespace().next().unwrap_or(rest);
    let clean: String = raw.chars().filter(|&c| c != '[' && c != ']').collect();
    if clean.is_empty() { None } else { Some(clean) }
}

// ── P0.6 Handles clause ─────────────────────────────────────────────────────

pub fn extract_handles(fqn_maps: &FqnMaps, source: &str) -> Vec<ExtractedEdge> {
    let Some(sub_re) = get_compiled_regex(
        &HANDLES_SUB_RE,
        r"(?ix)
            \bSub\s+(?P<handler>[A-Za-z_][A-Za-z0-9_]*)
            \s*\([^)]*\)
            \s*Handles\s+
            (?P<list>[A-Za-z0-9_.]+(?:\s*,\s*[A-Za-z0-9_.]+)*)
            ",
        "vb_handles_sub",
    ) else {
        return Vec::new();
    };

    let Some(pair_re) = get_compiled_regex(
        &HANDLES_PAIR_RE,
        r"(?P<ctrl>[A-Za-z_][A-Za-z0-9_]*)\.(?P<evt>[A-Za-z_][A-Za-z0-9_]*)",
        "vb_handles_pair",
    ) else {
        return Vec::new();
    };

    let mut edges = Vec::new();
    let joined = join_logical_lines(source);

    for (line_no, line) in joined.iter().enumerate() {
        if let Some(caps) = sub_re.captures(line) {
            let handler = caps["handler"].to_string();
            let handler_fqn = fqn_maps.resolve(&handler);
            let list = &caps["list"];

            for pair_caps in pair_re.captures_iter(list) {
                let ctrl_id = pair_caps["ctrl"].to_string();
                let event_name = pair_caps["evt"].to_string();

                let source_kind = match ctrl_id.as_bytes() {
                    // Fast check: "Me" or "MyBase" (case-insensitive)
                    [b'M' | b'm', b'e' | b'E']
                    | [
                        b'M' | b'm',
                        b'y' | b'Y',
                        b'B' | b'b',
                        b'a' | b'A',
                        b's' | b'S',
                        b'e' | b'E',
                    ] => "self",
                    _ => "control",
                };

                let mut meta = HashMap::from([("event".into(), event_name)]);
                if handler_fqn != handler {
                    meta.insert("fqn".into(), handler_fqn.clone());
                }

                edges.push(ExtractedEdge {
                    source_name: ctrl_id,
                    source_kind: source_kind,
                    source_start_line: (line_no + 1) as u32,
                    source_language: "vb",
                    target_name: handler.clone(),
                    target_kind: Some("function"),
                    target_start_line: Some((line_no + 1) as u32),
                    kind: "event_wiring",
                    metadata: Some(meta),
                });
            }
        }
    }
    edges
}

/// Extract AddHandler / RemoveHandler event wiring.
///
/// Pattern: `AddHandler ctrl.Event, AddressOf handlerName`
/// Common in dynamically-created controls (Repeaters, GridViews).
pub fn extract_addhandler(fqn_maps: &FqnMaps, source: &str) -> Vec<ExtractedEdge> {
    let Some(re) = get_compiled_regex(
        &ADDHANDLER_RE,
        r"(?ix)
            \bAddHandler\s+
            (?P<ctrl>[A-Za-z_][A-Za-z0-9_]*)\.(?P<evt>[A-Za-z_][A-Za-z0-9_]*)
            \s*,\s*AddressOf\s+
            (?P<handler>[A-Za-z_][A-Za-z0-9_.]*)
            ",
        "vb_addhandler",
    ) else {
        return Vec::new();
    };

    let mut edges = Vec::new();
    for (line_no, line) in source.lines().enumerate() {
        if let Some(caps) = re.captures(line) {
            let ctrl_id = caps["ctrl"].to_string();
            let event_name = caps["evt"].to_string();
            let handler_raw = caps["handler"].to_string();
            // If handler is qualified (Me.DoStuff), take the last part
            let handler_short = handler_raw.split('.').next_back().unwrap_or(&handler_raw);
            let handler_fqn = fqn_maps.resolve(handler_short);

            let mut meta = HashMap::from([
                ("event".into(), event_name),
                ("wiring".into(), "AddHandler".into()),
            ]);
            if handler_fqn != handler_short {
                meta.insert("fqn".into(), handler_fqn.clone());
            }

            edges.push(ExtractedEdge {
                source_name: ctrl_id,
                source_kind: "control",
                source_start_line: (line_no + 1) as u32,
                source_language: "vb",
                target_name: handler_short.to_string(),
                target_kind: Some("function"),
                target_start_line: Some((line_no + 1) as u32),
                kind: "event_wiring",
                metadata: Some(meta),
            });
        }
    }
    edges
}

/// Strip an inline VB.NET comment while respecting string literal boundaries.
///
/// In VB.NET, `'` (apostrophe) starts a comment — unless it appears inside
/// a string literal (`"..."`). This function returns the slice of the line
/// up to (but not including) the first comment marker outside a string, or
/// the entire line if there is no comment.
///
/// Also handles the `REM` keyword as a comment starter (legacy syntax) when
/// it appears at the start of a token boundary (preceded by whitespace or
/// start-of-line) and is not inside a string.
fn strip_vb_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut in_string = false;
    let mut i = 0;

    while i < len {
        let b = bytes[i];
        if b == b'"' {
            in_string = !in_string;
            i += 1;
        } else if !in_string && b == b'\'' {
            // Found a comment outside a string literal.
            return &line[..i];
        } else if !in_string
            && i + 3 <= len
            && bytes[i..i + 3].eq_ignore_ascii_case(b"rem")
            && (i == 0 || bytes[i - 1].is_ascii_whitespace())
            && (i + 3 == len || bytes[i + 3].is_ascii_whitespace())
        {
            // `REM` keyword at a word boundary outside a string → comment.
            return &line[..i];
        } else {
            i += 1;
        }
    }
    line
}

/// Join VB.NET logical lines (lines ending with ` _` are continuations).
///
/// Per the VB spec, line continuation is a *space followed by underscore*
/// at end of line — a bare trailing `_` (e.g. identifier `my_var_`) is NOT
/// a continuation.
///
/// Inline comments (`'` or `REM`) are legally allowed *after* the continuation
/// character. Example:
/// ```vb
/// Dim myVar As Integer = _ ' This is my variable
///     GetSomeValue()
/// ```
/// We strip inline comments (while respecting string literals) before checking
/// for the continuation marker.
fn join_logical_lines(source: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();

    for raw_line in source.lines() {
        // 1. Strip inline comments while respecting string literals.
        let code_only = strip_vb_comment(raw_line);
        // 2. Trim trailing whitespace from the code portion.
        let trimmed = code_only.trim_end();

        // 3. Check for continuation: trailing ` _` (space then underscore)
        if trimmed.len() >= 2 && trimmed.ends_with(" _") {
            current.push_str(&trimmed[..trimmed.len() - 2]);
            current.push(' ');
        } else {
            current.push_str(trimmed);
            result.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

// ── P0.4a Tree-sitter Enhanced CommandText Extraction ─────────────────────────

/// Extract full `CommandText` assignment expressions using the tree-sitter parse tree.
///
/// Finds `.CommandText = <expression>` assignments and extracts the complete RHS,
/// including string concatenations. This captures dynamic SQL that the regex
/// fallback (which stops at the first closing quote) would miss.
///
/// Example:
///   `cmd.CommandText = "SELECT * FROM Users WHERE id = " & userId.ToString()`
/// Produces: `SELECT * FROM Users WHERE id = {dynamic}`
///
/// Returns `Vec<(sql_text, byte_position)>`.
fn extract_ts_command_text(tree: &tree_sitter::Tree, source: &str) -> Vec<(String, usize)> {
    let mut results = Vec::new();

    let Some(re) = get_compiled_regex(
        &CMD_TEXT_ASSIGN_RE,
        r"(?i)\.\s*CommandText\s*=\s*",
        "vb_command_text_assign",
    ) else {
        return results;
    };

    for m in re.find_iter(source) {
        let rhs_start = m.end();
        if rhs_start >= source.len() {
            continue;
        }

        // Use tree-sitter to find the extent of the RHS expression.
        let rhs_end = find_assignment_rhs_end(tree, rhs_start, m.start());

        let Some(rhs_text) = source.get(rhs_start..rhs_end) else {
            continue;
        };
        let rhs_text = rhs_text.trim();
        if rhs_text.is_empty() {
            continue;
        }

        // Extract SQL from the potentially concatenated expression.
        let sql = extract_sql_from_concat_expr(rhs_text);
        if !sql.trim().is_empty() {
            results.push((sql, m.start()));
        }
    }

    results
}

/// Walk up the tree-sitter AST from a byte position to find the end of the
/// enclosing statement. This gives us the full extent of the RHS expression,
/// including string concatenations that span the rest of the statement.
///
/// Safety: caps the extent at 2000 bytes from `match_start` to prevent
/// runaway extraction in malformed trees.
fn find_assignment_rhs_end(
    tree: &tree_sitter::Tree,
    rhs_start: usize,
    match_start: usize,
) -> usize {
    let max_end = match_start + 2000;

    let Some(node) = tree
        .root_node()
        .named_descendant_for_byte_range(rhs_start, rhs_start)
    else {
        // Fallback: scan forward to end of logical line.
        return rhs_start;
    };

    let mut current = node;

    loop {
        if current.end_byte() > max_end {
            break;
        }

        let kind = current.kind();
        // Statement-level nodes: stop here.
        if kind.contains("statement") || kind.contains("assignment") {
            return current.end_byte().min(max_end);
        }

        match current.parent() {
            Some(parent) => {
                let pk = parent.kind();
                // If parent is a block, body, or compilation unit, current IS the statement.
                if pk.contains("block")
                    || pk.contains("body")
                    || pk == "compilation_unit"
                    || pk == "program"
                {
                    return current.end_byte().min(max_end);
                }
                current = parent;
            }
            None => break,
        }
    }

    current.end_byte().min(max_end)
}

/// Extract SQL from a potentially concatenated VB.NET expression.
///
/// Parses string literals and replaces non-literal parts with `{dynamic}`.
///
/// Example: `"SELECT * FROM Users WHERE id = " & userId.ToString()`
/// becomes: `SELECT * FROM Users WHERE id = {dynamic}`
fn extract_sql_from_concat_expr(expr: &str) -> String {
    let parts = split_concat_parts(expr);
    let mut sql = String::new();

    for part in parts {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('"') {
            // String literal — extract content.
            if let Some(content) = extract_vb_string_literal(trimmed) {
                sql.push_str(&content);
            } else {
                sql.push_str("{dynamic}");
            }
        } else {
            // Non-literal part (variable, function call, etc.)
            sql.push_str("{dynamic}");
        }
    }

    sql
}

/// Split a VB expression by concatenation operators (`&` and `+`).
/// Respects string literal boundaries so `"a & b"` is not split.
fn split_concat_parts(expr: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let bytes = expr.as_bytes();
    let mut start = 0;
    let mut in_string = false;
    let mut paren_depth: i32 = 0;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            in_string = !in_string;
        } else if !in_string {
            if b == b'(' {
                paren_depth += 1;
            } else if b == b')' {
                paren_depth -= 1;
            } else if paren_depth == 0 && b == b'&' {
                parts.push(&expr[start..i]);
                start = i + 1;
            }
        }
        i += 1;
    }

    if start < expr.len() {
        parts.push(&expr[start..]);
    }

    parts
}

/// Extract the content of a VB.NET string literal.
/// Handles doubled quotes (`""`) as escaped quotes.
fn extract_vb_string_literal(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if !trimmed.starts_with('"') {
        return None;
    }

    let mut content = String::new();
    let bytes = trimmed.as_bytes();
    let mut i = 1; // Skip opening quote
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                // Doubled quote → literal quote
                content.push('"');
                i += 2;
            } else {
                // Closing quote
                return Some(content);
            }
        } else {
            content.push(bytes[i] as char);
            i += 1;
        }
    }

    // No closing quote found — return what we have if non-empty.
    if !content.is_empty() {
        Some(content)
    } else {
        None
    }
}

// ── P0.4b SQL Extraction (Regex) ────────────────────────────────────────────

fn regex_extract_sql(source: &str, ts_cmd_text_positions: &[usize]) -> Vec<(ExtractedEdge, usize)> {
    let mut results = Vec::new();

    let Some(sql_cmd_re) = get_compiled_regex(
        &SQL_CMD_RE,
        r#"(?i)New\s+(?:Sql|OleDb|Odbc)Command\s*\(\s*"([^"]+)""#,
        "vb_sql_cmd",
    ) else {
        return results;
    };

    let Some(sql_text_re) = get_compiled_regex(
        &SQL_TEXT_RE,
        r#"(?i)(?P<var>[A-Za-z_][A-Za-z0-9_]*)?\.CommandText\s*=\s*"(?P<sql>[^"]+)""#,
        "vb_sql_text",
    ) else {
        return results;
    };

    let Some(sql_exec_re) = get_compiled_regex(
        &SQL_EXEC_RE,
        r#"(?i)"(EXEC(?:UTE)?\s+[^"]+)""#,
        "vb_sql_exec",
    ) else {
        return results;
    };

    let Some(sql_exec_call_re) = get_compiled_regex(
        &SQL_EXEC_CALL_RE,
        r"(?i)(?P<var>[A-Za-z_][A-Za-z0-9_]*)\.(?P<method>Execute(?:Reader|NonQuery|Scalar))\s*\(",
        "vb_sql_exec_call",
    ) else {
        return results;
    };

    let Some(sql_adapter_re) = get_compiled_regex(
        &SQL_ADAPTER_RE,
        r#"(?i)New\s+(?:Sql|OleDb|Odbc)DataAdapter\s*\(\s*"([^"]+)""#,
        "vb_sql_adapter",
    ) else {
        return results;
    };

    let Some(sql_proc_type_re) = get_compiled_regex(
        &SQL_PROC_TYPE_RE,
        r#"(?i)(?P<var>[A-Za-z_][A-Za-z0-9_]*)\.CommandType\s*=\s*CommandType\.StoredProcedure"#,
        "vb_sql_proc_type",
    ) else {
        return results;
    };

    // De-duplicate by lowercased SQL. Use a compact hash set of u64 to avoid
    // storing full SQL strings a second time when files are large.
    let mut seen_hashes: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // Map of variable name -> is_stored_proc
    let mut var_is_sp: HashMap<String, bool> = HashMap::new();

    let mut add_sql_edge =
        |results: &mut Vec<(ExtractedEdge, usize)>, sql: &str, pos: usize, force_sp: bool| {
            let trimmed = sql.trim();
            if trimmed.is_empty() {
                return;
            }
            // Hash the lowercased SQL for dedup — avoids storing a second copy.
            let hash = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for b in trimmed.bytes() {
                    b.to_ascii_lowercase().hash(&mut h);
                }
                h.finish()
            };
            if !seen_hashes.insert(hash) {
                return;
            }

            let (target_id, target_kind_str) = if force_sp {
                let clean = sql
                    .chars()
                    .filter(|&c| c != '[' && c != ']')
                    .collect::<String>();
                (format!("sql:stored_proc:{clean}"), "stored_proc")
            } else {
                classify_sql(trimmed)
            };

            let snippet: String = trimmed.chars().take(SQL_SNIPPET_MAX_LEN).collect();
            let meta = HashMap::from([("sql_snippet".into(), snippet)]);
            results.push((
                ExtractedEdge {
                    source_name: "file".into(),
                    source_kind: "file",
                    source_start_line: 0,
                    source_language: "vb",
                    target_name: target_id,
                    target_kind: Some(target_kind_str),
                    target_start_line: None,
                    kind: "sql_calls",
                    metadata: Some(meta),
                },
                pos,
            ));
        };

    // For stateful SP detection, we still need physical ordering.
    // Captures_iter gives ordering.
    for cap in sql_proc_type_re.captures_iter(source) {
        var_is_sp.insert(cap["var"].to_lowercase(), true);
    }

    for cap in sql_cmd_re.captures_iter(source) {
        let Some(anchor) = cap.get(0) else {
            continue;
        };
        add_sql_edge(&mut results, &cap[1], anchor.start(), false);
    }
    for cap in sql_text_re.captures_iter(source) {
        let Some(anchor) = cap.get(0) else {
            continue;
        };
        // Skip if tree-sitter already handled this CommandText assignment.
        // The ts positions are the start of `.CommandText =` matches; regex
        // matches start at the variable name before `.CommandText`. A 100-byte
        // window handles the offset difference.
        let pos = anchor.start();
        if ts_cmd_text_positions
            .iter()
            .any(|&ts_pos| pos.abs_diff(ts_pos) < 100)
        {
            continue;
        }
        let var = cap
            .name("var")
            .map(|m| m.as_str().to_lowercase())
            .unwrap_or_default();
        let is_sp = var_is_sp.get(&var).cloned().unwrap_or(false);
        add_sql_edge(&mut results, &cap["sql"], anchor.start(), is_sp);
    }
    for cap in sql_exec_re.captures_iter(source) {
        let Some(anchor) = cap.get(0) else {
            continue;
        };
        add_sql_edge(&mut results, &cap[1], anchor.start(), false);
    }
    for cap in sql_adapter_re.captures_iter(source) {
        let Some(anchor) = cap.get(0) else {
            continue;
        };
        add_sql_edge(&mut results, &cap[1], anchor.start(), false);
    }

    // Emit lightweight `sql_exec` edges for Execute* calls
    for cap in sql_exec_call_re.captures_iter(source) {
        let var = &cap["var"];
        let method = &cap["method"];
        let Some(anchor) = cap.get(0) else {
            continue;
        };
        let pos = anchor.start();
        let meta = HashMap::from([("method".into(), method.to_string())]);
        results.push((
            ExtractedEdge {
                source_name: "file".into(),
                source_kind: "file",
                source_start_line: 0,
                source_language: "vb",
                target_name: format!("{var}.{method}"),
                target_kind: Some("sql_exec"),
                target_start_line: None,
                kind: "sql_exec",
                metadata: Some(meta),
            },
            pos,
        ));
    }

    results
}

// ── Regex Fallback ──────────────────────────────────────────────────────────

/// Pre-computed line offset table for O(log n) byte-offset → line-number.
struct LineIndex {
    offsets: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut offsets = Vec::with_capacity(source.len() / 40 + 1);
        offsets.push(0);
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                offsets.push(i + 1);
            }
        }
        Self { offsets }
    }

    #[inline]
    fn line_of(&self, byte_offset: usize) -> u32 {
        match self.offsets.binary_search(&byte_offset) {
            Ok(idx) => (idx + 1) as u32,
            Err(idx) => idx as u32,
        }
    }
}

fn regex_extract(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();

    if source.len() > MAX_REGEX_FALLBACK_SOURCE_BYTES {
        tracing::warn!(
            "regex fallback skipped for {} ({} bytes > {} byte limit)",
            path.display(),
            source.len(),
            MAX_REGEX_FALLBACK_SOURCE_BYTES,
        );
        return (symbols, edges);
    }

    let Some(ns_re) = get_compiled_regex(
        &REGEX_NS_RE,
        r"(?im)^\s*Namespace\s+([A-Za-z_][A-Za-z0-9_.]*)\s*$",
        "vb_regex_ns",
    ) else {
        return (symbols, edges);
    };
    let Some(type_re) = get_compiled_regex(
        &REGEX_TYPE_RE,
        r"(?im)^\s*(?:(?:Public|Private|Protected|Friend|Partial|MustInherit|NotInheritable)\s+)*(?:Class|Module|Structure|Interface|Enum)\s+([A-Za-z_][A-Za-z0-9_]*)",
        "vb_regex_type",
    ) else {
        return (symbols, edges);
    };
    let Some(member_re) = get_compiled_regex(
        &REGEX_MEMBER_RE,
        r"(?im)^\s*(?:(?:Public|Private|Protected|Friend|Overrides|Overridable|MustOverride|NotOverridable|Shared|Async|ReadOnly|WriteOnly|Default|Iterator)\s+)*(?P<member_kind>Sub|Function|Property)\s+(?P<member_name>[A-Za-z_][A-Za-z0-9_]*)",
        "vb_regex_member",
    ) else {
        return (symbols, edges);
    };

    let line_index = LineIndex::new(source);

    // Collect all regex hits with byte positions, then sort and process in
    // source order so namespace/class context is always correct.
    enum Hit<'a> {
        Namespace(&'a str, usize),
        Type(&'a str, usize),
        Member {
            name: &'a str,
            is_property: bool,
            pos: usize,
        },
    }

    let mut hits: Vec<Hit<'_>> = Vec::new();

    for cap in ns_re.captures_iter(source) {
        if let (Some(m), Some(m0)) = (cap.get(1), cap.get(0)) {
            hits.push(Hit::Namespace(m.as_str().trim(), m0.start()));
        }
    }
    for cap in type_re.captures_iter(source) {
        if let (Some(m), Some(m0)) = (cap.get(1), cap.get(0)) {
            hits.push(Hit::Type(m.as_str().trim(), m0.start()));
        }
    }
    for cap in member_re.captures_iter(source) {
        if let (Some(kind_match), Some(name_match), Some(m0)) =
            (cap.name("member_kind"), cap.name("member_name"), cap.get(0))
        {
            let is_property = kind_match.as_str().eq_ignore_ascii_case("property");
            hits.push(Hit::Member {
                name: name_match.as_str().trim(),
                is_property,
                pos: m0.start(),
            });
        }
    }

    hits.sort_by_key(|h| match h {
        Hit::Namespace(_, pos) | Hit::Type(_, pos) | Hit::Member { pos, .. } => *pos,
    });

    let mut current_ns = String::new();
    let mut current_class = String::new();

    for hit in &hits {
        match hit {
            Hit::Namespace(ns, _) => {
                current_ns = (*ns).to_string();
            }
            Hit::Type(name, pos) => {
                current_class = (*name).to_string();
                let line_no = line_index.line_of(*pos);
                let fqn = make_fqn(&current_ns, &current_class, "");
                let meta = HashMap::from([("fqn".into(), fqn)]);
                symbols.push(ExtractedSymbol {
                    name: current_class.clone(),
                    kind: "class",
                    start_line: line_no,
                    end_line: line_no,
                    metadata: Some(meta),
                });
            }
            Hit::Member {
                name,
                is_property,
                pos,
            } => {
                let line_no = line_index.line_of(*pos);
                let fqn = make_fqn(&current_ns, &current_class, name);
                let kind = if *is_property { "property" } else { "function" };
                let meta = HashMap::from([("fqn".into(), fqn)]);
                symbols.push(ExtractedSymbol {
                    name: (*name).to_string(),
                    kind,
                    start_line: line_no,
                    end_line: line_no,
                    metadata: Some(meta),
                });
            }
        }
    }

    if has_sql_keyword(source) {
        let sql_results = regex_extract_sql(source, &[]);
        for (edge, _) in sql_results {
            edges.push(edge);
        }
    }
    if has_handles_keyword(source) {
        // Build FqnMaps from the symbols we already extracted via regex,
        // so that Handles clause handler names can be resolved to their FQN.
        let mut fqn_maps = FqnMaps::with_capacity(symbols.len());
        for sym in &symbols {
            if let Some(meta) = &sym.metadata {
                if let Some(fqn) = meta.get("fqn") {
                    fqn_maps.insert_name(&sym.name, fqn.clone());
                }
            }
        }
        edges.extend(extract_handles(&fqn_maps, source));
    }

    (symbols, edges)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ── FQN tests ────────────────────────────────────────────────────────

    #[test]
    fn test_vb_class_and_method_fqn() {
        let code = r#"
Namespace MyOrg.Reports
    Public Class OrderReport
        Public Sub GenerateReport()
        End Sub
        Private Function FormatRow() As String
            Return ""
        End Function
    End Class
End Namespace
"#;
        let (symbols, _edges) = extract_vb(Path::new("OrderReport.vb"), code);

        let cls = symbols.iter().find(|s| s.name == "OrderReport").unwrap();
        assert_eq!(cls.kind, "class");
        assert_eq!(
            cls.metadata.as_ref().unwrap()["fqn"],
            "MyOrg.Reports.OrderReport"
        );

        let gen_sym = symbols.iter().find(|s| s.name == "GenerateReport").unwrap();
        assert_eq!(gen_sym.kind, "function");
        assert_eq!(
            gen_sym.metadata.as_ref().unwrap()["fqn"],
            "MyOrg.Reports.OrderReport.GenerateReport"
        );
    }

    #[test]
    fn test_regex_fallback_hard_limit_skips_extraction() {
        let mut code = String::with_capacity(MAX_TREE_SITTER_SOURCE_BYTES + 256);
        code.push_str("Namespace N\n");
        code.push_str("Public Class C\n");
        code.push_str("Public Sub M()\nEnd Sub\n");
        code.push_str("End Class\nEnd Namespace\n");
        while code.len() <= MAX_TREE_SITTER_SOURCE_BYTES {
            code.push_str("' pad to force fallback\n");
        }

        let (symbols, edges) = extract_vb(Path::new("Huge.vb"), &code);
        assert!(symbols.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn test_vb_module_fqn() {
        let code = r#"
Namespace Helpers
    Module StringUtils
        Public Sub Trim()
        End Sub
    End Module
End Namespace
"#;
        let (symbols, _) = extract_vb(Path::new("StringUtils.vb"), code);
        let m = symbols.iter().find(|s| s.name == "StringUtils").unwrap();
        assert_eq!(m.metadata.as_ref().unwrap()["fqn"], "Helpers.StringUtils");
    }

    #[test]
    fn test_make_fqn_all_empty() {
        assert_eq!(make_fqn("", "", ""), "");
    }

    #[test]
    fn test_make_fqn_single() {
        assert_eq!(make_fqn("NS", "", ""), "NS");
        assert_eq!(make_fqn("", "Cls", ""), "Cls");
        assert_eq!(make_fqn("", "", "Meth"), "Meth");
    }

    #[test]
    fn test_make_fqn_two() {
        assert_eq!(make_fqn("NS", "Cls", ""), "NS.Cls");
        assert_eq!(make_fqn("NS", "", "Meth"), "NS.Meth");
        assert_eq!(make_fqn("", "Cls", "Meth"), "Cls.Meth");
    }

    #[test]
    fn test_make_fqn_full() {
        assert_eq!(make_fqn("NS", "Cls", "Meth"), "NS.Cls.Meth");
    }

    // ── FqnMaps resolve tests ────────────────────────────────────────────

    #[test]
    fn test_fqn_maps_resolve_exact() {
        let mut maps = FqnMaps::with_capacity(4);
        maps.insert_name("Foo", "NS.Class.Foo".into());
        assert_eq!(maps.resolve("Foo"), "NS.Class.Foo");
    }

    #[test]
    fn test_fqn_maps_resolve_case_insensitive() {
        let mut maps = FqnMaps::with_capacity(4);
        maps.insert_name("Foo", "NS.Class.Foo".into());
        assert_eq!(maps.resolve("foo"), "NS.Class.Foo");
        assert_eq!(maps.resolve("FOO"), "NS.Class.Foo");
    }

    #[test]
    fn test_fqn_maps_resolve_not_found() {
        let maps = FqnMaps::with_capacity(4);
        assert_eq!(maps.resolve("Unknown"), "Unknown");
    }

    // ── SQL tests ────────────────────────────────────────────────────────

    #[test]
    fn test_sql_stored_proc_detection() {
        let code = r#"
Namespace Data
    Class OrderDao
        Public Function GetOrders() As DataSet
            Dim cmd As New SqlCommand("sp_GetVbOrders")
            Return Nothing
        End Function
    End Class
End Namespace
"#;
        let (_syms, edges) = extract_vb(Path::new("OrderDao.vb"), code);
        let sql_edge = edges
            .iter()
            .find(|e| e.kind == "sql_calls" && e.target_kind.as_deref() == Some("stored_proc"))
            .unwrap();
        assert_eq!(sql_edge.target_name, "sql:stored_proc:sp_GetVbOrders");
    }

    #[test]
    fn test_sql_inline_detection() {
        let code = r#"cmd.CommandText = "SELECT * FROM Orders WHERE id = @id""#;
        let results = regex_extract_sql(code, &[]);
        let edges: Vec<_> = results.into_iter().map(|(e, _)| e).collect();
        assert_eq!(edges.iter().filter(|e| e.kind == "sql_calls").count(), 1);
        let e = &edges[0];
        assert_eq!(e.target_kind.as_deref(), Some("inline_sql"));
        assert!(e.target_name.starts_with("sql:inline:"));
    }

    #[test]
    fn test_sql_exec_detection() {
        let code = r#"Dim cmd As New SqlCommand("EXEC sp_UpdateOrders @id, @status")"#;
        let results = regex_extract_sql(code, &[]);
        let edges: Vec<_> = results.into_iter().map(|(e, _)| e).collect();
        let proc = edges
            .iter()
            .find(|e| e.kind == "sql_calls" && e.target_kind.as_deref() == Some("stored_proc"))
            .unwrap();
        assert_eq!(proc.target_name, "sql:stored_proc:sp_UpdateOrders");
    }

    #[test]
    fn test_sql_execute_nonquery_detection() {
        let code = "cmd.ExecuteNonQuery()";
        let results = regex_extract_sql(code, &[]);
        let edges: Vec<_> = results.into_iter().map(|(e, _)| e).collect();
        let exec_edge = edges.iter().find(|e| e.kind == "sql_exec").unwrap();
        assert_eq!(exec_edge.target_name, "cmd.ExecuteNonQuery");
    }

    #[test]
    fn test_sql_deduplication() {
        let code = r#"
Dim cmd1 As New SqlCommand("sp_GetOrders")
Dim cmd2 As New SqlCommand("sp_GetOrders")
"#;
        let results = regex_extract_sql(code, &[]);
        let edges: Vec<_> = results.into_iter().map(|(e, _)| e).collect();
        let sql_edges: Vec<_> = edges.iter().filter(|e| e.kind == "sql_calls").collect();
        assert_eq!(sql_edges.len(), 1, "duplicate SQL should be deduplicated");
    }

    #[test]
    fn test_sql_case_insensitive_dedup() {
        let code = r#"
Dim cmd1 As New SqlCommand("SP_GETORDERS")
Dim cmd2 As New SqlCommand("sp_GetOrders")
"#;
        let results = regex_extract_sql(code, &[]);
        let edges: Vec<_> = results.into_iter().map(|(e, _)| e).collect();
        let sql_edges: Vec<_> = edges.iter().filter(|e| e.kind == "sql_calls").collect();
        assert_eq!(sql_edges.len(), 1, "case-different SQL should deduplicate");
    }

    #[test]
    fn test_vb_sql_advanced() {
        let code = r#"
            Dim adapter As New SqlDataAdapter("SELECT * FROM Users", conn)
            cmd.CommandType = CommandType.StoredProcedure
            cmd.CommandText = "GetDetails"
            cmd.ExecuteReader()
            Dim cmd2 As New SqlCommand() With {.CommandText = "SELECT 1"}
        "#;
        let results = regex_extract_sql(code, &[]);
        let edges: Vec<_> = results.into_iter().map(|(e, _)| e).collect();

        // 1. SqlDataAdapter
        assert!(
            edges
                .iter()
                .any(|e| e.target_name == "sql:inline:63cc23e01345")
        );

        // 2. CommandType.StoredProcedure + CommandText
        assert!(
            edges
                .iter()
                .any(|e| e.target_name == "sql:stored_proc:GetDetails")
        );

        // 3. ExecuteReader call
        assert!(edges.iter().any(|e| e.target_name == "cmd.ExecuteReader"));

        // 4. Object initializer
        assert!(
            edges
                .iter()
                .any(|e| e.target_name == "sql:inline:dd0b347a3141")
        );
    }

    #[test]
    fn test_classify_sql_empty() {
        let (name, kind) = classify_sql("   ");
        assert_eq!(name, "sql:inline:empty");
        assert_eq!(kind, "inline_sql");
    }

    #[test]
    fn test_classify_sql_exec_with_schema() {
        let (name, kind) = classify_sql("EXEC [dbo].sp_Proc");
        assert_eq!(name, "sql:stored_proc:dbo.sp_Proc");
        assert_eq!(kind, "stored_proc");
    }

    #[test]
    fn test_classify_sql_execute_variant() {
        let (name, kind) = classify_sql("execute sp_Foo @bar");
        assert_eq!(name, "sql:stored_proc:sp_Foo");
        assert_eq!(kind, "stored_proc");
    }

    #[test]
    fn test_vb_expanded_query() {
        let code = r#"
Namespace App
    Class UI
        Public WithEvents btnSubmit As Button
        Public Event Submitted(id As Integer)
        Public Property Label As String
            Get
                Return ""
            End Get
        End Property
    End Class
End Namespace
"#;
        let (symbols, _edges) = extract_vb(Path::new("UI.vb"), code);

        let field = symbols
            .iter()
            .find(|s| s.name == "btnSubmit")
            .expect("Should find field");
        assert_eq!(field.kind, "field");
        assert_eq!(field.metadata.as_ref().unwrap()["fqn"], "App.UI.btnSubmit");

        let event = symbols
            .iter()
            .find(|s| s.name == "Submitted")
            .expect("Should find event");
        assert_eq!(event.kind, "event");
        assert_eq!(event.metadata.as_ref().unwrap()["fqn"], "App.UI.Submitted");

        let prop = symbols
            .iter()
            .find(|s| s.name == "Label")
            .expect("Should find property");
        assert_eq!(prop.kind, "property");
        assert_eq!(prop.metadata.as_ref().unwrap()["fqn"], "App.UI.Label");
    }

    // ── Call extraction tests ────────────────────────────────────────────

    #[test]
    fn test_vb_call_extraction() {
        let code = r#"
Namespace App
    Module Main
        Sub A()
            B()
        End Sub
        Sub B()
        End Sub
    End Module
End Namespace
"#;
        let (_syms, edges) = extract_vb(Path::new("Main.vb"), code);
        let call_edge = edges
            .iter()
            .find(|e| e.kind == "calls" && e.target_name.ends_with(".B"))
            .unwrap();
        assert_eq!(call_edge.source_name, "App.Main.A");
    }

    #[test]
    fn test_vb_unresolved_call() {
        let code = r#"
Namespace App
    Module Main
        Sub A()
            UnknownMethod()
        End Sub
    End Module
End Namespace
"#;
        let (_symbols, edges) = extract_vb(Path::new("Main.vb"), code);
        let edge = edges
            .iter()
            .find(|e| e.target_name == "UnknownMethod")
            .expect("Should find edge to UnknownMethod");

        assert_eq!(edge.target_kind, None);
        assert_eq!(edge.metadata.as_ref().unwrap()["unresolved"], "true");
    }

    // ── Handles clause tests ─────────────────────────────────────────────

    #[test]
    fn test_vb_member_call_resolution() {
        let code = r#"
Namespace App
    Class Repo
        Shared Sub Save()
        End Sub
    End Class
    Class UI
        Sub Run()
            Repo.Save()
        End Sub
    End Class
End Namespace
"#;
        let (_, edges) = extract_vb(Path::new("App.vb"), code);
        let call = edges
            .iter()
            .find(|e| e.kind == "calls" && e.source_name == "App.UI.Run")
            .expect("Should find call edge");
        assert_eq!(call.target_name, "App.Repo.Save");
    }

    #[test]
    fn test_handles_simple() {
        let code = "Protected Sub btnPrint_Click(ByVal sender As Object, ByVal e As EventArgs) _\n    Handles btnPrint.Click\n    PrintReport()\nEnd Sub\n";
        let fqn_maps = FqnMaps::with_capacity(0);
        let edges = extract_handles(&fqn_maps, code);
        let ew = edges.iter().find(|e| e.kind == "event_wiring").unwrap();
        assert_eq!(ew.target_name, "btnPrint_Click");
        assert_eq!(ew.source_name, "btnPrint");
        assert_eq!(ew.source_kind, "control");
        assert_eq!(ew.metadata.as_ref().unwrap()["event"], "Click");
    }

    #[test]
    fn test_handles_multiple_events() {
        let code = "Protected Sub SaveAll(sender As Object, e As EventArgs) _\n    Handles btnSave.Click, btnApply.Click\n    DoSave()\nEnd Sub\n";
        let fqn_maps = FqnMaps::with_capacity(0);
        let edges = extract_handles(&fqn_maps, code);
        let wirings: Vec<_> = edges.iter().filter(|e| e.kind == "event_wiring").collect();
        assert_eq!(wirings.len(), 2);
        assert!(wirings.iter().any(|e| e.source_name == "btnSave"));
        assert!(wirings.iter().any(|e| e.source_name == "btnApply"));
    }

    #[test]
    fn test_handles_me_and_mybase() {
        let code = "Private Sub OnLoad(sender As Object, e As EventArgs) Handles Me.Load, MyBase.Init\nEnd Sub\n";
        let fqn_maps = FqnMaps::with_capacity(0);
        let edges = extract_handles(&fqn_maps, code);
        let wirings: Vec<_> = edges.iter().filter(|e| e.kind == "event_wiring").collect();
        assert_eq!(wirings.len(), 2);

        let me_edge = wirings.iter().find(|e| e.source_name == "Me").unwrap();
        assert_eq!(me_edge.source_kind, "self");
        assert_eq!(me_edge.metadata.as_ref().unwrap()["event"], "Load");

        let base_edge = wirings.iter().find(|e| e.source_name == "MyBase").unwrap();
        assert_eq!(base_edge.source_kind, "self");
    }

    // ── Line continuation tests ──────────────────────────────────────────

    #[test]
    fn test_join_logical_lines_continuation() {
        let source = "first _\nsecond _\nthird";
        let joined = join_logical_lines(source);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0], "first second third");
    }

    #[test]
    fn test_join_logical_lines_no_continuation() {
        let source = "first\nsecond\nthird";
        let joined = join_logical_lines(source);
        assert_eq!(joined.len(), 3);
    }

    #[test]
    fn test_join_logical_lines_identifier_with_underscore() {
        let source = "Dim my_var_\nAs Integer";
        let joined = join_logical_lines(source);
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0], "Dim my_var_");
    }

    #[test]
    fn test_join_logical_lines_trailing_unflushed() {
        let source = "line1 _\nline2 _";
        let joined = join_logical_lines(source);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0], "line1 line2 ");
    }

    // ── P12: Comment-aware line continuation tests ────────────────────────

    #[test]
    fn test_join_logical_lines_comment_after_continuation() {
        // VB.NET allows comments after the continuation character.
        // The continuation line's leading whitespace is preserved (only trailing ws is trimmed).
        let source = "Dim myVar As Integer = _ ' This is my variable\n    GetSomeValue()";
        let joined = join_logical_lines(source);
        assert_eq!(joined.len(), 1);
        // The joined line preserves the leading spaces from the continuation line
        assert_eq!(joined[0], "Dim myVar As Integer =     GetSomeValue()");
    }

    #[test]
    fn test_join_logical_lines_handles_with_comment() {
        let source = "Protected Sub btnPrint_Click(ByVal sender As Object, ByVal e As EventArgs) _ ' handler\n    Handles btnPrint.Click\n    PrintReport()\nEnd Sub\n";
        let joined = join_logical_lines(source);
        assert!(
            joined.iter().any(|l| l.contains("Handles btnPrint.Click")),
            "Should join continuation despite trailing comment: {:?}",
            joined
        );
    }

    #[test]
    fn test_strip_vb_comment_preserves_string_apostrophe() {
        // Apostrophe inside a string literal must NOT be treated as comment
        let line = r#"Response.Write("Don't fail here") ' real comment"#;
        let stripped = strip_vb_comment(line);
        assert_eq!(stripped, r#"Response.Write("Don't fail here") "#);
    }

    #[test]
    fn test_strip_vb_comment_no_comment() {
        let line = r#"Dim x As Integer = 42"#;
        assert_eq!(strip_vb_comment(line), line);
    }

    #[test]
    fn test_strip_vb_comment_rem_keyword() {
        let line = "Dim x = 1 REM old-style comment";
        let stripped = strip_vb_comment(line);
        assert_eq!(stripped, "Dim x = 1 ");
    }

    #[test]
    fn test_strip_vb_comment_rem_inside_string() {
        // REM inside a string is not a comment
        let line = r#"Dim s = "REM is not a comment""#;
        assert_eq!(strip_vb_comment(line), line);
    }

    #[test]
    fn test_join_logical_lines_addhandler_with_comment() {
        let source = "AddHandler ctrl.Click, _ ' wire up the event\n    AddressOf HandleClick";
        let joined = join_logical_lines(source);
        assert_eq!(joined.len(), 1);
        assert!(
            joined[0].contains("AddHandler") && joined[0].contains("AddressOf HandleClick"),
            "Should join AddHandler continuation: {:?}",
            joined
        );
    }

    // ── LineIndex tests ──────────────────────────────────────────────────

    #[test]
    fn test_line_index() {
        let source = "line1\nline2\nline3\n";
        let idx = LineIndex::new(source);
        assert_eq!(idx.line_of(0), 1);
        assert_eq!(idx.line_of(3), 1);
        assert_eq!(idx.line_of(6), 2);
        assert_eq!(idx.line_of(12), 3);
    }

    #[test]
    fn test_line_index_single_line() {
        let source = "hello";
        let idx = LineIndex::new(source);
        assert_eq!(idx.line_of(0), 1);
        assert_eq!(idx.line_of(4), 1);
    }

    // ── Edge case tests ──────────────────────────────────────────────────

    #[test]
    fn test_empty_source() {
        let (symbols, edges) = extract_vb(Path::new("empty.vb"), "");
        assert!(symbols.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn test_whitespace_only_source() {
        let (symbols, edges) = extract_vb(Path::new("blank.vb"), "   \n\n  \t  \n");
        assert!(symbols.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn test_comment_only_source() {
        let code = "' This is just a comment\n' And another\nREM old style comment\n";
        let (symbols, edges) = extract_vb(Path::new("comments.vb"), code);
        assert!(symbols.is_empty());
        assert!(edges.is_empty());
    }

    // ── Namespace helpers ────────────────────────────────────────────────

    #[test]
    fn test_build_dotted_namespace_empty() {
        assert_eq!(build_dotted_namespace(&[]), "");
    }

    #[test]
    fn test_build_dotted_namespace_single() {
        let stack = vec![("MyOrg.Data".to_string(), 999)];
        assert_eq!(build_dotted_namespace(&stack), "MyOrg.Data");
    }

    #[test]
    fn test_build_dotted_namespace_nested() {
        let stack = vec![("Outer".to_string(), 999), ("Inner".to_string(), 500)];
        assert_eq!(build_dotted_namespace(&stack), "Outer.Inner");
    }

    // ── Scope stack tests ────────────────────────────────────────────────

    #[test]
    fn test_find_enclosing_scope_walks_backwards() {
        let stack = vec![
            ScopeEntry {
                start: 0,
                end: 100,
                _name: "Outer".to_string(),
                fqn: "Outer".to_string(),
                kind: "class",
                line: 1,
            },
            ScopeEntry {
                start: 10,
                end: 50,
                _name: "Inner".to_string(),
                fqn: "Outer.Inner".to_string(),
                kind: "function",
                line: 5,
            },
        ];
        // Position 60 is past Inner's end but inside Outer
        let (name, kind, _) = find_enclosing_scope(&stack, 60);
        assert_eq!(name, "Outer");
        assert_eq!(kind, "class");
    }

    #[test]
    fn test_find_enclosing_scope_innermost() {
        let stack = vec![
            ScopeEntry {
                start: 0,
                end: 100,
                _name: "Outer".to_string(),
                fqn: "Outer".to_string(),
                kind: "class",
                line: 1,
            },
            ScopeEntry {
                start: 10,
                end: 50,
                _name: "Inner".to_string(),
                fqn: "Outer.Inner".to_string(),
                kind: "function",
                line: 5,
            },
        ];
        let (name, _, _) = find_enclosing_scope(&stack, 30);
        assert_eq!(name, "Outer.Inner");
    }

    #[test]
    fn test_find_enclosing_scope_falls_to_file() {
        let stack = vec![ScopeEntry {
            start: 0,
            end: 50,
            _name: "Only".to_string(),
            fqn: "Only".to_string(),
            kind: "class",
            line: 1,
        }];
        let (name, kind, _) = find_enclosing_scope(&stack, 60);
        assert_eq!(name, "file");
        assert_eq!(kind, "file");
    }

    // ── Short-circuit tests ──────────────────────────────────────────────

    #[test]
    fn test_has_handles_keyword() {
        assert!(has_handles_keyword("Sub Foo() Handles btn.Click"));
        assert!(has_handles_keyword("Sub Foo() handles btn.Click"));
        assert!(!has_handles_keyword("Sub Foo()\nEnd Sub"));
    }

    #[test]
    fn test_has_sql_keyword() {
        assert!(has_sql_keyword("New SqlCommand(\"sp_Foo\")"));
        assert!(has_sql_keyword("cmd.CommandText = \"SELECT 1\""));
        assert!(has_sql_keyword("cmd.ExecuteNonQuery()"));
        assert!(!has_sql_keyword("Dim x As Integer = 42"));
    }

    // ── starts_with_ci tests ─────────────────────────────────────────────

    #[test]
    fn test_starts_with_ci() {
        assert!(starts_with_ci("EXEC sp_Foo", "EXEC "));
        assert!(starts_with_ci("exec sp_Foo", "EXEC "));
        assert!(starts_with_ci("Execute sp_Foo", "EXECUTE "));
        assert!(!starts_with_ci("EXE", "EXEC "));
        assert!(!starts_with_ci("SELECT 1", "EXEC "));
    }

    // ── Regex fallback property detection ────────────────────────────────

    #[test]
    fn test_regex_fallback_detects_property() {
        let code = r#"
Namespace App
    Class Settings
        Public Property Name As String
        Public ReadOnly Property Count As Integer
    End Class
End Namespace
"#;
        let (symbols, _) = regex_extract(Path::new("Settings.vb"), code);
        let prop = symbols.iter().find(|s| s.name == "Name");
        assert!(prop.is_some(), "should detect Property");
        assert_eq!(prop.unwrap().kind, "property");

        let ro_prop = symbols.iter().find(|s| s.name == "Count");
        assert!(ro_prop.is_some(), "should detect ReadOnly Property");
        assert_eq!(ro_prop.unwrap().kind, "property");
    }

    // ── Lifecycle metadata tests ──────────────────────────────────────────

    #[test]
    fn test_lifecycle_page_load() {
        let code = r#"
Namespace Web
    Public Class MyPage
        Inherits System.Web.UI.Page
        Protected Sub Page_Load(sender As Object, e As EventArgs)
        End Sub
    End Class
End Namespace
"#;
        let (symbols, _) = extract_vb(Path::new("MyPage.aspx.vb"), code);
        let page_load = symbols.iter().find(|s| s.name == "Page_Load").unwrap();
        let meta = page_load.metadata.as_ref().unwrap();
        assert_eq!(meta.get("lifecycle_stage").unwrap(), "Load");
        assert_eq!(meta.get("lifecycle_sequence").unwrap(), "5");
    }

    #[test]
    fn test_lifecycle_not_tagged() {
        let code = r#"
Namespace Web
    Public Class MyPage
        Private Sub DoStuff()
        End Sub
    End Class
End Namespace
"#;
        let (symbols, _) = extract_vb(Path::new("MyPage.aspx.vb"), code);
        let do_stuff = symbols.iter().find(|s| s.name == "DoStuff").unwrap();
        let meta = do_stuff.metadata.as_ref().unwrap();
        assert!(meta.get("lifecycle_stage").is_none());
    }

    #[test]
    fn test_lifecycle_oninit_override() {
        let code = r#"
Namespace Web
    Public Class MyPage
        Protected Overrides Sub OnInit(e As EventArgs)
        End Sub
    End Class
End Namespace
"#;
        let (symbols, _) = extract_vb(Path::new("MyPage.aspx.vb"), code);
        let on_init = symbols.iter().find(|s| s.name == "OnInit").unwrap();
        let meta = on_init.metadata.as_ref().unwrap();
        assert_eq!(meta.get("lifecycle_stage").unwrap(), "Init");
        assert_eq!(meta.get("lifecycle_sequence").unwrap(), "2");
    }

    // ── ADO.NET column access tests ───────────────────────────────────────

    #[test]
    fn test_ado_row_indexer() {
        let code = r#"
Namespace Data
    Public Class OrderDal
        Public Sub LoadOrder()
            Dim name = row("CustomerName")
            Dim qty = dr("Quantity")
        End Sub
    End Class
End Namespace
"#;
        let (_, edges) = extract_vb(Path::new("OrderDal.vb"), code);
        let col_edges: Vec<_> = edges.iter().filter(|e| e.kind == "reads_column").collect();
        assert_eq!(col_edges.len(), 2, "Should find 2 column accesses");

        let names: Vec<_> = col_edges.iter().map(|e| e.target_name.as_str()).collect();
        assert!(names.contains(&"binding_field:CustomerName"));
        assert!(names.contains(&"binding_field:Quantity"));
    }

    #[test]
    fn test_ado_reader_ordinal() {
        let code = r#"
Namespace Data
    Public Class ReaderHelper
        Public Sub ReadData()
            Dim val = reader.GetString(reader.GetOrdinal("OrderDate"))
        End Sub
    End Class
End Namespace
"#;
        let (_, edges) = extract_vb(Path::new("ReaderHelper.vb"), code);
        let col_edges: Vec<_> = edges.iter().filter(|e| e.kind == "reads_column").collect();
        assert_eq!(col_edges.len(), 1);
        assert_eq!(col_edges[0].target_name, "binding_field:OrderDate");
        let meta = col_edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["access_pattern"], "ado_ordinal");
    }

    #[test]
    fn test_ado_item_access() {
        let code = r#"
Namespace Data
    Public Class ItemAccess
        Public Sub ReadRow()
            Dim v = dt.Rows(0).Item("ProductName")
        End Sub
    End Class
End Namespace
"#;
        let (_, edges) = extract_vb(Path::new("ItemAccess.vb"), code);
        let col_edges: Vec<_> = edges.iter().filter(|e| e.kind == "reads_column").collect();
        assert_eq!(col_edges.len(), 1);
        assert_eq!(col_edges[0].target_name, "binding_field:ProductName");
    }

    #[test]
    fn test_ado_scope_attribution() {
        let code = r#"
Namespace Data
    Public Class Scoped
        Public Sub LoadCustomer()
            Dim name = row("CustomerName")
        End Sub
        Public Sub LoadOrder()
            Dim id = row("OrderId")
        End Sub
    End Class
End Namespace
"#;
        let (_, edges) = extract_vb(Path::new("Scoped.vb"), code);
        let col_edges: Vec<_> = edges.iter().filter(|e| e.kind == "reads_column").collect();
        assert_eq!(col_edges.len(), 2);

        let cust = col_edges
            .iter()
            .find(|e| e.target_name == "binding_field:CustomerName")
            .unwrap();
        assert!(
            cust.source_name.contains("LoadCustomer"),
            "CustomerName should be attributed to LoadCustomer, got: {}",
            cust.source_name
        );

        let order = col_edges
            .iter()
            .find(|e| e.target_name == "binding_field:OrderId")
            .unwrap();
        assert!(
            order.source_name.contains("LoadOrder"),
            "OrderId should be attributed to LoadOrder, got: {}",
            order.source_name
        );
    }

    // ── Side-effect classification tests ──────────────────────────────────

    #[test]
    fn test_side_effect_ui_mutation() {
        let code = r#"
Namespace Web
    Public Class MyPage
        Protected Sub UpdateLabel()
            lblStatus.Text = "Saved"
            lblMessage.Visible = True
        End Sub
    End Class
End Namespace
"#;
        let (symbols, _) = extract_vb(Path::new("MyPage.aspx.vb"), code);
        let func = symbols.iter().find(|s| s.name == "UpdateLabel").unwrap();
        let meta = func.metadata.as_ref().unwrap();
        assert_eq!(meta.get("side_effects").unwrap(), "UI_Mutation");
    }

    #[test]
    fn test_side_effect_db_access() {
        let code = r#"
Namespace Data
    Public Class DataAccess
        Public Sub SaveRecord()
            Dim cmd As New SqlCommand("INSERT INTO Orders VALUES (@id)")
            cmd.ExecuteNonQuery()
        End Sub
    End Class
End Namespace
"#;
        let (symbols, _) = extract_vb(Path::new("DataAccess.vb"), code);
        let func = symbols.iter().find(|s| s.name == "SaveRecord").unwrap();
        let meta = func.metadata.as_ref().unwrap();
        assert!(
            meta.get("side_effects").unwrap().contains("DB_Access"),
            "Should tag as DB_Access"
        );
    }

    #[test]
    fn test_side_effect_combined() {
        let code = r#"
Namespace Web
    Public Class CombinedPage
        Protected Sub SaveAndUpdate()
            Dim cmd As New SqlCommand("UPDATE Orders SET Status = @s")
            cmd.ExecuteNonQuery()
            lblStatus.Text = "Saved"
            Session("LastSave") = DateTime.Now
        End Sub
    End Class
End Namespace
"#;
        let (symbols, _) = extract_vb(Path::new("Combined.aspx.vb"), code);
        let func = symbols.iter().find(|s| s.name == "SaveAndUpdate").unwrap();
        let meta = func.metadata.as_ref().unwrap();
        let effects = meta.get("side_effects").unwrap();
        assert!(
            effects.contains("UI_Mutation"),
            "Should contain UI_Mutation"
        );
        assert!(effects.contains("DB_Access"), "Should contain DB_Access");
        assert!(
            effects.contains("State_Access"),
            "Should contain State_Access"
        );
    }

    #[test]
    fn test_side_effect_none() {
        let code = r#"
Namespace Util
    Public Class Calculator
        Public Function Add(a As Integer, b As Integer) As Integer
            Return a + b
        End Function
    End Class
End Namespace
"#;
        let (symbols, _) = extract_vb(Path::new("Calculator.vb"), code);
        let func = symbols.iter().find(|s| s.name == "Add").unwrap();
        let meta = func.metadata.as_ref().unwrap();
        assert!(
            meta.get("side_effects").is_none(),
            "Pure computation should have no side_effects"
        );
    }

    // ── Script Injection Detection ───────────────────────────────────────

    #[test]
    fn test_register_startup_script() {
        let code = r#"
Imports System.Web.UI

Namespace MyApp
    Public Class Default1
        Inherits Page

        Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load
            Me.ClientScript.RegisterStartupScript(Me.GetType(), "mapInit", "initializeMap(40.7, -74.0);", True)
        End Sub
    End Class
End Namespace
"#;
        let (_, edges) = extract_vb(Path::new("Default.aspx.vb"), code);
        let injects: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "injects_script")
            .collect();
        assert!(
            !injects.is_empty(),
            "expected injects_script edges for RegisterStartupScript"
        );
        assert!(injects.iter().any(|e| e.target_name == "initializeMap"));
        let meta = injects[0].metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("injection_method").unwrap(),
            "RegisterStartupScript"
        );
    }

    #[test]
    fn test_script_manager_register() {
        let code = r#"
Imports System.Web.UI

Namespace MyApp
    Public Class Default2
        Inherits Page

        Protected Sub btnSave_Click(sender As Object, e As EventArgs)
            ScriptManager.RegisterStartupScript(Me, Me.GetType(), "alert", "showSaveConfirmation();", True)
        End Sub
    End Class
End Namespace
"#;
        let (_, edges) = extract_vb(Path::new("Form.aspx.vb"), code);
        let injects: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "injects_script")
            .collect();
        assert!(
            !injects.is_empty(),
            "expected injects_script edges for ScriptManager"
        );
        assert!(
            injects
                .iter()
                .any(|e| e.target_name == "showSaveConfirmation")
        );
    }

    #[test]
    fn test_script_injection_inline_no_function() {
        let code = r#"
Imports System.Web.UI

Namespace MyApp
    Public Class Default3
        Inherits Page

        Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load
            Me.ClientScript.RegisterStartupScript(Me.GetType(), "inline", "alert('hello');", True)
        End Sub
    End Class
End Namespace
"#;
        let (_, edges) = extract_vb(Path::new("Inline.aspx.vb"), code);
        let injects: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "injects_script")
            .collect();
        // Should still emit an edge even when only alert() is found (skipped as noise)
        // → falls back to inline_script: target
        assert!(
            !injects.is_empty(),
            "expected injects_script edge for inline script"
        );
    }
}
