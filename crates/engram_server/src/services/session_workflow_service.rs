//! Phase 37: Session Workflow Reconstruction
//!
//! Synthesizes existing graph edges (WritesState, ReadsState) into session
//! workflow narratives. Turns isolated facts into cross-page flow stories.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use engram_graph::{EdgeKind, GraphStore};
use serde::Serialize;

// ── Structs ──────────────────────────────────────────────────────────────────

/// Complete session workflow report for a project.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SessionWorkflowReport {
    pub workflows: Vec<SessionKeyFlow>,
    pub total_keys: usize,
    pub cross_page_chains: usize,
    pub warnings: Vec<String>,
}

/// Flow narrative for a single session/state key.
#[derive(Debug, Clone, Serialize)]
pub struct SessionKeyFlow {
    pub key: String,
    pub scope: StateScope,
    pub writers: Vec<StateOperation>,
    pub readers: Vec<StateOperation>,
    pub flow_narrative: String,
    pub pattern: FlowPattern,
}

/// The scope of a state key.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum StateScope {
    Session,
    Application,
    Cache,
    ViewState,
    Cookie,
    Unknown,
}

impl std::fmt::Display for StateScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session => write!(f, "Session"),
            Self::Application => write!(f, "Application"),
            Self::Cache => write!(f, "Cache"),
            Self::ViewState => write!(f, "ViewState"),
            Self::Cookie => write!(f, "Cookie"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Detected flow pattern.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum FlowPattern {
    /// A writes, B reads, C reads — simple pipeline
    Linear,
    /// A writes, B reads OR C reads — conditional flow
    Branching,
    /// A writes, B writes more, C reads all — state accumulates
    Accumulation,
    /// Only readers, no writers found
    MissingWriter,
    /// Only writers, no readers found
    MissingReader,
    /// 4+ pages involved
    ComplexWorkflow,
    /// Single page only (ViewState typically)
    SinglePage,
}

impl std::fmt::Display for FlowPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Linear => write!(f, "Linear"),
            Self::Branching => write!(f, "Branching"),
            Self::Accumulation => write!(f, "Accumulation"),
            Self::MissingWriter => write!(f, "Missing Writer"),
            Self::MissingReader => write!(f, "Missing Reader"),
            Self::ComplexWorkflow => write!(f, "Complex Workflow"),
            Self::SinglePage => write!(f, "Single Page"),
        }
    }
}

/// A single read or write operation on a state key.
#[derive(Debug, Clone, Serialize)]
pub struct StateOperation {
    pub file: String,
    pub operation: String, // "write" or "read"
}

// ── Reconstruction Logic ─────────────────────────────────────────────────────

/// Reconstruct session workflows from graph edges.
pub fn reconstruct_session_workflows(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> SessionWorkflowReport {
    // Query WritesState and ReadsState edges
    let write_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::WritesState, 10_000)
        .unwrap_or_default();
    let read_edges = graph
        .list_edges_by_kind(project_id, EdgeKind::ReadsState, 10_000)
        .unwrap_or_default();

    // Also include unresolved state reads/writes
    let unresolved_writes = graph
        .list_edges_by_kind(project_id, EdgeKind::UnresolvedStateWrite, 10_000)
        .unwrap_or_default();
    let unresolved_reads = graph
        .list_edges_by_kind(project_id, EdgeKind::UnresolvedStateRead, 10_000)
        .unwrap_or_default();

    // Group by state key (the target node ID)
    let mut key_writers: HashMap<String, Vec<StateOperation>> = HashMap::new();
    let mut key_readers: HashMap<String, Vec<StateOperation>> = HashMap::new();

    // The VB/C# state extractors occasionally emit `WritesState` /
    // `ReadsState` edges whose `target_id` is a symbol node (e.g.
    // `sym:member:...`) rather than a real state key. These come from
    // heuristic matches on patterns like `row.pas_area = pas_area`
    // inside DAL methods — they are property setters, not ASP.NET state
    // mutations. A real state key has a target_id starting with
    // `state:` (resolved) or `unresolved_state:` (best-effort). Filter
    // the rest out here so they do not pollute the workflow report
    // with an "Other" bucket of pseudo-state keys.
    let mut filtered_edges: usize = 0;

    for edge in write_edges.iter().chain(unresolved_writes.iter()) {
        if !is_state_target(&edge.target_id) {
            filtered_edges += 1;
            continue;
        }
        let key = edge.target_id.clone();
        let file = extract_file_from_node_id(&edge.source_id);
        key_writers.entry(key).or_default().push(StateOperation {
            file,
            operation: "write".to_string(),
        });
    }

    for edge in read_edges.iter().chain(unresolved_reads.iter()) {
        if !is_state_target(&edge.target_id) {
            filtered_edges += 1;
            continue;
        }
        let key = edge.target_id.clone();
        let file = extract_file_from_node_id(&edge.source_id);
        key_readers.entry(key).or_default().push(StateOperation {
            file,
            operation: "read".to_string(),
        });
    }

    let total_edges =
        write_edges.len() + unresolved_writes.len() + read_edges.len() + unresolved_reads.len();

    // Collect all unique keys
    let all_keys: HashSet<String> = key_writers
        .keys()
        .chain(key_readers.keys())
        .cloned()
        .collect();

    tracing::info!(
        project_id = %project_id,
        total_edges = total_edges,
        accepted_edges = total_edges - filtered_edges,
        filtered_non_state_targets = filtered_edges,
        distinct_state_keys = all_keys.len(),
        "reconstruct_session_workflows: edge filtering summary"
    );

    let mut workflows = Vec::new();
    let mut warnings = Vec::new();
    let mut cross_page_count = 0;

    for key in &all_keys {
        let raw_writers = key_writers.get(key).cloned().unwrap_or_default();
        let raw_readers = key_readers.get(key).cloned().unwrap_or_default();

        let scope = detect_scope(key);

        // Deduplicate operations: keep one entry per unique file
        let writer_files: HashSet<String> = raw_writers.iter().map(|w| w.file.clone()).collect();
        let reader_files: HashSet<String> = raw_readers.iter().map(|r| r.file.clone()).collect();
        let writers: Vec<StateOperation> = writer_files
            .iter()
            .map(|f| StateOperation {
                file: f.clone(),
                operation: "write".to_string(),
            })
            .collect();
        let readers: Vec<StateOperation> = reader_files
            .iter()
            .map(|f| StateOperation {
                file: f.clone(),
                operation: "read".to_string(),
            })
            .collect();
        let all_files: HashSet<String> = writer_files
            .iter()
            .chain(reader_files.iter())
            .cloned()
            .collect();

        // Detect pattern
        let pattern = detect_pattern(&writers, &readers, &writer_files, &reader_files, &scope);

        // Build narrative (reuse pre-computed file sets)
        let narrative = build_narrative(key, &scope, &writer_files, &reader_files, &pattern);

        // Warnings
        match &pattern {
            FlowPattern::MissingWriter => {
                warnings.push(format!(
                    "{key}: read but never written in analyzed code — possibly set externally"
                ));
            }
            FlowPattern::MissingReader => {
                warnings.push(format!(
                    "{key}: written but never read — possibly dead state"
                ));
            }
            FlowPattern::ComplexWorkflow => {
                warnings.push(format!(
                    "{key}: complex workflow spanning {} pages — requires careful migration",
                    all_files.len()
                ));
            }
            _ => {}
        }

        // Count cross-page chains
        if all_files.len() > 1 {
            cross_page_count += 1;
        }

        workflows.push(SessionKeyFlow {
            key: key.clone(),
            scope,
            writers,
            readers,
            flow_narrative: narrative,
            pattern,
        });
    }

    // Sort by key for deterministic output
    workflows.sort_by(|a, b| a.key.cmp(&b.key));

    SessionWorkflowReport {
        total_keys: workflows.len(),
        cross_page_chains: cross_page_count,
        workflows,
        warnings,
    }
}

/// True iff `target_id` is a genuine state-key node emitted by the state
/// extractor — i.e. the `state:<scope>:<key>` form (resolved) or its
/// best-effort `unresolved_state:<scope>:<key>` counterpart. All other
/// target shapes (e.g. `sym:member:...` from misclassified property
/// setters) are rejected so they never enter the workflow report.
fn is_state_target(target_id: &str) -> bool {
    target_id.starts_with("state:") || target_id.starts_with("unresolved_state:")
}

fn extract_file_from_node_id(node_id: &str) -> String {
    // Node IDs can have several formats:
    //   "file::path/to/file.aspx.vb::ClassName.MethodName"
    //   "path/to/file.aspx.vb"
    //   "global_state::Session:Key"  (state nodes — source_id is the file side)
    //
    // The source_id (which is what we pass here) is always the code file node.
    // We split on "::" and look for the segment containing a file extension.
    let parts: Vec<&str> = node_id.split("::").collect();
    for part in &parts {
        if part.contains('.') && !part.contains(' ') {
            // Looks like a file path (has extension)
            return part.to_string();
        }
    }
    // Fallback: return the first segment
    parts.first().unwrap_or(&node_id).to_string()
}

fn detect_scope(key: &str) -> StateScope {
    let lower = key.to_lowercase();

    // Check for explicit prefix format first (e.g., "Session:CartID", "Application:Counter")
    if let Some(prefix) = lower.split(':').next() {
        match prefix.trim() {
            "session" => return StateScope::Session,
            "application" => return StateScope::Application,
            "cache" => return StateScope::Cache,
            "viewstate" => return StateScope::ViewState,
            "cookie" | "cookies" => return StateScope::Cookie,
            _ => {}
        }
    }

    // Fallback to substring matching for node IDs like "global_state::Session_UserRole"
    if lower.contains("session") {
        StateScope::Session
    } else if lower.contains("application") {
        StateScope::Application
    } else if lower.contains("cache") {
        StateScope::Cache
    } else if lower.contains("viewstate") {
        StateScope::ViewState
    } else if lower.contains("cookie") {
        StateScope::Cookie
    } else {
        StateScope::Unknown
    }
}

fn detect_pattern(
    writers: &[StateOperation],
    readers: &[StateOperation],
    writer_files: &HashSet<String>,
    reader_files: &HashSet<String>,
    scope: &StateScope,
) -> FlowPattern {
    if writers.is_empty() && readers.is_empty() {
        return FlowPattern::SinglePage;
    }

    if writers.is_empty() {
        return FlowPattern::MissingWriter;
    }

    if readers.is_empty() {
        return FlowPattern::MissingReader;
    }

    let all_files: HashSet<String> = writer_files
        .iter()
        .chain(reader_files.iter())
        .cloned()
        .collect();

    // ViewState is always single-page by definition
    if *scope == StateScope::ViewState {
        return FlowPattern::SinglePage;
    }

    // Single page (same file reads and writes)
    if all_files.len() == 1 {
        return FlowPattern::SinglePage;
    }

    // Complex workflow: 4+ pages
    if all_files.len() >= 4 {
        return FlowPattern::ComplexWorkflow;
    }

    // Accumulation: multiple writers
    if writer_files.len() > 1 {
        return FlowPattern::Accumulation;
    }

    // Branching: one writer, multiple readers from different files
    if reader_files.len() > 1 {
        return FlowPattern::Branching;
    }

    // Linear: one writer, one reader
    FlowPattern::Linear
}

fn build_narrative(
    key: &str,
    scope: &StateScope,
    writer_files: &HashSet<String>,
    reader_files: &HashSet<String>,
    pattern: &FlowPattern,
) -> String {
    // Sort for deterministic output
    let mut w_sorted: Vec<&str> = writer_files.iter().map(|s| s.as_str()).collect();
    w_sorted.sort();
    let mut r_sorted: Vec<&str> = reader_files.iter().map(|s| s.as_str()).collect();
    r_sorted.sort();

    match pattern {
        FlowPattern::Linear => {
            format!(
                "{scope} key '{key}': {} (creates) → {} (reads)",
                w_sorted.join(", "),
                r_sorted.join(", ")
            )
        }
        FlowPattern::Branching => {
            format!(
                "{scope} key '{key}': {} (creates) → [{}] (reads from multiple pages)",
                w_sorted.join(", "),
                r_sorted.join(" OR ")
            )
        }
        FlowPattern::Accumulation => {
            format!(
                "{scope} key '{key}': [{}] (multiple writers) → {} (reads accumulated state)",
                w_sorted.join(", "),
                r_sorted.join(", ")
            )
        }
        FlowPattern::MissingWriter => {
            format!(
                "{scope} key '{key}': ??? → {} (reads, but no writer found in code)",
                r_sorted.join(", ")
            )
        }
        FlowPattern::MissingReader => {
            format!(
                "{scope} key '{key}': {} (writes, but no reader found in code)",
                w_sorted.join(", ")
            )
        }
        FlowPattern::ComplexWorkflow => {
            let mut all: Vec<&str> = writer_files
                .iter()
                .chain(reader_files.iter())
                .map(|s| s.as_str())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            all.sort();
            format!(
                "{scope} key '{key}': complex workflow across {} pages: {}",
                all.len(),
                all.join(" → ")
            )
        }
        FlowPattern::SinglePage => {
            let page = w_sorted
                .first()
                .or(r_sorted.first())
                .copied()
                .unwrap_or("unknown");
            format!("{scope} key '{key}': single-page state in {page}")
        }
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// Render session workflows as a markdown section.
pub fn render_session_workflows_markdown(report: &SessionWorkflowReport) -> String {
    if report.workflows.is_empty() {
        return String::new();
    }

    let mut md = String::with_capacity(8_000);
    md.push_str("## Session Workflows\n\n");
    md.push_str(&format!(
        "- **Total state keys**: {}\n- **Cross-page flows**: {}\n\n",
        report.total_keys, report.cross_page_chains
    ));

    // Group by scope
    let mut by_scope: BTreeMap<String, Vec<&SessionKeyFlow>> = BTreeMap::new();
    for flow in &report.workflows {
        by_scope
            .entry(flow.scope.to_string())
            .or_default()
            .push(flow);
    }

    for (scope, flows) in &by_scope {
        md.push_str(&format!("### {scope} State Flows\n\n"));
        for flow in flows {
            let pattern_label = match &flow.pattern {
                FlowPattern::ComplexWorkflow => " ⚠️ **Complex**",
                FlowPattern::MissingWriter => " ⚠️ **No Writer**",
                FlowPattern::MissingReader => " ℹ️ **No Reader**",
                _ => "",
            };
            md.push_str(&format!("- {}{pattern_label}\n", flow.flow_narrative));
        }
        md.push('\n');
    }

    if !report.warnings.is_empty() {
        md.push_str("### Session Workflow Warnings\n\n");
        for w in &report.warnings {
            md.push_str(&format!("- ⚠️ {w}\n"));
        }
        md.push('\n');
    }

    md
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_scope_session() {
        assert_eq!(detect_scope("Session:CartID"), StateScope::Session);
        assert_eq!(detect_scope("session_userRole"), StateScope::Session);
    }

    #[test]
    fn test_detect_scope_viewstate() {
        assert_eq!(detect_scope("ViewState:SortColumn"), StateScope::ViewState);
    }

    #[test]
    fn test_detect_scope_application() {
        assert_eq!(
            detect_scope("Application:GlobalCounter"),
            StateScope::Application
        );
    }

    #[test]
    fn test_linear_flow_pattern() {
        let writers = vec![StateOperation {
            file: "Products.aspx".to_string(),
            operation: "write".to_string(),
        }];
        let readers = vec![StateOperation {
            file: "Cart.aspx".to_string(),
            operation: "read".to_string(),
        }];
        let writer_files: HashSet<String> = writers.iter().map(|w| w.file.clone()).collect();
        let reader_files: HashSet<String> = readers.iter().map(|r| r.file.clone()).collect();

        let pattern = detect_pattern(
            &writers,
            &readers,
            &writer_files,
            &reader_files,
            &StateScope::Session,
        );
        assert_eq!(pattern, FlowPattern::Linear);
    }

    #[test]
    fn test_branching_flow_pattern() {
        let writers = vec![StateOperation {
            file: "Login.aspx".to_string(),
            operation: "write".to_string(),
        }];
        let readers = vec![
            StateOperation {
                file: "Dashboard.aspx".to_string(),
                operation: "read".to_string(),
            },
            StateOperation {
                file: "Profile.aspx".to_string(),
                operation: "read".to_string(),
            },
        ];
        let writer_files: HashSet<String> = writers.iter().map(|w| w.file.clone()).collect();
        let reader_files: HashSet<String> = readers.iter().map(|r| r.file.clone()).collect();

        let pattern = detect_pattern(
            &writers,
            &readers,
            &writer_files,
            &reader_files,
            &StateScope::Session,
        );
        assert_eq!(pattern, FlowPattern::Branching);
    }

    #[test]
    fn test_accumulation_flow_pattern() {
        let writers = vec![
            StateOperation {
                file: "Step1.aspx".to_string(),
                operation: "write".to_string(),
            },
            StateOperation {
                file: "Step2.aspx".to_string(),
                operation: "write".to_string(),
            },
        ];
        let readers = vec![StateOperation {
            file: "Summary.aspx".to_string(),
            operation: "read".to_string(),
        }];
        let writer_files: HashSet<String> = writers.iter().map(|w| w.file.clone()).collect();
        let reader_files: HashSet<String> = readers.iter().map(|r| r.file.clone()).collect();

        let pattern = detect_pattern(
            &writers,
            &readers,
            &writer_files,
            &reader_files,
            &StateScope::Session,
        );
        assert_eq!(pattern, FlowPattern::Accumulation);
    }

    #[test]
    fn test_missing_writer_pattern() {
        let writers: Vec<StateOperation> = vec![];
        let readers = vec![StateOperation {
            file: "Page.aspx".to_string(),
            operation: "read".to_string(),
        }];
        let writer_files: HashSet<String> = HashSet::new();
        let reader_files: HashSet<String> = readers.iter().map(|r| r.file.clone()).collect();

        let pattern = detect_pattern(
            &writers,
            &readers,
            &writer_files,
            &reader_files,
            &StateScope::Session,
        );
        assert_eq!(pattern, FlowPattern::MissingWriter);
    }

    #[test]
    fn test_missing_reader_pattern() {
        let writers = vec![StateOperation {
            file: "Page.aspx".to_string(),
            operation: "write".to_string(),
        }];
        let readers: Vec<StateOperation> = vec![];
        let writer_files: HashSet<String> = writers.iter().map(|w| w.file.clone()).collect();
        let reader_files: HashSet<String> = HashSet::new();

        let pattern = detect_pattern(
            &writers,
            &readers,
            &writer_files,
            &reader_files,
            &StateScope::Session,
        );
        assert_eq!(pattern, FlowPattern::MissingReader);
    }

    #[test]
    fn test_complex_workflow_pattern() {
        let writers = vec![StateOperation {
            file: "Step1.aspx".to_string(),
            operation: "write".to_string(),
        }];
        let readers = vec![
            StateOperation {
                file: "Step2.aspx".to_string(),
                operation: "read".to_string(),
            },
            StateOperation {
                file: "Step3.aspx".to_string(),
                operation: "read".to_string(),
            },
            StateOperation {
                file: "Step4.aspx".to_string(),
                operation: "read".to_string(),
            },
        ];
        let writer_files: HashSet<String> = writers.iter().map(|w| w.file.clone()).collect();
        let reader_files: HashSet<String> = readers.iter().map(|r| r.file.clone()).collect();

        let pattern = detect_pattern(
            &writers,
            &readers,
            &writer_files,
            &reader_files,
            &StateScope::Session,
        );
        assert_eq!(pattern, FlowPattern::ComplexWorkflow);
    }

    #[test]
    fn test_single_page_viewstate() {
        let writers = vec![StateOperation {
            file: "Page.aspx".to_string(),
            operation: "write".to_string(),
        }];
        let readers = vec![StateOperation {
            file: "Page.aspx".to_string(),
            operation: "read".to_string(),
        }];
        let writer_files: HashSet<String> = writers.iter().map(|w| w.file.clone()).collect();
        let reader_files: HashSet<String> = readers.iter().map(|r| r.file.clone()).collect();

        let pattern = detect_pattern(
            &writers,
            &readers,
            &writer_files,
            &reader_files,
            &StateScope::ViewState,
        );
        assert_eq!(pattern, FlowPattern::SinglePage);
    }

    #[test]
    fn test_empty_graph_produces_empty_report() {
        let report = SessionWorkflowReport::default();
        assert!(report.workflows.is_empty());
        assert_eq!(report.total_keys, 0);
    }

    #[test]
    fn test_render_session_workflows_empty() {
        let report = SessionWorkflowReport::default();
        let md = render_session_workflows_markdown(&report);
        assert!(md.is_empty());
    }

    #[test]
    fn test_render_session_workflows_with_flows() {
        let report = SessionWorkflowReport {
            workflows: vec![SessionKeyFlow {
                key: "Session:CartID".to_string(),
                scope: StateScope::Session,
                writers: vec![StateOperation {
                    file: "Products.aspx".to_string(),
                    operation: "write".to_string(),
                }],
                readers: vec![StateOperation {
                    file: "Cart.aspx".to_string(),
                    operation: "read".to_string(),
                }],
                flow_narrative:
                    "Session key 'Session:CartID': Products.aspx (creates) → Cart.aspx (reads)"
                        .to_string(),
                pattern: FlowPattern::Linear,
            }],
            total_keys: 1,
            cross_page_chains: 1,
            warnings: vec![],
        };

        let md = render_session_workflows_markdown(&report);
        assert!(md.contains("## Session Workflows"));
        assert!(md.contains("Session:CartID"));
        assert!(md.contains("Products.aspx"));
    }

    // ── is_state_target + reconstruction filter ─────────────────────────────

    #[test]
    fn is_state_target_accepts_state_and_unresolved_prefixes() {
        assert!(is_state_target("state:Session:CartID"));
        assert!(is_state_target("state:Application:Counter"));
        assert!(is_state_target("state:ViewState:SortColumn"));
        assert!(is_state_target("state:Cache:HomepageHtml"));
        assert!(is_state_target("state:Cookies:PreviousUrl"));
        assert!(is_state_target("unresolved_state:Session:LastLogin"));
    }

    #[test]
    fn is_state_target_rejects_symbol_and_file_targets() {
        // The exact shape of the pilot-corpus pollution: a VB row/property assignment
        // misclassified as a WritesState edge.
        assert!(!is_state_target(
            "sym:member:Site/App_Code/permits/code/permits.vb:row.pas_color = pas_color:0"
        ));
        assert!(!is_state_target("sym:class:Foo"));
        assert!(!is_state_target("sym:function:Foo.Bar"));
        assert!(!is_state_target("file:Default.aspx.vb"));
        assert!(!is_state_target("page:Default.aspx"));
        assert!(!is_state_target(""));
        assert!(!is_state_target("Session:CartID")); // missing `state:` prefix
    }

    /// Reconstruction must drop WritesState / ReadsState edges whose
    /// `target_id` is not a real state key. The real-world symptom on
    /// the pilot corpus was 879 `sym:member:...` edges polluting the "Other" bucket.
    #[test]
    fn reconstruct_session_workflows_filters_non_state_targets() {
        use engram_graph::{Edge, GraphStore, Node};

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let graph =
            Arc::new(GraphStore::open(&tmp.path().join("graph.redb")).expect("GraphStore::open"));

        let project = "proj";

        // Upsert source + target nodes. The content doesn't matter for this
        // service — it only reads edges — but nodes are required for edges
        // to resolve cleanly through the store's usual paths.
        fn mknode(id: &str, kind: &str) -> Node {
            Node {
                node_id: id.to_string(),
                node_type: kind.to_string(),
                name: id.to_string(),
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                file_path: engram_core::RelPath::new("Site/App_Code/Demo.vb"),
                start_line: 0,
                end_line: 0,
                generation: 1,
                metadata: None,
            }
        }
        fn mkedge(src: &str, tgt: &str, kind: EdgeKind) -> Edge {
            Edge {
                source_id: src.to_string(),
                target_id: tgt.to_string(),
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                edge_kind: kind,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }
        }

        let source_id = "sym:function:Site/App_Code/Demo.vb:DoWork:0";
        let real_session = "state:Session:CartID";
        let real_app = "state:Application:Counter";
        // The pollution: a misclassified VB property-setter assignment.
        let fake_member =
            "sym:member:Site/App_Code/permits/code/permits.vb:row.pas_color = pas_color:0";

        graph
            .upsert_nodes(
                project,
                &[
                    mknode(source_id, "function"),
                    mknode(real_session, "global_state"),
                    mknode(real_app, "global_state"),
                    mknode(fake_member, "member"),
                ],
            )
            .expect("upsert_nodes");

        graph
            .upsert_edges(
                project,
                &[
                    mkedge(source_id, real_session, EdgeKind::WritesState),
                    mkedge(source_id, real_app, EdgeKind::WritesState),
                    mkedge(source_id, fake_member, EdgeKind::WritesState),
                ],
            )
            .expect("upsert_edges");

        let report = reconstruct_session_workflows(&graph, project);

        assert_eq!(
            report.total_keys, 2,
            "only the two state:... targets count; sym:member:... must be filtered"
        );
        for flow in &report.workflows {
            assert!(
                !flow.key.starts_with("sym:"),
                "no workflow entry may reference a sym: target; got key = {}",
                flow.key
            );
        }
        let keys: Vec<&str> = report.workflows.iter().map(|f| f.key.as_str()).collect();
        assert!(keys.contains(&real_session));
        assert!(keys.contains(&real_app));
    }
}
