//! Design anti-pattern detection via graph heuristics.
//!
//! Runs 5 deterministic checks against the project graph:
//! 1. God Object — class/file with too many Contains edges
//! 2. Spaghetti Events — function with Dependency in-edges from many files
//! 3. Session Soup — Session key accessed from many files
//! 4. SqlDataSource Coupling — node with both SqlCalls + DataBinding
//! 5. Tight GIS Coupling — node with both SpatialCall + DataBinding

use engram_graph::store::{EdgeKind, GraphStore};
use std::collections::{HashMap, HashSet};

// ── Public types ──────────────────────────────────────────────────────────

/// Severity of a detected design anti-pattern.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
pub enum AntiPatternSeverity {
    Minor,
    Moderate,
    Severe,
}

impl std::fmt::Display for AntiPatternSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AntiPatternSeverity::Minor => f.write_str("Minor"),
            AntiPatternSeverity::Moderate => f.write_str("Moderate"),
            AntiPatternSeverity::Severe => f.write_str("Severe"),
        }
    }
}

/// A detected design anti-pattern in the codebase.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DesignAntiPattern {
    pub pattern_name: String,
    pub description: String,
    pub severity: AntiPatternSeverity,
    pub affected_nodes: Vec<String>,
    pub evidence: Vec<String>,
    pub modern_target: String,
    pub refactoring_steps: Vec<String>,
}

// ── Detection engine ──────────────────────────────────────────────────────

/// Detect design anti-patterns by analyzing graph structure.
///
/// Runs 5 deterministic heuristics (no LLM required):
/// 1. **God Object** — classes/files with > `god_threshold` Contains edges
/// 2. **Spaghetti Events** — functions targeted by > `spaghetti_threshold` Dependency edges from different files
/// 3. **Session Soup** — Session state keys accessed from > `soup_threshold` different files
/// 4. **SqlDataSource Coupling** — nodes with both SqlCalls + DataBinding edges
/// 5. **Tight GIS Coupling** — files with both SpatialCall + DataBinding edges
pub fn detect_design_antipatterns(
    graph: &GraphStore,
    project_id: &str,
    god_threshold: usize,
    spaghetti_threshold: usize,
    soup_threshold: usize,
) -> anyhow::Result<Vec<DesignAntiPattern>> {
    let mut patterns = Vec::new();

    // 1. God Object: class/file nodes with too many Contains edges
    let contains_edges = graph.list_edges_by_kind(project_id, EdgeKind::Contains, 10_000)?;
    let mut contains_out: HashMap<String, Vec<String>> = HashMap::new();
    for e in &contains_edges {
        contains_out
            .entry(e.source_id.clone())
            .or_default()
            .push(e.target_id.clone());
    }
    for (node_id, children) in &contains_out {
        if children.len() > god_threshold {
            let node = graph.get_node(project_id, node_id)?;
            let node_type = node
                .as_ref()
                .map(|n| n.node_type.as_str())
                .unwrap_or("unknown");
            if matches!(node_type, "class" | "file") {
                let node_name = node
                    .as_ref()
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| node_id.clone());
                patterns.push(DesignAntiPattern {
                    pattern_name: "God Object".into(),
                    description: format!(
                        "{} contains {} members (threshold: {}). \
                         This violates the Single Responsibility Principle.",
                        node_name,
                        children.len(),
                        god_threshold
                    ),
                    severity: if children.len() > god_threshold * 2 {
                        AntiPatternSeverity::Severe
                    } else {
                        AntiPatternSeverity::Moderate
                    },
                    affected_nodes: vec![node_id.clone()],
                    evidence: children.iter().take(5).cloned().collect(),
                    modern_target:
                        "Split into focused service classes (Single Responsibility Principle). \
                         Each class should have one reason to change."
                            .into(),
                    refactoring_steps: vec![
                        "Identify cohesive groups of methods by data affinity".into(),
                        "Extract each group into a dedicated service class".into(),
                        "Create interfaces for dependency injection".into(),
                        "Route callers through the new services".into(),
                    ],
                });
            }
        }
    }

    // 2. Spaghetti Events: functions with many incoming Dependency edges from different files
    let dep_edges = graph.list_edges_by_kind(project_id, EdgeKind::Dependency, 10_000)?;
    let mut dep_in: HashMap<String, HashSet<String>> = HashMap::new();
    for e in &dep_edges {
        let source_file = if e.source_id.starts_with("file:") {
            e.source_id.clone()
        } else {
            // Extract file from sym:kind:path:name:line
            let parts: Vec<&str> = e.source_id.splitn(4, ':').collect();
            if parts.len() >= 3 {
                format!("file:{}", parts[2])
            } else {
                e.source_id.clone()
            }
        };
        dep_in
            .entry(e.target_id.clone())
            .or_default()
            .insert(source_file);
    }
    for (node_id, source_files) in &dep_in {
        if source_files.len() > spaghetti_threshold {
            let node = graph.get_node(project_id, node_id)?;
            let node_type = node
                .as_ref()
                .map(|n| n.node_type.as_str())
                .unwrap_or("unknown");
            if matches!(node_type, "function" | "class") {
                let node_name = node
                    .as_ref()
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| node_id.clone());
                patterns.push(DesignAntiPattern {
                    pattern_name: "Spaghetti Events".into(),
                    description: format!(
                        "{} is called from {} different files (threshold: {}). \
                         Indicates tight coupling and fragile event chains.",
                        node_name,
                        source_files.len(),
                        spaghetti_threshold
                    ),
                    severity: if source_files.len() > spaghetti_threshold * 2 {
                        AntiPatternSeverity::Severe
                    } else {
                        AntiPatternSeverity::Moderate
                    },
                    affected_nodes: vec![node_id.clone()],
                    evidence: source_files.iter().take(5).cloned().collect(),
                    modern_target: "Mediator Pattern (MediatR) or Event Bus. \
                                    Decouple callers from the handler."
                        .into(),
                    refactoring_steps: vec![
                        "Define IRequest/INotification for each event type".into(),
                        "Move handler logic into IRequestHandler implementations".into(),
                        "Replace direct calls with mediator.Send(request)".into(),
                        "Register handlers in DI container".into(),
                    ],
                });
            }
        }
    }

    // 3. Session Soup: Session state keys accessed from many different files
    let reads_edges = graph.list_edges_by_kind(project_id, EdgeKind::ReadsState, 5_000)?;
    let writes_edges = graph.list_edges_by_kind(project_id, EdgeKind::WritesState, 5_000)?;
    let mut state_files: HashMap<String, HashSet<String>> = HashMap::new();
    for e in reads_edges.iter().chain(writes_edges.iter()) {
        if e.target_id.starts_with("state:Session:") {
            let source_file = if e.source_id.starts_with("file:") {
                e.source_id.clone()
            } else {
                let parts: Vec<&str> = e.source_id.splitn(4, ':').collect();
                if parts.len() >= 3 {
                    format!("file:{}", parts[2])
                } else {
                    e.source_id.clone()
                }
            };
            state_files
                .entry(e.target_id.clone())
                .or_default()
                .insert(source_file);
        }
    }
    for (state_key, files) in &state_files {
        if files.len() > soup_threshold {
            let key_name = state_key
                .strip_prefix("state:Session:")
                .unwrap_or(state_key);
            patterns.push(DesignAntiPattern {
                pattern_name: "Session Soup".into(),
                description: format!(
                    "Session key '{}' is accessed from {} different files (threshold: {}). \
                     Session overuse makes stateless migration difficult.",
                    key_name,
                    files.len(),
                    soup_threshold
                ),
                severity: if files.len() > soup_threshold * 2 {
                    AntiPatternSeverity::Severe
                } else {
                    AntiPatternSeverity::Moderate
                },
                affected_nodes: vec![state_key.clone()],
                evidence: files.iter().take(5).cloned().collect(),
                modern_target: "Repository Pattern + REST API with JWT claims. \
                                Replace Session with proper state management."
                    .into(),
                refactoring_steps: vec![
                    "Identify the data lifecycle for this session key".into(),
                    "Create a typed DTO/ViewModel to replace the session slot".into(),
                    "Expose via REST API endpoint with proper auth".into(),
                    "Use JWT claims for auth-related state, Redis for shared session".into(),
                ],
            });
        }
    }

    // 4. SqlDataSource Coupling: nodes with both SqlCalls + DataBinding in the same file
    let sql_edges = graph.list_edges_by_kind(project_id, EdgeKind::SqlCalls, 5_000)?;
    let binding_edges = graph.list_edges_by_kind(project_id, EdgeKind::DataBinding, 5_000)?;
    let sql_sources: HashSet<_> = sql_edges.iter().map(|e| &e.source_id).collect();
    let binding_sources: HashSet<_> = binding_edges.iter().map(|e| &e.source_id).collect();
    let coupled: Vec<_> = sql_sources
        .intersection(&binding_sources)
        .cloned()
        .collect();
    for node_id in &coupled {
        let node = graph.get_node(project_id, node_id)?;
        let node_name = node
            .as_ref()
            .map(|n| n.name.clone())
            .unwrap_or_else(|| (*node_id).clone());
        patterns.push(DesignAntiPattern {
            pattern_name: "SqlDataSource Coupling".into(),
            description: format!(
                "{} has both SQL data access and UI data binding in the same scope. \
                 This violates separation of concerns.",
                node_name
            ),
            severity: AntiPatternSeverity::Moderate,
            affected_nodes: vec![(*node_id).clone()],
            evidence: vec![
                format!("SQL edges from {}", node_id),
                format!("DataBinding edges from {}", node_id),
            ],
            modern_target: "Repository Pattern + Typed DTOs + Blazor/Razor component binding. \
                            Separate data access from UI."
                .into(),
            refactoring_steps: vec![
                "Extract SQL queries into a Repository class".into(),
                "Define DTOs for data transfer between layers".into(),
                "Bind UI to DTOs via ViewModel/Controller".into(),
                "Remove SqlDataSource controls from markup".into(),
            ],
        });
    }

    // 5. Tight GIS Coupling: files with both SpatialCall + DataBinding
    let gis_edges = graph.list_edges_by_kind(project_id, EdgeKind::SpatialCall, 5_000)?;
    let gis_sources: HashSet<_> = gis_edges.iter().map(|e| &e.source_id).collect();
    let gis_coupled: Vec<_> = gis_sources
        .intersection(&binding_sources)
        .cloned()
        .collect();
    for node_id in &gis_coupled {
        let node = graph.get_node(project_id, node_id)?;
        let node_name = node
            .as_ref()
            .map(|n| n.name.clone())
            .unwrap_or_else(|| (*node_id).clone());
        patterns.push(DesignAntiPattern {
            pattern_name: "Tight GIS Coupling".into(),
            description: format!(
                "{} mixes GIS map logic with UI data binding. \
                 Isolate spatial logic for independent migration.",
                node_name
            ),
            severity: AntiPatternSeverity::Minor,
            affected_nodes: vec![(*node_id).clone()],
            evidence: vec![
                format!("SpatialCall edges from {}", node_id),
                format!("DataBinding edges from {}", node_id),
            ],
            modern_target: "Dedicated GIS service layer + React map component (react-leaflet or \
                            @react-google-maps/api)."
                .into(),
            refactoring_steps: vec![
                "Extract GIS logic into a standalone service class".into(),
                "Create a REST API for coordinate/polygon data".into(),
                "Replace legacy map widget with React map component".into(),
                "Pass GIS data via props/state, not server-rendered scripts".into(),
            ],
        });
    }

    // 6. Phase 30 Gap 8c: Windows Service / Background Job detection
    // Look for function/class nodes with metadata indicating background service patterns
    let all_nodes_by_type = graph.count_nodes_by_type(project_id)?;
    if all_nodes_by_type.contains_key("background_service") {
        // background_service nodes were emitted by extractors
        let dep_edges_all = graph.list_edges_by_kind(project_id, EdgeKind::Dependency, 10_000)?;
        for e in &dep_edges_all {
            if let Some(node) = graph.get_node(project_id, &e.source_id)?
                && node.node_type == "background_service"
            {
                patterns.push(DesignAntiPattern {
                    pattern_name: "Windows Service".into(),
                    description: format!(
                        "{} is a Windows Service or background job. \
                             These require special migration strategies.",
                        node.name
                    ),
                    severity: AntiPatternSeverity::Moderate,
                    affected_nodes: vec![e.source_id.clone()],
                    evidence: vec![format!(
                        "Node type: background_service in {}",
                        node.file_path.as_str()
                    )],
                    modern_target: "ASP.NET Core BackgroundService / IHostedService, \
                                        Hangfire, or Azure Functions"
                        .into(),
                    refactoring_steps: vec![
                        "Identify timer intervals and trigger conditions".into(),
                        "Create IHostedService or BackgroundService implementation".into(),
                        "Migrate OnStart/OnStop to StartAsync/StopAsync".into(),
                        "Register in Program.cs via builder.Services.AddHostedService<T>()".into(),
                    ],
                });
            }
        }
    }

    Ok(patterns)
}

/// A detected background service pattern in source code.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DetectedServicePattern {
    pub pattern: String,
    pub file_path: String,
    pub modern_equivalent: String,
    pub evidence: String,
}

/// Detect Windows Service, Quartz.NET, Hangfire, and Timer-based patterns in source code.
/// Returns detected patterns for background service/job identification.
pub fn detect_background_service_patterns(
    source: &str,
    file_path: &str,
    language: &str,
) -> Vec<DetectedServicePattern> {
    let mut results = Vec::new();
    let src_lower = source.to_lowercase();

    // ServiceBase inheritance
    if src_lower.contains("servicebase") || src_lower.contains("inherits servicebase") {
        results.push(DetectedServicePattern {
            pattern: "windows_service".to_string(),
            file_path: file_path.to_string(),
            modern_equivalent: "ASP.NET Core BackgroundService / IHostedService".to_string(),
            evidence: "Inherits from System.ServiceProcess.ServiceBase".to_string(),
        });
    }

    // Quartz.NET
    if src_lower.contains("ijob") && src_lower.contains("execute") && src_lower.contains("quartz") {
        results.push(DetectedServicePattern {
            pattern: "quartz_scheduled_job".to_string(),
            file_path: file_path.to_string(),
            modern_equivalent: "Quartz.NET on ASP.NET Core, or Hangfire".to_string(),
            evidence: "Implements Quartz.NET IJob interface".to_string(),
        });
    }

    // Hangfire
    if src_lower.contains("backgroundjob.enqueue") || src_lower.contains("recurringjob.addorupdate")
    {
        results.push(DetectedServicePattern {
            pattern: "hangfire_job".to_string(),
            file_path: file_path.to_string(),
            modern_equivalent: "Hangfire on ASP.NET Core (compatible)".to_string(),
            evidence: "Uses Hangfire BackgroundJob/RecurringJob API".to_string(),
        });
    }

    // System.Timers.Timer or System.Threading.Timer in service context
    if (src_lower.contains("system.timers.timer") || src_lower.contains("system.threading.timer"))
        && (src_lower.contains("servicebase") || src_lower.contains("onstart"))
    {
        results.push(DetectedServicePattern {
            pattern: "timer_service".to_string(),
            file_path: file_path.to_string(),
            modern_equivalent: "PeriodicTimer in BackgroundService".to_string(),
            evidence: "Timer usage in service context".to_string(),
        });
    }

    let _ = language; // reserved for future language-specific patterns
    results
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_display() {
        assert_eq!(AntiPatternSeverity::Minor.to_string(), "Minor");
        assert_eq!(AntiPatternSeverity::Moderate.to_string(), "Moderate");
        assert_eq!(AntiPatternSeverity::Severe.to_string(), "Severe");
    }

    #[test]
    fn test_design_antipattern_struct() {
        let p = DesignAntiPattern {
            pattern_name: "God Object".into(),
            description: "Too many members".into(),
            severity: AntiPatternSeverity::Severe,
            affected_nodes: vec!["file:test.vb".into()],
            evidence: vec!["child1".into(), "child2".into()],
            modern_target: "SRP".into(),
            refactoring_steps: vec!["Step 1".into()],
        };
        assert_eq!(p.pattern_name, "God Object");
        assert_eq!(p.severity, AntiPatternSeverity::Severe);
        assert_eq!(p.affected_nodes.len(), 1);
        assert_eq!(p.refactoring_steps.len(), 1);
    }

    #[test]
    fn test_detect_on_empty_graph() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&dir.path().join("graph.redb")).unwrap();
        let result = detect_design_antipatterns(&graph, "test_proj", 20, 10, 5).unwrap();
        assert!(
            result.is_empty(),
            "Empty graph should yield no anti-patterns"
        );
    }

    // ── AntiPatternSeverity ──────────────────────────────────────────────────

    #[test]
    fn anti_pattern_severity_equality() {
        assert_eq!(AntiPatternSeverity::Minor, AntiPatternSeverity::Minor);
        assert_ne!(AntiPatternSeverity::Minor, AntiPatternSeverity::Moderate);
        assert_ne!(AntiPatternSeverity::Moderate, AntiPatternSeverity::Severe);
    }

    // ── detect_background_service_patterns: windows service ──────────────────

    #[test]
    fn detects_windows_service_inherits() {
        let source = r#"
Public Class MyWindowsService
    Inherits ServiceBase

    Protected Overrides Sub OnStart(ByVal args() As String)
        ' Start logic
    End Sub
End Class
"#;
        let results = detect_background_service_patterns(source, "MyService.vb", "vb");
        assert!(
            results.iter().any(|r| r.pattern == "windows_service"),
            "should detect ServiceBase inheritance"
        );
    }

    #[test]
    fn detects_windows_service_case_insensitive() {
        let source = "public class MyService : ServiceBase { }";
        let results = detect_background_service_patterns(source, "MyService.cs", "cs");
        assert!(results.iter().any(|r| r.pattern == "windows_service"));
    }

    #[test]
    fn windows_service_modern_equivalent_is_ihosted_service() {
        let source = "Inherits ServiceBase";
        let results = detect_background_service_patterns(source, "Svc.vb", "vb");
        let r = results
            .iter()
            .find(|r| r.pattern == "windows_service")
            .unwrap();
        assert!(
            r.modern_equivalent.contains("BackgroundService")
                || r.modern_equivalent.contains("IHostedService"),
            "modern equivalent should mention BackgroundService or IHostedService"
        );
    }

    // ── detect_background_service_patterns: quartz ───────────────────────────

    #[test]
    fn detects_quartz_ijob() {
        let source = r#"
using Quartz;
public class MyJob : IJob
{
    public void Execute(IJobExecutionContext context) { }
}
"#;
        let results = detect_background_service_patterns(source, "MyJob.cs", "cs");
        assert!(
            results.iter().any(|r| r.pattern == "quartz_scheduled_job"),
            "should detect Quartz IJob"
        );
    }

    #[test]
    fn quartz_requires_all_three_tokens() {
        // missing "quartz" → should NOT detect quartz
        let source = "public class MyJob : IJob { public void Execute() { } }";
        let results = detect_background_service_patterns(source, "Job.cs", "cs");
        assert!(
            !results.iter().any(|r| r.pattern == "quartz_scheduled_job"),
            "should not detect Quartz without 'quartz' keyword"
        );
    }

    // ── detect_background_service_patterns: hangfire ─────────────────────────

    #[test]
    fn detects_hangfire_background_job_enqueue() {
        let source = r#"BackgroundJob.Enqueue(() => SendEmail(userId));"#;
        let results = detect_background_service_patterns(source, "EmailService.cs", "cs");
        assert!(
            results.iter().any(|r| r.pattern == "hangfire_job"),
            "should detect BackgroundJob.Enqueue"
        );
    }

    #[test]
    fn detects_hangfire_recurring_job() {
        let source =
            r#"RecurringJob.AddOrUpdate("daily-report", () => GenerateReport(), Cron.Daily);"#;
        let results = detect_background_service_patterns(source, "Scheduler.cs", "cs");
        assert!(
            results.iter().any(|r| r.pattern == "hangfire_job"),
            "should detect RecurringJob.AddOrUpdate"
        );
    }

    #[test]
    fn hangfire_modern_equivalent_mentions_hangfire() {
        let source = "BackgroundJob.Enqueue(() => Process());";
        let results = detect_background_service_patterns(source, "Job.cs", "cs");
        let r = results
            .iter()
            .find(|r| r.pattern == "hangfire_job")
            .unwrap();
        assert!(r.modern_equivalent.to_lowercase().contains("hangfire"));
    }

    // ── detect_background_service_patterns: timer service ────────────────────

    #[test]
    fn detects_timer_service_with_service_base() {
        let source = r#"
Inherits ServiceBase
Dim timer As New System.Timers.Timer(60000)
Protected Overrides Sub OnStart(args() As String)
    timer.Start()
End Sub
"#;
        let results = detect_background_service_patterns(source, "TimerSvc.vb", "vb");
        assert!(
            results.iter().any(|r| r.pattern == "timer_service"),
            "should detect timer service"
        );
    }

    #[test]
    fn timer_without_service_context_not_detected_as_timer_service() {
        // System.Timers.Timer but no ServiceBase or OnStart
        let source = r#"Dim timer As New System.Timers.Timer(5000)
timer.Elapsed += OnTimerElapsed"#;
        let results = detect_background_service_patterns(source, "Utils.cs", "cs");
        assert!(
            !results.iter().any(|r| r.pattern == "timer_service"),
            "standalone timer without service context should not trigger timer_service"
        );
    }

    // ── detect_background_service_patterns: no patterns ──────────────────────

    #[test]
    fn no_background_patterns_in_regular_code() {
        let source = r#"
public class OrderController : Controller
{
    public ActionResult Index() { return View(); }
}
"#;
        let results = detect_background_service_patterns(source, "OrderController.cs", "cs");
        assert!(
            results.is_empty(),
            "regular controller should have no background patterns"
        );
    }

    // ── DesignAntiPattern struct fields ──────────────────────────────────────

    #[test]
    fn design_anti_pattern_fields_accessible() {
        let p = DesignAntiPattern {
            pattern_name: "Session Soup".into(),
            description: "Session key 'UserId' accessed from 10 files".into(),
            severity: AntiPatternSeverity::Moderate,
            affected_nodes: vec!["state:Session:UserId".into()],
            evidence: vec!["file:Search.aspx.vb".into(), "file:Orders.aspx.vb".into()],
            modern_target: "JWT claims".into(),
            refactoring_steps: vec!["Identify lifecycle".into(), "Create DTO".into()],
        };
        assert_eq!(p.pattern_name, "Session Soup");
        assert_eq!(p.severity, AntiPatternSeverity::Moderate);
        assert_eq!(p.evidence.len(), 2);
        assert_eq!(p.refactoring_steps.len(), 2);
        assert!(p.modern_target.contains("JWT"));
    }

    #[test]
    fn detected_service_pattern_struct_fields() {
        let sp = DetectedServicePattern {
            pattern: "windows_service".into(),
            file_path: "src/Svc.vb".into(),
            modern_equivalent: "BackgroundService".into(),
            evidence: "Inherits ServiceBase".into(),
        };
        assert_eq!(sp.pattern, "windows_service");
        assert_eq!(sp.file_path, "src/Svc.vb");
        assert!(sp.evidence.contains("ServiceBase"));
    }

    // ── severity scoring logic ────────────────────────────────────────────────

    #[test]
    fn severity_is_severe_when_double_threshold() {
        // For god object: 20 children > god_threshold(5) * 2 = 10 → Severe
        // We can't call detect_design_antipatterns on a real graph easily,
        // but we can test the severity logic inline by constructing a mock scenario
        // via the public DesignAntiPattern struct.
        let god_threshold = 5usize;
        let children_count = 20usize;
        let severity = if children_count > god_threshold * 2 {
            AntiPatternSeverity::Severe
        } else {
            AntiPatternSeverity::Moderate
        };
        assert_eq!(severity, AntiPatternSeverity::Severe);
    }

    #[test]
    fn severity_is_moderate_when_between_threshold_and_double() {
        let god_threshold = 5usize;
        let children_count = 8usize; // > 5 but <= 10
        let severity = if children_count > god_threshold * 2 {
            AntiPatternSeverity::Severe
        } else {
            AntiPatternSeverity::Moderate
        };
        assert_eq!(severity, AntiPatternSeverity::Moderate);
    }

    // ── file_path is stored in DetectedServicePattern ─────────────────────────

    #[test]
    fn detect_background_service_stores_file_path() {
        let source = "Inherits ServiceBase";
        let results = detect_background_service_patterns(source, "Services/MySvc.vb", "vb");
        let r = results
            .iter()
            .find(|r| r.pattern == "windows_service")
            .unwrap();
        assert_eq!(r.file_path, "Services/MySvc.vb");
    }
}
