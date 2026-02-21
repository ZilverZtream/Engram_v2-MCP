//! Scaffold generator service — produces compilable target-stack skeletons from
//! graph extraction data and the control mapping catalog.
//!
//! Given a file's extracted graph data (controls, events, data bindings, state
//! access, SQL calls) and a target stack (Blazor, React, Angular), generates a
//! component skeleton with correct imports, mapped controls, event handler stubs,
//! data access interface stubs (repository pattern), and state management hooks.

use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_index::control_mapping;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

/// Result from scaffold generation.
#[derive(Debug, Clone, Serialize)]
pub struct ScaffoldResult {
    /// The generated component/page code.
    pub component_code: String,
    /// Generated repository interface (if SQL edges found).
    pub repository_interface: Option<String>,
    /// Generated DTO classes (if column edges found).
    pub dto_classes: Option<String>,
    /// Generated test scaffold (if requested).
    pub test_scaffold: Option<String>,
    /// Mapping report: legacy element → modern element.
    pub mapping_report: Vec<MappingEntry>,
    /// Target stack used.
    pub target_stack: String,
    /// Warnings during generation.
    pub warnings: Vec<String>,
}

/// A single legacy→modern mapping line in the report.
#[derive(Debug, Clone, Serialize)]
pub struct MappingEntry {
    pub legacy_element: String,
    pub modern_element: String,
    pub category: String,
    pub notes: String,
}

/// Collected graph context for a single file.
#[allow(dead_code)]
struct FileContext {
    file_node: Option<Node>,
    controls: Vec<Node>,
    functions: Vec<Node>,
    sql_edges: Vec<Edge>,
    reads_state: Vec<Edge>,
    writes_state: Vec<Edge>,
    reads_column: Vec<Edge>,
    queries_table: Vec<Edge>,
    data_binding: Vec<Edge>,
    triggers_postback: Vec<Edge>,
    exposes_service: Vec<Edge>,
    connection_strings: Vec<Node>,
}

/// Generate a migration scaffold for the given file.
///
/// # Arguments
/// * `graph` — shared graph store
/// * `project_id` — project identifier
/// * `file_path` — the legacy file to scaffold from
/// * `target_stack` — "blazor", "react", or "angular"
/// * `include_tests` — whether to generate a test scaffold
/// * `output_format` — "full" or "diff"
pub fn generate_scaffold(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    target_stack: &str,
    include_tests: bool,
    _output_format: &str,
) -> anyhow::Result<ScaffoldResult> {
    let target = normalize_target(target_stack);
    let ctx = collect_file_context(graph, project_id, file_path)?;
    let mut warnings = Vec::new();
    let mut mapping_report = Vec::new();

    // ── Build component code ─────────────────────────────────────────────────
    let component_code = match target.as_str() {
        "blazor" => generate_blazor_component(&ctx, file_path, &mut mapping_report, &mut warnings),
        "react" => generate_react_component(&ctx, file_path, &mut mapping_report, &mut warnings),
        "angular" => {
            generate_angular_component(&ctx, file_path, &mut mapping_report, &mut warnings)
        }
        _ => {
            warnings.push(format!("Unknown target stack '{target}', defaulting to Blazor"));
            generate_blazor_component(&ctx, file_path, &mut mapping_report, &mut warnings)
        }
    };

    // ── Repository interface from SQL edges ───────────────────────────────────
    let repository_interface = generate_repository_interface(&ctx, &mut mapping_report);

    // ── DTO classes from column edges ─────────────────────────────────────────
    let dto_classes = generate_dto_classes(&ctx, &mut mapping_report);

    // ── Test scaffold ────────────────────────────────────────────────────────
    let test_scaffold = if include_tests {
        Some(generate_test_scaffold(&ctx, file_path, &target))
    } else {
        None
    };

    Ok(ScaffoldResult {
        component_code,
        repository_interface,
        dto_classes,
        test_scaffold,
        mapping_report,
        target_stack: target,
        warnings,
    })
}

fn normalize_target(t: &str) -> String {
    match t.to_lowercase().trim() {
        "blazor" | "blazor-server" | "blazor-wasm" => "blazor".into(),
        "react" | "reactjs" | "next" | "nextjs" => "react".into(),
        "angular" | "ng" => "angular".into(),
        other => other.to_string(),
    }
}

// ─── Graph context collection ─────────────────────────────────────────────────

fn collect_file_context(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<FileContext> {
    let file_node_id = format!("file:{file_path}");
    let file_node = graph.get_node(project_id, &file_node_id)?;

    // Collect control nodes for this file
    let all_controls = graph.query_nodes(project_id, Some("control"), None, None, 5000)?;
    let controls: Vec<Node> = all_controls
        .into_iter()
        .filter(|n| n.file_path.as_str() == file_path)
        .collect();

    // Collect function nodes for this file
    let all_fns = graph.query_nodes(project_id, Some("function"), None, None, 5000)?;
    let functions: Vec<Node> = all_fns
        .into_iter()
        .filter(|n| n.file_path.as_str() == file_path)
        .collect();

    // Edges by kind
    let sql_edges = collect_edges_for_file(graph, project_id, EdgeKind::SqlCalls, file_path)?;
    let reads_state = collect_edges_for_file(graph, project_id, EdgeKind::ReadsState, file_path)?;
    let writes_state =
        collect_edges_for_file(graph, project_id, EdgeKind::WritesState, file_path)?;
    let reads_column =
        collect_edges_for_file(graph, project_id, EdgeKind::ReadsColumn, file_path)?;
    let queries_table =
        collect_edges_for_file(graph, project_id, EdgeKind::QueriesTable, file_path)?;
    let data_binding =
        collect_edges_for_file(graph, project_id, EdgeKind::DataBinding, file_path)?;
    let triggers_postback =
        collect_edges_for_file(graph, project_id, EdgeKind::TriggersPostback, file_path)?;

    // Service exposure edges
    let mut exposes_service = Vec::new();
    for kind in [
        EdgeKind::ExposesWebService,
        EdgeKind::ExposesHttpHandler,
        EdgeKind::ExposesWcfService,
    ] {
        exposes_service.extend(collect_edges_for_file(graph, project_id, kind, file_path)?);
    }

    // Connection string nodes
    let all_conns = graph.query_nodes(project_id, Some("connection_string"), None, None, 500)?;
    let connection_strings: Vec<Node> = all_conns
        .into_iter()
        .filter(|n| n.file_path.as_str() == file_path)
        .collect();

    Ok(FileContext {
        file_node,
        controls,
        functions,
        sql_edges,
        reads_state,
        writes_state,
        reads_column,
        queries_table,
        data_binding,
        triggers_postback,
        exposes_service,
        connection_strings,
    })
}

fn collect_edges_for_file(
    graph: &Arc<GraphStore>,
    project_id: &str,
    kind: EdgeKind,
    file_path: &str,
) -> anyhow::Result<Vec<Edge>> {
    let all = graph.list_edges_by_kind(project_id, kind, 10_000)?;
    Ok(all
        .into_iter()
        .filter(|e| {
            e.source_id.contains(file_path) || e.metadata.as_ref().is_some_and(|m| {
                m.as_object()
                    .and_then(|o| o.get("file_path"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|fp| fp == file_path)
            })
        })
        .collect())
}

// ─── Blazor scaffold ──────────────────────────────────────────────────────────

fn generate_blazor_component(
    ctx: &FileContext,
    file_path: &str,
    mapping_report: &mut Vec<MappingEntry>,
    warnings: &mut Vec<String>,
) -> String {
    let page_name = extract_page_name(file_path);
    let mut code = String::with_capacity(4096);

    // Page directive
    let route = file_path
        .replace('\\', "/")
        .replace(".aspx", "")
        .replace(".ascx", "");
    let route = route.rsplit('/').next().unwrap_or(&route);
    let _ = writeln!(code, "@page \"/{route}\"");
    let _ = writeln!(code, "@using Microsoft.AspNetCore.Components");
    let _ = writeln!(code, "@using Microsoft.AspNetCore.Components.Web");

    // Inject services for data access
    if !ctx.sql_edges.is_empty() || !ctx.queries_table.is_empty() {
        let _ = writeln!(code, "@inject I{page_name}Repository Repository");
    }
    if !ctx.reads_state.is_empty() || !ctx.writes_state.is_empty() {
        let _ = writeln!(code, "@inject ISessionStateService SessionState");
    }
    let _ = writeln!(code);

    // Component body — mapped controls
    let _ = writeln!(code, "<h3>{page_name}</h3>");
    let _ = writeln!(code);

    for control in &ctx.controls {
        let legacy_name = &control.name;
        let control_type = extract_control_type(&control.node_id);
        if let Some(mapping) = control_mapping::lookup(&control_type) {
            let _ = writeln!(
                code,
                "<!-- Mapped from {legacy_name} ({control_type}) -->"
            );
            let _ = writeln!(code, "<!-- {}: {} -->", mapping.blazor_equivalent, mapping.notes);
            let _ = writeln!(code, "<{} />", simplify_blazor_tag(mapping.blazor_equivalent));
            let _ = writeln!(code);

            mapping_report.push(MappingEntry {
                legacy_element: format!("{control_type}#{legacy_name}"),
                modern_element: mapping.blazor_equivalent.to_string(),
                category: "control".into(),
                notes: mapping.data_binding_pattern.to_string(),
            });
        } else {
            let _ = writeln!(code, "<!-- TODO: No mapping for {control_type}#{legacy_name} -->");
            warnings.push(format!("No control mapping for '{control_type}'"));
        }
    }

    // State access — inject session helpers
    let state_keys = collect_state_keys(ctx);
    if !state_keys.is_empty() {
        let _ = writeln!(code);
        let _ = writeln!(code, "<!-- State management -->");
        for (key, is_write) in &state_keys {
            let op = if *is_write { "read/write" } else { "read-only" };
            let _ = writeln!(code, "<!-- Session[\"{key}\"] — {op} -->");
            mapping_report.push(MappingEntry {
                legacy_element: format!("Session[\"{key}\"]"),
                modern_element: format!("SessionState.Get<T>(\"{key}\")"),
                category: "state".into(),
                notes: format!("{op} access — consider JWT claim, Redis, or component state"),
            });
        }
    }

    // Code-behind section
    let _ = writeln!(code);
    let _ = writeln!(code, "@code {{");

    // Fields for state
    for (key, _) in &state_keys {
        let field_name = to_camel_case(key);
        let _ = writeln!(code, "    private string? {field_name};");
    }

    // OnInitializedAsync
    let _ = writeln!(code);
    let _ = writeln!(code, "    protected override async Task OnInitializedAsync()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        // TODO: migrate from Page_Load / Page_Init"
    );
    for (key, _) in &state_keys {
        let field_name = to_camel_case(key);
        let _ = writeln!(
            code,
            "        {field_name} = await SessionState.GetAsync<string>(\"{key}\");"
        );
    }
    let _ = writeln!(code, "        await base.OnInitializedAsync();");
    let _ = writeln!(code, "    }}");

    // Event handler stubs
    for func in &ctx.functions {
        let fname = &func.name;
        if fname.contains("_Click")
            || fname.contains("_Command")
            || fname.contains("_Changed")
            || fname.contains("_SelectedIndexChanged")
        {
            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "    /// TODO: migrate from {fname} (line {}-{})",
                func.start_line, func.end_line
            );
            let _ = writeln!(code, "    private async Task {fname}()");
            let _ = writeln!(code, "    {{");
            let _ = writeln!(
                code,
                "        // Original handler: {fname}"
            );

            // Note SQL calls from this handler
            for sql_edge in &ctx.sql_edges {
                if sql_edge.source_id.contains(fname) {
                    let _ = writeln!(
                        code,
                        "        // SQL: {} → use Repository method instead",
                        sql_edge.target_id
                    );
                }
            }

            let _ = writeln!(code, "        throw new NotImplementedException();");
            let _ = writeln!(code, "    }}");

            mapping_report.push(MappingEntry {
                legacy_element: fname.clone(),
                modern_element: format!("async Task {fname}()"),
                category: "event_handler".into(),
                notes: format!(
                    "Lines {}-{} in legacy code",
                    func.start_line, func.end_line
                ),
            });
        }
    }

    let _ = writeln!(code, "}}");
    code
}

// ─── React scaffold ───────────────────────────────────────────────────────────

fn generate_react_component(
    ctx: &FileContext,
    file_path: &str,
    mapping_report: &mut Vec<MappingEntry>,
    warnings: &mut Vec<String>,
) -> String {
    let page_name = extract_page_name(file_path);
    let mut code = String::with_capacity(4096);

    // Imports
    let _ = writeln!(code, "import React, {{ useState, useEffect }} from 'react';");
    if !ctx.sql_edges.is_empty() || !ctx.queries_table.is_empty() {
        let _ = writeln!(code, "import {{ use{page_name}Repository }} from '../hooks/use{page_name}Repository';");
    }
    let _ = writeln!(code);

    // Control-specific imports
    let mut react_imports: HashSet<String> = HashSet::new();
    for control in &ctx.controls {
        let control_type = extract_control_type(&control.node_id);
        if let Some(mapping) = control_mapping::lookup(&control_type) {
            if mapping.react_equivalent.contains('/') || mapping.react_equivalent.contains("@") {
                react_imports.insert(mapping.react_equivalent.to_string());
            }
        }
    }
    for imp in &react_imports {
        let _ = writeln!(code, "// TODO: install and import: {imp}");
    }
    if !react_imports.is_empty() {
        let _ = writeln!(code);
    }

    // Component
    let _ = writeln!(code, "export default function {page_name}() {{");

    // State hooks
    let state_keys = collect_state_keys(ctx);
    for (key, _) in &state_keys {
        let field_name = to_camel_case(key);
        let _ = writeln!(
            code,
            "  const [{field_name}, set{field_name}] = useState(null);"
        );
        mapping_report.push(MappingEntry {
            legacy_element: format!("Session[\"{key}\"]"),
            modern_element: format!("useState: {field_name}"),
            category: "state".into(),
            notes: "Consider server-side session, JWT, or context for cross-component state".into(),
        });
    }

    // Repository hook
    if !ctx.sql_edges.is_empty() {
        let _ = writeln!(
            code,
            "  const repository = use{page_name}Repository();"
        );
    }
    let _ = writeln!(code);

    // useEffect for init
    let _ = writeln!(code, "  useEffect(() => {{");
    let _ = writeln!(code, "    // TODO: migrate from Page_Load");
    let _ = writeln!(code, "    loadData();");
    let _ = writeln!(code, "  }}, []);");
    let _ = writeln!(code);

    // Handler stubs
    let _ = writeln!(code, "  const loadData = async () => {{");
    let _ = writeln!(code, "    // TODO: load initial data");
    let _ = writeln!(code, "  }};");

    for func in &ctx.functions {
        let fname = &func.name;
        if fname.contains("_Click")
            || fname.contains("_Command")
            || fname.contains("_Changed")
        {
            let handler_name = to_camel_case(fname);
            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "  // TODO: migrate from {fname} (line {}-{})",
                func.start_line, func.end_line
            );
            let _ = writeln!(code, "  const {handler_name} = async () => {{");
            let _ = writeln!(
                code,
                "    throw new Error('Not implemented: {fname}');"
            );
            let _ = writeln!(code, "  }};");

            mapping_report.push(MappingEntry {
                legacy_element: fname.clone(),
                modern_element: format!("const {handler_name}"),
                category: "event_handler".into(),
                notes: format!(
                    "Lines {}-{} in legacy code",
                    func.start_line, func.end_line
                ),
            });
        }
    }
    let _ = writeln!(code);

    // JSX
    let _ = writeln!(code, "  return (");
    let _ = writeln!(code, "    <div className=\"{page_name}\">");
    let _ = writeln!(code, "      <h3>{page_name}</h3>");

    for control in &ctx.controls {
        let legacy_name = &control.name;
        let control_type = extract_control_type(&control.node_id);
        if let Some(mapping) = control_mapping::lookup(&control_type) {
            let _ = writeln!(
                code,
                "      {{/* {control_type}#{legacy_name} → {} */}}",
                mapping.react_equivalent
            );
            let _ = writeln!(
                code,
                "      {{/* TODO: implement {legacy_name} using {} */}}",
                mapping.react_equivalent
            );

            mapping_report.push(MappingEntry {
                legacy_element: format!("{control_type}#{legacy_name}"),
                modern_element: mapping.react_equivalent.to_string(),
                category: "control".into(),
                notes: mapping.data_binding_pattern.to_string(),
            });
        } else {
            let _ = writeln!(
                code,
                "      {{/* TODO: No mapping for {control_type}#{legacy_name} */}}"
            );
            warnings.push(format!("No control mapping for '{control_type}'"));
        }
    }

    let _ = writeln!(code, "    </div>");
    let _ = writeln!(code, "  );");
    let _ = writeln!(code, "}}");
    code
}

// ─── Angular scaffold ─────────────────────────────────────────────────────────

fn generate_angular_component(
    ctx: &FileContext,
    file_path: &str,
    mapping_report: &mut Vec<MappingEntry>,
    warnings: &mut Vec<String>,
) -> String {
    let page_name = extract_page_name(file_path);
    let class_name = to_pascal_case(&page_name);
    let selector = to_kebab_case(&page_name);
    let mut code = String::with_capacity(4096);

    // Imports
    let _ = writeln!(
        code,
        "import {{ Component, OnInit }} from '@angular/core';"
    );
    if !ctx.sql_edges.is_empty() || !ctx.queries_table.is_empty() {
        let _ = writeln!(
            code,
            "import {{ {class_name}Service }} from './{selector}.service';"
        );
    }
    let _ = writeln!(code);

    // Component decorator
    let _ = writeln!(code, "@Component({{");
    let _ = writeln!(code, "  selector: 'app-{selector}',");
    let _ = writeln!(code, "  templateUrl: './{selector}.component.html',");
    let _ = writeln!(code, "  styleUrls: ['./{selector}.component.css']");
    let _ = writeln!(code, "}})");
    let _ = writeln!(
        code,
        "export class {class_name}Component implements OnInit {{"
    );

    // Properties from state
    let state_keys = collect_state_keys(ctx);
    for (key, _) in &state_keys {
        let field_name = to_camel_case(key);
        let _ = writeln!(code, "  {field_name}: string | null = null;");
        mapping_report.push(MappingEntry {
            legacy_element: format!("Session[\"{key}\"]"),
            modern_element: format!("{field_name}: string"),
            category: "state".into(),
            notes: "Consider NgRx, service state, or route params".into(),
        });
    }
    let _ = writeln!(code);

    // Constructor
    if !ctx.sql_edges.is_empty() || !ctx.queries_table.is_empty() {
        let _ = writeln!(
            code,
            "  constructor(private service: {class_name}Service) {{}}"
        );
    } else {
        let _ = writeln!(code, "  constructor() {{}}");
    }
    let _ = writeln!(code);

    // ngOnInit
    let _ = writeln!(code, "  ngOnInit(): void {{");
    let _ = writeln!(code, "    // TODO: migrate from Page_Load");
    let _ = writeln!(code, "    this.loadData();");
    let _ = writeln!(code, "  }}");
    let _ = writeln!(code);

    let _ = writeln!(code, "  private loadData(): void {{");
    let _ = writeln!(code, "    // TODO: load initial data");
    let _ = writeln!(code, "  }}");

    // Event handler stubs
    for func in &ctx.functions {
        let fname = &func.name;
        if fname.contains("_Click")
            || fname.contains("_Command")
            || fname.contains("_Changed")
        {
            let method_name = to_camel_case(fname);
            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "  /** TODO: migrate from {fname} (line {}-{}) */",
                func.start_line, func.end_line
            );
            let _ = writeln!(code, "  {method_name}(): void {{");
            let _ = writeln!(
                code,
                "    throw new Error('Not implemented: {fname}');"
            );
            let _ = writeln!(code, "  }}");

            mapping_report.push(MappingEntry {
                legacy_element: fname.clone(),
                modern_element: format!("{method_name}()"),
                category: "event_handler".into(),
                notes: format!(
                    "Lines {}-{} in legacy code",
                    func.start_line, func.end_line
                ),
            });
        }
    }

    let _ = writeln!(code, "}}");

    // Template
    let _ = writeln!(code);
    let _ = writeln!(code, "<!-- {selector}.component.html -->");
    let _ = writeln!(code, "<div>");
    let _ = writeln!(code, "  <h3>{page_name}</h3>");

    for control in &ctx.controls {
        let legacy_name = &control.name;
        let control_type = extract_control_type(&control.node_id);
        if let Some(mapping) = control_mapping::lookup(&control_type) {
            let _ = writeln!(
                code,
                "  <!-- {control_type}#{legacy_name} → {} -->",
                mapping.angular_equivalent
            );

            mapping_report.push(MappingEntry {
                legacy_element: format!("{control_type}#{legacy_name}"),
                modern_element: mapping.angular_equivalent.to_string(),
                category: "control".into(),
                notes: mapping.data_binding_pattern.to_string(),
            });
        } else {
            let _ = writeln!(
                code,
                "  <!-- TODO: No mapping for {control_type}#{legacy_name} -->"
            );
            warnings.push(format!("No control mapping for '{control_type}'"));
        }
    }

    let _ = writeln!(code, "</div>");
    code
}

// ─── Repository interface generation ──────────────────────────────────────────

fn generate_repository_interface(
    ctx: &FileContext,
    mapping_report: &mut Vec<MappingEntry>,
) -> Option<String> {
    if ctx.sql_edges.is_empty() && ctx.queries_table.is_empty() {
        return None;
    }

    let page_name = ctx
        .file_node
        .as_ref()
        .map(|n| extract_page_name(n.file_path.as_str()))
        .unwrap_or_else(|| "Data".into());

    let mut code = String::with_capacity(2048);
    let _ = writeln!(code, "using System.Threading.Tasks;");
    let _ = writeln!(code, "using System.Collections.Generic;");
    let _ = writeln!(code);

    // Collect table operations
    let mut table_ops: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for edge in &ctx.queries_table {
        let table = edge
            .target_id
            .strip_prefix("table:")
            .unwrap_or(&edge.target_id);
        table_ops
            .entry(table.to_string())
            .or_default()
            .insert("query".into());
    }
    for edge in &ctx.sql_edges {
        let sql_hint = edge
            .metadata
            .as_ref()
            .and_then(|m| m.get("sql"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let upper = sql_hint.to_uppercase();
        if upper.contains("INSERT") {
            for (_, ops) in table_ops.iter_mut() {
                ops.insert("insert".into());
            }
        }
        if upper.contains("UPDATE") {
            for (_, ops) in table_ops.iter_mut() {
                ops.insert("update".into());
            }
        }
        if upper.contains("DELETE") {
            for (_, ops) in table_ops.iter_mut() {
                ops.insert("delete".into());
            }
        }
    }

    // Generate interface
    let _ = writeln!(code, "public interface I{page_name}Repository");
    let _ = writeln!(code, "{{");

    for (table, ops) in &table_ops {
        let entity = to_pascal_case(table);
        if ops.contains("query") {
            let _ = writeln!(
                code,
                "    Task<IEnumerable<{entity}>> GetAll{entity}Async();"
            );
            let _ = writeln!(
                code,
                "    Task<{entity}?> Get{entity}ByIdAsync(int id);"
            );
        }
        if ops.contains("insert") {
            let _ = writeln!(
                code,
                "    Task<int> Create{entity}Async({entity} entity);"
            );
        }
        if ops.contains("update") {
            let _ = writeln!(
                code,
                "    Task Update{entity}Async({entity} entity);"
            );
        }
        if ops.contains("delete") {
            let _ = writeln!(
                code,
                "    Task Delete{entity}Async(int id);"
            );
        }

        mapping_report.push(MappingEntry {
            legacy_element: format!("Inline SQL → {table}"),
            modern_element: format!("I{page_name}Repository.{entity}*Async()"),
            category: "data_access".into(),
            notes: "Repository pattern with Dapper/EF Core".into(),
        });
    }

    let _ = writeln!(code, "}}");
    Some(code)
}

// ─── DTO generation ───────────────────────────────────────────────────────────

fn generate_dto_classes(
    ctx: &FileContext,
    mapping_report: &mut Vec<MappingEntry>,
) -> Option<String> {
    if ctx.reads_column.is_empty() && ctx.queries_table.is_empty() {
        return None;
    }

    let mut code = String::with_capacity(1024);
    let mut table_columns: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for edge in &ctx.reads_column {
        let col_name = edge
            .target_id
            .strip_prefix("col:")
            .unwrap_or(&edge.target_id);
        // Try to find parent table from source
        let table = edge
            .metadata
            .as_ref()
            .and_then(|m| m.get("table"))
            .and_then(|v| v.as_str())
            .unwrap_or("UnknownTable");
        table_columns
            .entry(table.to_string())
            .or_default()
            .push(col_name.to_string());
    }

    // Fallback: use queries_table edges if no column data
    if table_columns.is_empty() {
        for edge in &ctx.queries_table {
            let table = edge
                .target_id
                .strip_prefix("table:")
                .unwrap_or(&edge.target_id);
            table_columns.entry(table.to_string()).or_default();
        }
    }

    for (table, columns) in &table_columns {
        let class_name = to_pascal_case(table);
        let _ = writeln!(code, "public class {class_name}");
        let _ = writeln!(code, "{{");

        if columns.is_empty() {
            let _ = writeln!(code, "    // TODO: add properties from schema");
        } else {
            for col in columns {
                let prop_name = to_pascal_case(col);
                let prop_type = infer_csharp_type(col);
                let _ = writeln!(
                    code,
                    "    public {prop_type} {prop_name} {{ get; set; }}"
                );
            }
        }

        let _ = writeln!(code, "}}");
        let _ = writeln!(code);

        mapping_report.push(MappingEntry {
            legacy_element: format!("DataReader/DataSet → {table}"),
            modern_element: format!("class {class_name}"),
            category: "dto".into(),
            notes: "Strongly-typed DTO replacing DataRow/DataTable".into(),
        });
    }

    Some(code)
}

/// Infer a C# type from a column name using naming conventions.
fn infer_csharp_type(col_name: &str) -> &'static str {
    let lower = col_name.to_lowercase();
    if lower.ends_with("id") {
        "int"
    } else if lower.ends_with("name")
        || lower.ends_with("description")
        || lower.ends_with("title")
        || lower.ends_with("email")
        || lower.ends_with("address")
        || lower.ends_with("text")
        || lower.ends_with("url")
    {
        "string"
    } else if lower.ends_with("date")
        || lower.ends_with("time")
        || lower.starts_with("created")
        || lower.starts_with("modified")
        || lower.starts_with("updated")
    {
        "DateTime"
    } else if lower.ends_with("amount")
        || lower.ends_with("total")
        || lower.ends_with("price")
        || lower.ends_with("cost")
        || lower.ends_with("balance")
    {
        "decimal"
    } else if lower.starts_with("is") || lower.starts_with("has") || lower.starts_with("can") {
        "bool"
    } else if lower.ends_with("count") || lower.ends_with("quantity") || lower.ends_with("number")
    {
        "int"
    } else {
        "string"
    }
}

// ─── Test scaffold ────────────────────────────────────────────────────────────

fn generate_test_scaffold(ctx: &FileContext, file_path: &str, target: &str) -> String {
    let page_name = extract_page_name(file_path);
    let mut code = String::with_capacity(2048);

    match target {
        "blazor" => {
            let _ = writeln!(code, "using Bunit;");
            let _ = writeln!(code, "using NUnit.Framework;");
            let _ = writeln!(code, "using Moq;");
            let _ = writeln!(code);
            let _ = writeln!(code, "[TestFixture]");
            let _ = writeln!(code, "public class {page_name}Tests : TestContext");
            let _ = writeln!(code, "{{");
            let _ = writeln!(code, "    [SetUp]");
            let _ = writeln!(code, "    public void Setup()");
            let _ = writeln!(code, "    {{");
            if !ctx.sql_edges.is_empty() {
                let _ = writeln!(
                    code,
                    "        Services.AddSingleton(Mock.Of<I{page_name}Repository>());"
                );
            }
            let _ = writeln!(code, "    }}");
            let _ = writeln!(code);
            let _ = writeln!(code, "    [Test]");
            let _ = writeln!(code, "    public void Should_Render_Without_Error()");
            let _ = writeln!(code, "    {{");
            let _ = writeln!(
                code,
                "        var cut = RenderComponent<{page_name}>();"
            );
            let _ = writeln!(code, "        Assert.That(cut.Markup, Does.Contain(\"{page_name}\"));");
            let _ = writeln!(code, "    }}");
            let _ = writeln!(code, "}}");
        }
        "react" => {
            let _ = writeln!(code, "import {{ render, screen }} from '@testing-library/react';");
            let _ = writeln!(code, "import {page_name} from './{page_name}';");
            let _ = writeln!(code);
            let _ = writeln!(code, "describe('{page_name}', () => {{");
            let _ = writeln!(code, "  it('renders without crashing', () => {{");
            let _ = writeln!(code, "    render(<{page_name} />);");
            let _ = writeln!(code, "    expect(screen.getByText('{page_name}')).toBeInTheDocument();");
            let _ = writeln!(code, "  }});");
            let _ = writeln!(code, "}});");
        }
        "angular" => {
            let class_name = to_pascal_case(&page_name);
            let selector = to_kebab_case(&page_name);
            let _ = writeln!(code, "import {{ ComponentFixture, TestBed }} from '@angular/core/testing';");
            let _ = writeln!(
                code,
                "import {{ {class_name}Component }} from './{selector}.component';"
            );
            let _ = writeln!(code);
            let _ = writeln!(code, "describe('{class_name}Component', () => {{");
            let _ = writeln!(
                code,
                "  let component: {class_name}Component;"
            );
            let _ = writeln!(
                code,
                "  let fixture: ComponentFixture<{class_name}Component>;"
            );
            let _ = writeln!(code);
            let _ = writeln!(code, "  beforeEach(async () => {{");
            let _ = writeln!(code, "    await TestBed.configureTestingModule({{");
            let _ = writeln!(
                code,
                "      declarations: [{class_name}Component]"
            );
            let _ = writeln!(code, "    }}).compileComponents();");
            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "    fixture = TestBed.createComponent({class_name}Component);"
            );
            let _ = writeln!(code, "    component = fixture.componentInstance;");
            let _ = writeln!(code, "    fixture.detectChanges();");
            let _ = writeln!(code, "  }});");
            let _ = writeln!(code);
            let _ = writeln!(code, "  it('should create', () => {{");
            let _ = writeln!(code, "    expect(component).toBeTruthy();");
            let _ = writeln!(code, "  }});");
            let _ = writeln!(code, "}});");
        }
        _ => {}
    }

    code
}

// ─── Helper utilities ─────────────────────────────────────────────────────────

fn extract_page_name(file_path: &str) -> String {
    let file_name = file_path
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(file_path)
        .to_string();
    file_name
        .replace(".aspx.cs", "")
        .replace(".aspx.vb", "")
        .replace(".aspx", "")
        .replace(".ascx.cs", "")
        .replace(".ascx.vb", "")
        .replace(".ascx", "")
        .replace(".cs", "")
        .replace(".vb", "")
}

fn extract_control_type(node_id: &str) -> String {
    // node_id format: "control:GridView1" or "control:asp:GridView#GridView1"
    let id = node_id.strip_prefix("control:").unwrap_or(node_id);
    // Try to extract the asp: type
    if let Some(rest) = id.strip_prefix("asp:") {
        if let Some(pos) = rest.find('#') {
            return format!("asp:{}", &rest[..pos]);
        }
        return format!("asp:{rest}");
    }
    id.split('#').next().unwrap_or(id).to_string()
}

fn simplify_blazor_tag(component: &str) -> String {
    // "QuickGrid<T>" → "QuickGrid"
    component.split('<').next().unwrap_or(component).to_string()
}

fn collect_state_keys(ctx: &FileContext) -> Vec<(String, bool)> {
    let mut keys: HashMap<String, bool> = HashMap::new();
    for edge in &ctx.reads_state {
        let key = edge
            .target_id
            .strip_prefix("state:")
            .unwrap_or(&edge.target_id);
        keys.entry(key.to_string()).or_insert(false);
    }
    for edge in &ctx.writes_state {
        let key = edge
            .target_id
            .strip_prefix("state:")
            .unwrap_or(&edge.target_id);
        keys.insert(key.to_string(), true);
    }
    let mut sorted: Vec<_> = keys.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted
}

fn to_camel_case(s: &str) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let mut result = String::new();
    let mut capitalize_next = false;
    for (i, c) in clean.chars().enumerate() {
        if c == '_' {
            capitalize_next = true;
            continue;
        }
        if i == 0 {
            result.push(c.to_lowercase().next().unwrap_or(c));
        } else if capitalize_next {
            result.push(c.to_uppercase().next().unwrap_or(c));
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

fn to_pascal_case(s: &str) -> String {
    let camel = to_camel_case(s);
    let mut chars = camel.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

fn to_kebab_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            result.push('-');
        }
        result.push(c.to_lowercase().next().unwrap_or(c));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_target_variants() {
        assert_eq!(normalize_target("blazor"), "blazor");
        assert_eq!(normalize_target("Blazor"), "blazor");
        assert_eq!(normalize_target("react"), "react");
        assert_eq!(normalize_target("ReactJS"), "react");
        assert_eq!(normalize_target("angular"), "angular");
        assert_eq!(normalize_target("ng"), "angular");
        assert_eq!(normalize_target("vue"), "vue");
    }

    #[test]
    fn extract_page_name_variants() {
        assert_eq!(extract_page_name("Orders.aspx"), "Orders");
        assert_eq!(extract_page_name("Orders.aspx.cs"), "Orders");
        assert_eq!(extract_page_name("Admin/Users.aspx.vb"), "Users");
        assert_eq!(
            extract_page_name("Controls/HeaderControl.ascx"),
            "HeaderControl"
        );
    }

    #[test]
    fn extract_control_type_from_node_id() {
        assert_eq!(
            extract_control_type("control:asp:GridView#GridView1"),
            "asp:GridView"
        );
        assert_eq!(
            extract_control_type("control:asp:TextBox"),
            "asp:TextBox"
        );
        assert_eq!(extract_control_type("control:MyCustom"), "MyCustom");
    }

    #[test]
    fn infer_csharp_types() {
        assert_eq!(infer_csharp_type("OrderId"), "int");
        assert_eq!(infer_csharp_type("CustomerName"), "string");
        assert_eq!(infer_csharp_type("CreatedDate"), "DateTime");
        assert_eq!(infer_csharp_type("TotalAmount"), "decimal");
        assert_eq!(infer_csharp_type("IsActive"), "bool");
        assert_eq!(infer_csharp_type("ItemCount"), "int");
        assert_eq!(infer_csharp_type("Data"), "string");
    }

    #[test]
    fn to_camel_case_variants() {
        assert_eq!(to_camel_case("Button_Click"), "buttonClick");
        assert_eq!(to_camel_case("UserId"), "userId");
        assert_eq!(to_camel_case("some_thing"), "someThing");
    }

    #[test]
    fn to_pascal_case_variants() {
        assert_eq!(to_pascal_case("orders"), "Orders");
        assert_eq!(to_pascal_case("user_profile"), "UserProfile");
    }

    #[test]
    fn to_kebab_case_variants() {
        assert_eq!(to_kebab_case("OrderList"), "order-list");
        assert_eq!(to_kebab_case("UserProfile"), "user-profile");
    }

    #[test]
    fn simplify_blazor_tag_removes_generic() {
        assert_eq!(simplify_blazor_tag("QuickGrid<T>"), "QuickGrid");
        assert_eq!(simplify_blazor_tag("InputText"), "InputText");
    }

    #[test]
    fn collect_state_keys_deduplicates() {
        let ctx = FileContext {
            file_node: None,
            controls: vec![],
            functions: vec![],
            sql_edges: vec![],
            reads_state: vec![
                Edge {
                    source_id: "fn:Page_Load".into(),
                    target_id: "state:UserId".into(),
                    namespace: "test".into(),
                    language: "csharp".into(),
                    edge_kind: EdgeKind::ReadsState,
                    weight: 1,
                    generation: 1,
                    metadata: None,
                    updated_at_ms: 0,
                },
                Edge {
                    source_id: "fn:Page_Load".into(),
                    target_id: "state:UserName".into(),
                    namespace: "test".into(),
                    language: "csharp".into(),
                    edge_kind: EdgeKind::ReadsState,
                    weight: 1,
                    generation: 1,
                    metadata: None,
                    updated_at_ms: 0,
                },
            ],
            writes_state: vec![Edge {
                source_id: "fn:Login_Click".into(),
                target_id: "state:UserId".into(),
                namespace: "test".into(),
                language: "csharp".into(),
                edge_kind: EdgeKind::WritesState,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }],
            reads_column: vec![],
            queries_table: vec![],
            data_binding: vec![],
            triggers_postback: vec![],
            exposes_service: vec![],
            connection_strings: vec![],
        };

        let keys = collect_state_keys(&ctx);
        assert_eq!(keys.len(), 2);
        // UserId should be write (true) because writes_state overrides
        let user_id = keys.iter().find(|(k, _)| k == "UserId");
        assert!(user_id.is_some_and(|(_, w)| *w));
        // UserName should be read-only (false)
        let user_name = keys.iter().find(|(k, _)| k == "UserName");
        assert!(user_name.is_some_and(|(_, w)| !*w));
    }
}
