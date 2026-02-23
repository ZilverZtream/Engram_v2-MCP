// Ticket 4: ViewState Dependency Analysis Service
//
// Analyzes both explicit ViewState["key"] usage and implicit ViewState
// consumed by WebForms controls (GridView sort/page state, DropDownList
// selection, etc.). Maps to modern component state equivalents.

use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

// ── Result structs ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ViewStateDependencyReport {
    pub file_path: String,
    pub explicit_viewstate: Vec<ExplicitViewStateEntry>,
    pub implicit_viewstate: Vec<ImplicitViewStateEntry>,
    pub viewstate_disabled_controls: Vec<String>,
    pub page_level_viewstate: Option<bool>,
    pub heaviest_controls: Vec<(String, String, String)>,
    pub total_state_fields: usize,
    pub migration_complexity: String,
    pub modern_state_model: Vec<StateFieldRecommendation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplicitViewStateEntry {
    pub key: String,
    pub data_type_guess: String,
    pub readers: Vec<String>,
    pub writers: Vec<String>,
    pub lifecycle: String,
    pub modern_replacement: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImplicitViewStateEntry {
    pub control_id: String,
    pub control_type: String,
    pub properties_persisted: Vec<String>,
    pub estimated_size_impact: String,
    pub modern_replacement: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateFieldRecommendation {
    pub field_name: String,
    pub source: String,
    pub blazor_declaration: String,
    pub react_declaration: String,
    pub persist_across: String,
}

// ── Implicit ViewState knowledge base ─────────────────────────────────────

struct ImplicitVSProperties {
    control_type: &'static str,
    properties: &'static [&'static str],
    size_impact: &'static str,
    modern_state: &'static str,
}

const IMPLICIT_VIEWSTATE: &[ImplicitVSProperties] = &[
    ImplicitVSProperties {
        control_type: "GridView",
        properties: &[
            "SortExpression",
            "SortDirection",
            "PageIndex",
            "PageCount",
            "SelectedIndex",
            "EditIndex",
            "DataKeys",
            "Columns (generated)",
            "RowState (editing/selected)",
        ],
        size_impact: "High (grows with data rows and columns)",
        modern_state: "private string sortColumn; private bool sortAscending; private int currentPage; private int? selectedIndex; private int? editIndex;",
    },
    ImplicitVSProperties {
        control_type: "DropDownList",
        properties: &["SelectedValue", "SelectedIndex", "Items (if data-bound)"],
        size_impact: "Medium (all items serialized if data-bound)",
        modern_state: "private string selectedValue; // Bind items from service, not ViewState",
    },
    ImplicitVSProperties {
        control_type: "ListView",
        properties: &[
            "EditIndex",
            "SelectedIndex",
            "InsertItemPosition",
            "DataKeys",
            "SortExpression",
            "SortDirection",
        ],
        size_impact: "High (similar to GridView)",
        modern_state: "private int? editIndex; private int? selectedIndex; private string sortColumn;",
    },
    ImplicitVSProperties {
        control_type: "FormView",
        properties: &["CurrentMode", "DefaultMode", "DataKey", "PageIndex"],
        size_impact: "Low-Medium",
        modern_state: "private FormMode currentMode; private int pageIndex;",
    },
    ImplicitVSProperties {
        control_type: "DetailsView",
        properties: &["CurrentMode", "DefaultMode", "DataKey", "PageIndex"],
        size_impact: "Low-Medium",
        modern_state: "private FormMode currentMode; private int pageIndex;",
    },
    ImplicitVSProperties {
        control_type: "TreeView",
        properties: &[
            "SelectedValue",
            "ExpandedNodes",
            "CheckedNodes",
            "SelectedNode",
            "Nodes (if populated in code)",
        ],
        size_impact: "High (all expanded/checked state for entire tree)",
        modern_state: "private HashSet<string> expandedNodes; private HashSet<string> checkedNodes; private string? selectedNodeId;",
    },
    ImplicitVSProperties {
        control_type: "Calendar",
        properties: &["SelectedDate", "SelectedDates", "VisibleDate"],
        size_impact: "Low",
        modern_state: "private DateTime? selectedDate; private DateTime visibleDate;",
    },
    ImplicitVSProperties {
        control_type: "TextBox",
        properties: &["Text"],
        size_impact: "Low (but multiplied by number of textboxes)",
        modern_state: "private string fieldValue = \"\"; // @bind-Value",
    },
    ImplicitVSProperties {
        control_type: "CheckBox",
        properties: &["Checked"],
        size_impact: "Low",
        modern_state: "private bool isChecked; // @bind-Value",
    },
    ImplicitVSProperties {
        control_type: "RadioButton",
        properties: &["Checked", "GroupName"],
        size_impact: "Low",
        modern_state: "private string selectedRadio; // @bind for radio group",
    },
    ImplicitVSProperties {
        control_type: "RadioButtonList",
        properties: &["SelectedValue", "SelectedIndex", "Items"],
        size_impact: "Medium",
        modern_state: "private string selectedValue;",
    },
    ImplicitVSProperties {
        control_type: "CheckBoxList",
        properties: &["Items (with Selected state for each)"],
        size_impact: "Medium",
        modern_state: "private HashSet<string> selectedItems = new();",
    },
    ImplicitVSProperties {
        control_type: "ListBox",
        properties: &["SelectedValue", "SelectedIndex", "Items"],
        size_impact: "Medium (all items if data-bound)",
        modern_state: "private string selectedValue; // or List<string> for multi-select",
    },
    ImplicitVSProperties {
        control_type: "HiddenField",
        properties: &["Value"],
        size_impact: "Low",
        modern_state: "private string hiddenValue; // component state or @bind to hidden input",
    },
    ImplicitVSProperties {
        control_type: "Label",
        properties: &["Text"],
        size_impact: "Low (but unnecessary — Labels rarely need ViewState)",
        modern_state: "// Labels don't need state — render directly from model",
    },
    ImplicitVSProperties {
        control_type: "Panel",
        properties: &["Visible"],
        size_impact: "Low",
        modern_state: "private bool isPanelVisible = true; // @if (isPanelVisible) { ... }",
    },
    ImplicitVSProperties {
        control_type: "MultiView",
        properties: &["ActiveViewIndex"],
        size_impact: "Low",
        modern_state: "private int activeTab = 0;",
    },
    ImplicitVSProperties {
        control_type: "Wizard",
        properties: &["ActiveStepIndex", "History"],
        size_impact: "Medium",
        modern_state: "private int currentStep = 0; private List<int> stepHistory = new();",
    },
    ImplicitVSProperties {
        control_type: "Repeater",
        properties: &["Items (data-bound only, no paging state)"],
        size_impact: "Low (Repeater has minimal ViewState)",
        modern_state: "// Repeater equivalent: @foreach with component state for items",
    },
    ImplicitVSProperties {
        control_type: "DataList",
        properties: &["SelectedIndex", "EditItemIndex", "Items"],
        size_impact: "Medium",
        modern_state: "private int? selectedIndex; private int? editIndex;",
    },
];

// ── Regex patterns ────────────────────────────────────────────────────────

static RE_EXPLICIT_VS_CS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"ViewState\s*\[\s*"([^"]*)"\s*\]"#).unwrap());

static RE_EXPLICIT_VS_VB: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"ViewState\s*\(\s*"([^"]*)"\s*\)"#).unwrap());

static RE_CONTROL_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<asp:(\w+)\b[^>]*\bID\s*=\s*"([^"]*)"[^>]*>"#).unwrap());

static RE_CONTROL_TAG_SELFCLOSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<asp:(\w+)\b[^>]*\bID\s*=\s*"([^"]*)"[^>]*/>"#).unwrap());

static RE_ENABLE_VS_CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)<asp:\w+\b[^>]*\bID\s*=\s*"([^"]*)"[^>]*\bEnableViewState\s*=\s*"false"[^>]*/?>"#,
    )
    .unwrap()
});

static RE_PAGE_VS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<%@\s+(?:Page|Control)\b[^%]*EnableViewState\s*=\s*"(true|false)""#).unwrap()
});

static RE_METHOD_CONTEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)(?:Sub|void|Function|Task)\s+(\w+)\s*\(").unwrap());

// ── Main analysis function ────────────────────────────────────────────────

pub fn analyze_viewstate_dependencies(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    codebehind_content: &str,
    aspx_content: Option<&str>,
) -> anyhow::Result<ViewStateDependencyReport> {
    let is_vb = file_path.ends_with(".vb");
    let mut explicit_viewstate = Vec::new();
    let mut implicit_viewstate = Vec::new();
    let mut viewstate_disabled = Vec::new();
    let mut heaviest_controls = Vec::new();
    let mut modern_state_model = Vec::new();

    // ── Parse explicit ViewState["key"] usage ──

    let vs_re = if is_vb {
        &*RE_EXPLICIT_VS_VB
    } else {
        &*RE_EXPLICIT_VS_CS
    };

    let mut key_map: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();

    // Build method context map for line → method name resolution
    let method_starts = build_method_map(codebehind_content);

    for (line_num, line) in codebehind_content.lines().enumerate() {
        for cap in vs_re.captures_iter(line) {
            let key = cap[1].to_string();
            let method = find_enclosing_method(&method_starts, line_num + 1);

            let entry = key_map
                .entry(key.clone())
                .or_insert_with(|| (vec![], vec![]));

            // Detect read vs write
            let trimmed = line.trim();
            let is_write = is_viewstate_write(trimmed, &key, is_vb);

            if is_write {
                if !entry.1.contains(&method) {
                    entry.1.push(method);
                }
            } else if !entry.0.contains(&method) {
                entry.0.push(method);
            }
        }
    }

    // Also pull from graph state edges
    if let Ok(reads) = graph.list_edges_by_kind(project_id, EdgeKind::ReadsState, 10_000) {
        for edge in &reads {
            if edge.target_id.contains("ViewState:") {
                let key = edge
                    .target_id
                    .strip_prefix("state:ViewState:")
                    .unwrap_or(&edge.target_id)
                    .to_string();
                let entry = key_map.entry(key).or_insert_with(|| (vec![], vec![]));
                if !entry.0.contains(&edge.source_id) {
                    entry.0.push(edge.source_id.clone());
                }
            }
        }
    }
    if let Ok(writes) = graph.list_edges_by_kind(project_id, EdgeKind::WritesState, 10_000) {
        for edge in &writes {
            if edge.target_id.contains("ViewState:") {
                let key = edge
                    .target_id
                    .strip_prefix("state:ViewState:")
                    .unwrap_or(&edge.target_id)
                    .to_string();
                let entry = key_map.entry(key).or_insert_with(|| (vec![], vec![]));
                if !entry.1.contains(&edge.source_id) {
                    entry.1.push(edge.source_id.clone());
                }
            }
        }
    }

    for (key, (readers, writers)) in &key_map {
        let dtype = infer_viewstate_type(key);
        let lifecycle = classify_viewstate_lifecycle(readers, writers);
        let modern = format!(
            "private {} {} = default; // component state",
            dtype,
            to_camel_case(key)
        );

        explicit_viewstate.push(ExplicitViewStateEntry {
            key: key.clone(),
            data_type_guess: dtype,
            readers: readers.clone(),
            writers: writers.clone(),
            lifecycle,
            modern_replacement: modern.clone(),
        });

        modern_state_model.push(StateFieldRecommendation {
            field_name: to_camel_case(key),
            source: format!("ViewState[\"{key}\"]"),
            blazor_declaration: modern,
            react_declaration: format!(
                "const [{}, set{}] = useState(null);",
                to_camel_case(key),
                capitalize(key)
            ),
            persist_across: "Component re-renders (no persistence needed if page doesn't reload)"
                .to_string(),
        });
    }

    // ── Parse implicit ViewState from ASPX controls ──

    if let Some(aspx) = aspx_content {
        // Find controls with EnableViewState="false"
        for cap in RE_ENABLE_VS_CONTROL.captures_iter(aspx) {
            viewstate_disabled.push(cap[1].to_string());
        }

        // Find all ASP.NET controls
        let mut controls: Vec<(String, String)> = Vec::new(); // (type, id)
        for cap in RE_CONTROL_TAG.captures_iter(aspx) {
            controls.push((cap[1].to_string(), cap[2].to_string()));
        }
        for cap in RE_CONTROL_TAG_SELFCLOSE.captures_iter(aspx) {
            controls.push((cap[1].to_string(), cap[2].to_string()));
        }

        // Deduplicate
        controls.sort_by(|a, b| a.1.cmp(&b.1));
        controls.dedup_by(|a, b| a.1 == b.1);

        for (ctype, cid) in &controls {
            // Skip if ViewState is disabled for this control
            if viewstate_disabled.contains(cid) {
                continue;
            }

            if let Some(implicit) = IMPLICIT_VIEWSTATE
                .iter()
                .find(|vs| vs.control_type.eq_ignore_ascii_case(ctype))
            {
                implicit_viewstate.push(ImplicitViewStateEntry {
                    control_id: cid.clone(),
                    control_type: ctype.clone(),
                    properties_persisted: implicit
                        .properties
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    estimated_size_impact: implicit.size_impact.to_string(),
                    modern_replacement: implicit.modern_state.to_string(),
                });

                // Add to heaviest list if High impact
                if implicit.size_impact.starts_with("High") {
                    heaviest_controls.push((
                        cid.clone(),
                        ctype.clone(),
                        implicit.size_impact.to_string(),
                    ));
                }

                // Generate modern state fields for this control
                for prop in implicit.properties.iter().take(4) {
                    let field_name = format!(
                        "{}{}",
                        to_camel_case(cid),
                        prop.replace(" (generated)", "")
                            .replace(" (data-bound only, no paging state)", "")
                            .replace(' ', "")
                    );
                    modern_state_model.push(StateFieldRecommendation {
                        field_name: field_name.clone(),
                        source: format!("{cid}.{prop} (implicit ViewState)"),
                        blazor_declaration: format!("// {cid}.{prop} → manage explicitly"),
                        react_declaration: format!("// {cid}.{prop} → manage in component state"),
                        persist_across: "Component re-renders".to_string(),
                    });
                }
            }
        }

        // Page-level ViewState setting
    }

    let page_level_viewstate = aspx_content.and_then(|aspx| {
        RE_PAGE_VS
            .captures(aspx)
            .map(|cap| cap[1].eq_ignore_ascii_case("true"))
    });

    let total = explicit_viewstate.len()
        + implicit_viewstate
            .iter()
            .map(|i| i.properties_persisted.len())
            .sum::<usize>();

    let migration_complexity = if total == 0 {
        "None: no ViewState dependencies detected".to_string()
    } else if total <= 5 && heaviest_controls.is_empty() {
        "Low: few state fields, straightforward component state mapping".to_string()
    } else if total <= 15 {
        format!("Medium: {total} state fields to manage explicitly")
    } else {
        format!(
            "High: {total} state fields ({} heavy controls) — consider state management library",
            heaviest_controls.len()
        )
    };

    Ok(ViewStateDependencyReport {
        file_path: file_path.to_string(),
        explicit_viewstate,
        implicit_viewstate,
        viewstate_disabled_controls: viewstate_disabled,
        page_level_viewstate,
        heaviest_controls,
        total_state_fields: total,
        migration_complexity,
        modern_state_model,
    })
}

// ── Helper functions ──────────────────────────────────────────────────────

fn build_method_map(content: &str) -> Vec<(usize, String)> {
    let mut methods = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        if let Some(cap) = RE_METHOD_CONTEXT.captures(line) {
            methods.push((line_num + 1, cap[1].to_string()));
        }
    }
    methods
}

fn find_enclosing_method(methods: &[(usize, String)], line: usize) -> String {
    let mut best = "(top-level)".to_string();
    for (start, name) in methods {
        if *start <= line {
            best = name.clone();
        } else {
            break;
        }
    }
    best
}

fn is_viewstate_write(line: &str, key: &str, is_vb: bool) -> bool {
    let pattern = if is_vb {
        format!(r#"ViewState\s*\(\s*"{}"\s*\)\s*="#, regex::escape(key))
    } else {
        format!(r#"ViewState\s*\[\s*"{}"\s*\]\s*="#, regex::escape(key))
    };
    Regex::new(&pattern)
        .map(|re| re.is_match(line))
        .unwrap_or(false)
}

fn infer_viewstate_type(key: &str) -> String {
    let k = key.to_lowercase();
    if k.contains("index") || k.contains("count") || k.contains("page") || k.contains("size") {
        "int".to_string()
    } else if k.contains("sort") && k.contains("dir") {
        "SortDirection".to_string()
    } else if k.contains("sort") {
        "string".to_string()
    } else if k.contains("date") || k.contains("time") {
        "DateTime?".to_string()
    } else if k.starts_with("is")
        || k.contains("has")
        || k.contains("enable")
        || k.contains("show")
        || k.contains("visible")
        || k.contains("active")
        || k.contains("checked")
    {
        "bool".to_string()
    } else if k.contains("id")
        && !k.contains("grid")
        && !k.contains("guid")
        && !k.contains("valid")
        && !k.contains("void")
        && !k.contains("width")
    {
        "int?".to_string()
    } else if k.contains("list")
        || k.contains("items")
        || k.starts_with("data")
        || k.contains("dataset")
        || k.contains("datatable")
        || k.contains("datasource")
    {
        "List<object>".to_string()
    } else {
        "object".to_string()
    }
}

fn classify_viewstate_lifecycle(readers: &[String], writers: &[String]) -> String {
    if writers.is_empty() && !readers.is_empty() {
        "ReadOnly (set externally or by framework)".to_string()
    } else if readers.is_empty() && !writers.is_empty() {
        "WriteOnly (set but never read — possibly dead code)".to_string()
    } else if writers.len() == 1 && readers.len() >= 1 {
        "SingleWriter (simple state)".to_string()
    } else if writers.len() > 1 {
        format!(
            "MultiWriter ({} writers — consider consolidating state mutations)",
            writers.len()
        )
    } else {
        "Unknown".to_string()
    }
}

fn to_camel_case(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap().to_lowercase().to_string();
    format!("{first}{}", chars.collect::<String>())
}

fn capitalize(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap().to_uppercase().to_string();
    format!("{first}{}", chars.collect::<String>())
}

// ── Format ────────────────────────────────────────────────────────────────

pub fn format_viewstate_report(report: &ViewStateDependencyReport) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str(&format!(
        "## ViewState Dependency Report: {}\n\n",
        report.file_path
    ));
    out.push_str(&format!(
        "**Total State Fields:** {} | **Complexity:** {}\n",
        report.total_state_fields, report.migration_complexity
    ));
    if let Some(page_vs) = report.page_level_viewstate {
        out.push_str(&format!(
            "**Page-level ViewState:** {}\n",
            if page_vs { "Enabled" } else { "Disabled" }
        ));
    }
    out.push('\n');

    // Explicit ViewState
    if !report.explicit_viewstate.is_empty() {
        out.push_str("### Explicit ViewState Usage\n\n");
        out.push_str("| Key | Type | Readers | Writers | Lifecycle |\n");
        out.push_str("|---|---|---|---|---|\n");
        for vs in &report.explicit_viewstate {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                vs.key,
                vs.data_type_guess,
                vs.readers.join(", "),
                vs.writers.join(", "),
                vs.lifecycle,
            ));
        }
        out.push('\n');
    }

    // Implicit ViewState
    if !report.implicit_viewstate.is_empty() {
        out.push_str("### Implicit ViewState (Control State)\n\n");
        for vs in &report.implicit_viewstate {
            out.push_str(&format!(
                "**{} ({})** — Size impact: {}\n",
                vs.control_id, vs.control_type, vs.estimated_size_impact
            ));
            out.push_str(&format!(
                "- Persisted: {}\n",
                vs.properties_persisted.join(", ")
            ));
            out.push_str(&format!("- Modern: `{}`\n\n", vs.modern_replacement));
        }
    }

    // ViewState disabled
    if !report.viewstate_disabled_controls.is_empty() {
        out.push_str(&format!(
            "### Controls with ViewState Disabled: {}\n\n",
            report.viewstate_disabled_controls.join(", ")
        ));
    }

    // Modern state model
    if !report.modern_state_model.is_empty() {
        out.push_str("### Recommended Modern State Model\n\n");
        out.push_str("```csharp\n");
        out.push_str("// Blazor component state fields\n");
        let mut seen = std::collections::HashSet::new();
        for field in &report.modern_state_model {
            if field.blazor_declaration.starts_with("private")
                && seen.insert(field.field_name.clone())
            {
                out.push_str(&format!("{}\n", field.blazor_declaration));
            }
        }
        out.push_str("```\n");
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_graph() -> Arc<GraphStore> {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.redb");
        Arc::new(GraphStore::open(&db_path).unwrap())
    }

    #[test]
    fn test_explicit_viewstate_vb() {
        let graph = make_graph();
        let code = r#"
Protected Sub Page_Load(sender As Object, e As EventArgs)
    If Not IsPostBack Then
        ViewState("SortColumn") = "Name"
        ViewState("SortDirection") = "ASC"
    End If
End Sub

Protected Sub gvResults_Sorting(sender As Object, e As GridViewSortEventArgs)
    Dim col As String = CStr(ViewState("SortColumn"))
    Dim dir As String = CStr(ViewState("SortDirection"))
End Sub
        "#;

        let result =
            analyze_viewstate_dependencies(&graph, "test", "Page.aspx.vb", code, None).unwrap();
        assert_eq!(result.explicit_viewstate.len(), 2);

        let sort_col = result
            .explicit_viewstate
            .iter()
            .find(|v| v.key == "SortColumn")
            .unwrap();
        assert_eq!(sort_col.data_type_guess, "string");
        assert!(!sort_col.writers.is_empty());
        assert!(!sort_col.readers.is_empty());
    }

    #[test]
    fn test_explicit_viewstate_cs() {
        let graph = make_graph();
        let code = r#"
protected void Page_Load(object sender, EventArgs e)
{
    if (!IsPostBack)
    {
        ViewState["PageIndex"] = 0;
        ViewState["IsEditing"] = false;
    }
}

protected void NextPage()
{
    int page = (int)ViewState["PageIndex"];
    ViewState["PageIndex"] = page + 1;
}
        "#;

        let result =
            analyze_viewstate_dependencies(&graph, "test", "Page.aspx.cs", code, None).unwrap();
        assert_eq!(result.explicit_viewstate.len(), 2);

        let page_idx = result
            .explicit_viewstate
            .iter()
            .find(|v| v.key == "PageIndex")
            .unwrap();
        assert_eq!(page_idx.data_type_guess, "int");
    }

    #[test]
    fn test_implicit_viewstate_gridview() {
        let graph = make_graph();
        let aspx = r#"
            <asp:GridView ID="gvCustomers" runat="server" AllowPaging="true" AllowSorting="true">
            </asp:GridView>
        "#;

        let result =
            analyze_viewstate_dependencies(&graph, "test", "Page.aspx.vb", "", Some(aspx)).unwrap();
        assert!(result.implicit_viewstate.len() >= 1);

        let gv = result
            .implicit_viewstate
            .iter()
            .find(|v| v.control_id == "gvCustomers")
            .unwrap();
        assert_eq!(gv.control_type, "GridView");
        assert!(
            gv.properties_persisted
                .contains(&"SortExpression".to_string())
        );
        assert!(gv.estimated_size_impact.contains("High"));
    }

    #[test]
    fn test_viewstate_disabled_controls() {
        let graph = make_graph();
        let aspx = r#"
            <asp:GridView ID="gvResults" EnableViewState="false" runat="server" />
            <asp:DropDownList ID="ddlState" runat="server" />
        "#;

        let result =
            analyze_viewstate_dependencies(&graph, "test", "Page.aspx.vb", "", Some(aspx)).unwrap();
        assert!(
            result
                .viewstate_disabled_controls
                .contains(&"gvResults".to_string())
        );
        // gvResults should NOT be in implicit_viewstate since it's disabled
        assert!(
            !result
                .implicit_viewstate
                .iter()
                .any(|v| v.control_id == "gvResults")
        );
        // ddlState SHOULD be in implicit_viewstate
        assert!(
            result
                .implicit_viewstate
                .iter()
                .any(|v| v.control_id == "ddlState")
        );
    }

    #[test]
    fn test_page_level_viewstate() {
        let graph = make_graph();
        let aspx = r#"<%@ Page Language="VB" EnableViewState="false" CodeBehind="Page.aspx.vb" %>"#;

        let result =
            analyze_viewstate_dependencies(&graph, "test", "Page.aspx.vb", "", Some(aspx)).unwrap();
        assert_eq!(result.page_level_viewstate, Some(false));
    }

    #[test]
    fn test_mixed_explicit_and_implicit() {
        let graph = make_graph();
        let code = r#"
Protected Sub Page_Load(sender As Object, e As EventArgs)
    ViewState("FilterText") = txtFilter.Text
End Sub
        "#;
        let aspx = r#"
            <asp:TextBox ID="txtFilter" runat="server" />
            <asp:GridView ID="gvData" runat="server" />
            <asp:DropDownList ID="ddlSort" runat="server" />
        "#;

        let result =
            analyze_viewstate_dependencies(&graph, "test", "Page.aspx.vb", code, Some(aspx))
                .unwrap();
        assert!(!result.explicit_viewstate.is_empty());
        assert!(!result.implicit_viewstate.is_empty());
        assert!(result.total_state_fields > 1);
    }

    #[test]
    fn test_modern_state_model_generated() {
        let graph = make_graph();
        let code = r#"ViewState("SortColumn") = "Name""#;

        let result =
            analyze_viewstate_dependencies(&graph, "test", "Page.aspx.vb", code, None).unwrap();
        assert!(!result.modern_state_model.is_empty());
        let field = result
            .modern_state_model
            .iter()
            .find(|f| f.field_name == "sortColumn")
            .unwrap();
        assert!(field.blazor_declaration.contains("private"));
    }

    #[test]
    fn test_no_viewstate() {
        let graph = make_graph();
        let result =
            analyze_viewstate_dependencies(&graph, "test", "Page.aspx.vb", "", None).unwrap();
        assert_eq!(result.total_state_fields, 0);
        assert!(result.migration_complexity.contains("None"));
    }

    #[test]
    fn test_type_inference() {
        assert_eq!(infer_viewstate_type("PageIndex"), "int");
        assert_eq!(infer_viewstate_type("SortDirection"), "SortDirection");
        assert_eq!(infer_viewstate_type("SortColumn"), "string");
        assert_eq!(infer_viewstate_type("IsEditing"), "bool");
        assert_eq!(infer_viewstate_type("StartDate"), "DateTime?");
        assert_eq!(infer_viewstate_type("CustomData"), "object");
    }
}
