//! Migration execution workflow — extends blueprint generation to executable wave plans.
//!
//! Provides:
//! - **Seam identification**: Find boundaries between old and new code
//! - **Wave planning**: Order migration tasks by dependency and risk
//! - **Contract tests**: Generate interface contracts between migrated and legacy code
//! - **Compatibility adapters**: Template adapter patterns for legacy/modern interop
//! - **Rollback playbooks**: Generate rollback procedures for each wave

use engram_index::solution_parser::{self, SolutionStructure};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A complete migration execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub project_id: String,
    pub total_waves: usize,
    pub waves: Vec<MigrationWave>,
    pub seams: Vec<Seam>,
    pub rollback_playbook: RollbackPlaybook,
    pub risk_summary: RiskSummary,
    pub generated_at_ms: u64,
}

/// A single wave of migration work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationWave {
    pub wave_number: usize,
    pub name: String,
    pub description: String,
    /// Files/modules to migrate in this wave.
    pub items: Vec<MigrationItem>,
    /// Dependency waves that must complete first.
    pub depends_on: Vec<usize>,
    /// Contract tests to create for this wave.
    pub contract_tests: Vec<ContractTest>,
    /// Adapters needed during this wave.
    pub adapters: Vec<CompatibilityAdapter>,
    /// Risk level for this wave.
    pub risk_level: WaveRisk,
    /// Estimated effort (story points or relative units).
    pub estimated_effort: u32,
    /// Phase 31: Which project this wave targets (from solution structure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_scope: Option<String>,
    /// Phase 31: Projects this wave depends on being migrated first.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cross_project_deps: Vec<String>,
}

/// A single item (file, module, component) within a migration wave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationItem {
    pub path: String,
    pub item_type: MigrationItemType,
    pub complexity: Complexity,
    pub dependencies: Vec<String>,
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationItemType {
    Page,
    Component,
    Service,
    DataAccess,
    Configuration,
    SharedLibrary,
    StaticAsset,
    DatabaseMigration,
}

impl std::fmt::Display for MigrationItemType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Page => write!(f, "page"),
            Self::Component => write!(f, "component"),
            Self::Service => write!(f, "service"),
            Self::DataAccess => write!(f, "data_access"),
            Self::Configuration => write!(f, "configuration"),
            Self::SharedLibrary => write!(f, "shared_library"),
            Self::StaticAsset => write!(f, "static_asset"),
            Self::DatabaseMigration => write!(f, "database_migration"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Complexity {
    Low,
    Medium,
    High,
    VeryHigh,
}

impl std::fmt::Display for Complexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::VeryHigh => write!(f, "very_high"),
        }
    }
}

/// A seam: boundary between legacy and migrated code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seam {
    pub seam_id: String,
    /// Legacy-side file/symbol.
    pub legacy_endpoint: String,
    /// Modern-side file/symbol (to be created).
    pub modern_endpoint: String,
    /// Type of seam (API, data, UI, event).
    pub seam_type: SeamType,
    /// Interface contract describing the communication protocol.
    pub contract: String,
    /// Adapter pattern to bridge old and new.
    pub adapter_pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeamType {
    Api,
    Data,
    Ui,
    Event,
    Config,
}

impl std::fmt::Display for SeamType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api => write!(f, "api"),
            Self::Data => write!(f, "data"),
            Self::Ui => write!(f, "ui"),
            Self::Event => write!(f, "event"),
            Self::Config => write!(f, "config"),
        }
    }
}

/// Contract test specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractTest {
    pub test_name: String,
    pub description: String,
    pub legacy_behavior: String,
    pub expected_modern_behavior: String,
    pub test_template: String,
}

/// Adapter pattern for compatibility between legacy and migrated code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilityAdapter {
    pub name: String,
    pub adapter_type: AdapterType,
    pub legacy_interface: String,
    pub modern_interface: String,
    pub template_code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterType {
    /// Wraps legacy API behind modern interface.
    Facade,
    /// Translates data between old and new formats.
    Translator,
    /// Proxies calls between legacy and modern services.
    Proxy,
    /// Manages state synchronization during coexistence.
    StateBridge,
    /// Adapts authentication/session between systems.
    AuthBridge,
}

impl std::fmt::Display for AdapterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Facade => write!(f, "facade"),
            Self::Translator => write!(f, "translator"),
            Self::Proxy => write!(f, "proxy"),
            Self::StateBridge => write!(f, "state_bridge"),
            Self::AuthBridge => write!(f, "auth_bridge"),
        }
    }
}

/// Risk assessment for a migration wave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaveRisk {
    Low,
    Medium,
    High,
    Critical,
}

impl std::fmt::Display for WaveRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Rollback playbook for the entire migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackPlaybook {
    pub waves: Vec<WaveRollback>,
    pub global_rollback_steps: Vec<String>,
}

/// Rollback procedure for a specific wave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaveRollback {
    pub wave_number: usize,
    pub steps: Vec<RollbackStep>,
    pub verification: Vec<String>,
    pub estimated_rollback_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackStep {
    pub order: usize,
    pub action: String,
    pub target: String,
    pub command_template: String,
}

/// Overall risk summary for the migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskSummary {
    pub total_items: usize,
    pub high_risk_items: usize,
    pub cross_boundary_deps: usize,
    pub database_changes: usize,
    pub global_state_touches: usize,
    pub estimated_total_effort: u32,
    pub recommendation: String,
}

// ---------------------------------------------------------------------------
// Plan generation
// ---------------------------------------------------------------------------

/// Input for generating a migration execution plan.
pub struct PlanInput {
    pub project_id: String,
    /// Boundary clusters from suggest_migration_boundaries.
    pub boundaries: Vec<BoundaryCluster>,
    /// Graph edges across boundaries.
    pub cross_boundary_edges: Vec<CrossBoundaryEdge>,
    /// Files that touch global state.
    pub global_state_files: Vec<String>,
    /// Files that touch databases.
    pub database_files: Vec<String>,
    /// Current timestamp.
    pub timestamp_ms: u64,
    /// Phase 31: Optional parsed solution structure for project-aware wave planning.
    pub solution_structure: Option<SolutionStructure>,
}

pub struct BoundaryCluster {
    pub cluster_id: String,
    pub name: String,
    pub files: Vec<String>,
    pub internal_edges: usize,
    pub shared_across: Vec<String>,
}

pub struct CrossBoundaryEdge {
    pub source_cluster: String,
    pub target_cluster: String,
    pub source_file: String,
    pub target_file: String,
    pub edge_kind: String,
}

/// Generate a full migration execution plan from boundary analysis.
pub fn generate_migration_plan(input: &PlanInput) -> MigrationPlan {
    let mut waves: Vec<MigrationWave> = Vec::new();
    let mut seams: Vec<Seam> = Vec::new();

    // --- Phase 1: Topological sort of clusters by dependency ---
    let cluster_order = topological_sort_clusters(&input.boundaries, &input.cross_boundary_edges);

    // --- Phase 2: Build waves from sorted clusters ---
    // Wave 0: Shared infrastructure (clusters referenced by many others)
    let shared_clusters: Vec<&BoundaryCluster> = input
        .boundaries
        .iter()
        .filter(|c| c.shared_across.len() >= 2)
        .collect();

    if !shared_clusters.is_empty() {
        let items: Vec<MigrationItem> = shared_clusters
            .iter()
            .flat_map(|c| {
                c.files.iter().map(|f| {
                    let is_db = input.database_files.contains(f);
                    let is_state = input.global_state_files.contains(f);
                    MigrationItem {
                        path: f.clone(),
                        item_type: if is_db {
                            MigrationItemType::DataAccess
                        } else if f.ends_with(".config") || f.ends_with(".xml") {
                            MigrationItemType::Configuration
                        } else {
                            MigrationItemType::SharedLibrary
                        },
                        complexity: if is_db || is_state {
                            Complexity::High
                        } else {
                            Complexity::Medium
                        },
                        dependencies: vec![],
                        notes: if !c.shared_across.is_empty() {
                            format!("Shared by: {}", c.shared_across.join(", "))
                        } else {
                            String::new()
                        },
                    }
                })
            })
            .collect();

        waves.push(MigrationWave {
            wave_number: 0,
            name: "Shared Infrastructure".into(),
            description: "Migrate shared libraries, data access, and configuration first".into(),
            items,
            depends_on: vec![],
            contract_tests: vec![],
            adapters: vec![],
            risk_level: WaveRisk::High,
            estimated_effort: 8,
            project_scope: None,
            cross_project_deps: vec![],
        });
    }

    // Subsequent waves: one per cluster (or merged for small clusters)
    let wave_offset = if shared_clusters.is_empty() { 0 } else { 1 };
    for (i, cluster_id) in cluster_order.iter().enumerate() {
        let cluster = match input
            .boundaries
            .iter()
            .find(|c| c.cluster_id == *cluster_id)
        {
            Some(c) => c,
            None => continue,
        };

        // Skip clusters already in wave 0
        if cluster.shared_across.len() >= 2 {
            continue;
        }

        let wave_num = i + wave_offset;

        // Determine dependencies (which waves must complete first)
        let deps: Vec<usize> = input
            .cross_boundary_edges
            .iter()
            .filter(|e| e.target_cluster == *cluster_id)
            .filter_map(|e| {
                cluster_order
                    .iter()
                    .position(|c| *c == e.source_cluster)
                    .map(|pos| pos + wave_offset)
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let items: Vec<MigrationItem> = cluster
            .files
            .iter()
            .map(|f| {
                let is_db = input.database_files.contains(f);
                let is_state = input.global_state_files.contains(f);
                let item_type = classify_file(f);
                MigrationItem {
                    path: f.clone(),
                    item_type,
                    complexity: if is_db {
                        Complexity::High
                    } else if is_state {
                        Complexity::Medium
                    } else {
                        Complexity::Low
                    },
                    dependencies: vec![],
                    notes: String::new(),
                }
            })
            .collect();

        // Generate seams for cross-boundary edges into this cluster
        for edge in &input.cross_boundary_edges {
            if edge.target_cluster == *cluster_id {
                seams.push(Seam {
                    seam_id: format!("seam-{}-{}", edge.source_cluster, cluster_id),
                    legacy_endpoint: edge.source_file.clone(),
                    modern_endpoint: edge.target_file.clone(),
                    seam_type: classify_seam_type(&edge.edge_kind),
                    contract: format!(
                        "Contract: {} calls {} via {}",
                        edge.source_file, edge.target_file, edge.edge_kind
                    ),
                    adapter_pattern: suggest_adapter_pattern(&edge.edge_kind),
                });
            }
        }

        // Generate contract tests
        let contract_tests: Vec<ContractTest> = seams
            .iter()
            .filter(|s| {
                s.modern_endpoint == cluster.name || cluster.files.contains(&s.modern_endpoint)
            })
            .take(5) // Limit contract tests per wave
            .map(|s| ContractTest {
                test_name: format!("test_{}_contract", s.seam_id.replace('-', "_")),
                description: format!(
                    "Verify {} maintains behavior from {}",
                    s.modern_endpoint, s.legacy_endpoint
                ),
                legacy_behavior: format!(
                    "Legacy: {} provides data via {}",
                    s.legacy_endpoint, s.seam_type
                ),
                expected_modern_behavior: format!(
                    "Modern: {} must provide identical interface",
                    s.modern_endpoint
                ),
                test_template: generate_contract_test_template(s),
            })
            .collect();

        // Generate compatibility adapters for cross-boundary edges
        let adapters: Vec<CompatibilityAdapter> = seams
            .iter()
            .filter(|s| cluster.files.contains(&s.modern_endpoint))
            .take(3)
            .map(|s| {
                let adapter_type = match s.seam_type {
                    SeamType::Api => AdapterType::Facade,
                    SeamType::Data => AdapterType::Translator,
                    SeamType::Event => AdapterType::Proxy,
                    SeamType::Config => AdapterType::StateBridge,
                    SeamType::Ui => AdapterType::Facade,
                };
                CompatibilityAdapter {
                    name: format!("{}Adapter", to_pascal_case(&s.seam_id)),
                    adapter_type,
                    legacy_interface: s.legacy_endpoint.clone(),
                    modern_interface: s.modern_endpoint.clone(),
                    template_code: generate_adapter_template(
                        &s.legacy_endpoint,
                        &s.modern_endpoint,
                        adapter_type,
                    ),
                }
            })
            .collect();

        let risk = if items.iter().any(|i| i.complexity == Complexity::High) {
            WaveRisk::High
        } else if items.iter().any(|i| i.complexity == Complexity::Medium) {
            WaveRisk::Medium
        } else {
            WaveRisk::Low
        };

        let effort: u32 = items
            .iter()
            .map(|i| match i.complexity {
                Complexity::Low => 1,
                Complexity::Medium => 3,
                Complexity::High => 5,
                Complexity::VeryHigh => 8,
            })
            .sum();

        // Phase 31: Resolve project scope and cross-project deps from solution structure
        let (project_scope, cross_project_deps) = if let Some(ref sln) = input.solution_structure {
            // Determine which project this cluster's files belong to
            let scope = cluster
                .files
                .first()
                .and_then(|f| solution_parser::file_to_project(sln, f))
                .map(|s| s.to_string());

            // Find cross-project dependencies: projects this cluster depends on
            let cross_deps = if let Some(ref scope_name) = scope {
                sln.dependency_graph
                    .get(scope_name)
                    .cloned()
                    .unwrap_or_default()
            } else {
                vec![]
            };

            (scope, cross_deps)
        } else {
            (Some(cluster.name.clone()), vec![])
        };

        waves.push(MigrationWave {
            wave_number: wave_num,
            name: cluster.name.clone(),
            description: format!("Migrate {} ({} files)", cluster.name, cluster.files.len()),
            items,
            depends_on: deps,
            contract_tests,
            adapters,
            risk_level: risk,
            estimated_effort: effort,
            project_scope,
            cross_project_deps,
        });
    }

    // --- Phase 3: Generate rollback playbook ---
    let rollback_playbook = generate_rollback_playbook(&waves);

    // --- Phase 4: Risk summary ---
    let total_items: usize = waves.iter().map(|w| w.items.len()).sum();
    let high_risk_items = waves
        .iter()
        .flat_map(|w| &w.items)
        .filter(|i| matches!(i.complexity, Complexity::High | Complexity::VeryHigh))
        .count();
    let db_changes = input.database_files.len();
    let state_touches = input.global_state_files.len();
    let total_effort: u32 = waves.iter().map(|w| w.estimated_effort).sum();

    let recommendation = if high_risk_items == 0 {
        "Low risk migration — can proceed with standard review".into()
    } else if high_risk_items <= total_items / 4 {
        "Medium risk — prioritize testing for high-complexity items".into()
    } else {
        "High risk migration — consider phased rollout with feature flags".into()
    };

    MigrationPlan {
        project_id: input.project_id.clone(),
        total_waves: waves.len(),
        waves,
        seams,
        rollback_playbook,
        risk_summary: RiskSummary {
            total_items,
            high_risk_items,
            cross_boundary_deps: input.cross_boundary_edges.len(),
            database_changes: db_changes,
            global_state_touches: state_touches,
            estimated_total_effort: total_effort,
            recommendation,
        },
        generated_at_ms: input.timestamp_ms,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn topological_sort_clusters(
    clusters: &[BoundaryCluster],
    edges: &[CrossBoundaryEdge],
) -> Vec<String> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();

    for c in clusters {
        in_degree.entry(c.cluster_id.clone()).or_insert(0);
        adj.entry(c.cluster_id.clone()).or_default();
    }

    for e in edges {
        *in_degree.entry(e.target_cluster.clone()).or_insert(0) += 1;
        adj.entry(e.source_cluster.clone())
            .or_default()
            .push(e.target_cluster.clone());
    }

    let mut queue: VecDeque<String> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(id, _)| id.clone())
        .collect();

    let mut order = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    while let Some(node) = queue.pop_front() {
        if !visited.insert(node.clone()) {
            continue;
        }
        order.push(node.clone());
        if let Some(neighbors) = adj.get(&node) {
            for n in neighbors {
                if let Some(deg) = in_degree.get_mut(n) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 && !visited.contains(n) {
                        queue.push_back(n.clone());
                    }
                }
            }
        }
    }

    // Add any remaining clusters not reached (cycles)
    for c in clusters {
        if !visited.contains(&c.cluster_id) {
            order.push(c.cluster_id.clone());
        }
    }

    order
}

fn classify_file(path: &str) -> MigrationItemType {
    let lower = path.to_lowercase();
    if lower.ends_with(".aspx") || lower.ends_with(".cshtml") || lower.ends_with(".razor") {
        MigrationItemType::Page
    } else if lower.ends_with(".ascx") {
        MigrationItemType::Component
    } else if lower.contains("service") || lower.ends_with(".asmx") || lower.ends_with(".svc") {
        MigrationItemType::Service
    } else if lower.contains("dal") || lower.contains("repository") || lower.contains("data") {
        MigrationItemType::DataAccess
    } else if lower.ends_with(".config") || lower.ends_with(".json") || lower.ends_with(".yaml") {
        MigrationItemType::Configuration
    } else if lower.ends_with(".js") || lower.ends_with(".css") || lower.ends_with(".png") {
        MigrationItemType::StaticAsset
    } else if lower.contains("migration") || lower.ends_with(".sql") {
        MigrationItemType::DatabaseMigration
    } else {
        MigrationItemType::SharedLibrary
    }
}

fn classify_seam_type(edge_kind: &str) -> SeamType {
    match edge_kind {
        "api_call" | "exposes_web_service" | "exposes_http_handler" | "exposes_wcf_service" => {
            SeamType::Api
        }
        "sql_calls" | "queries_table" | "reads_column" | "data_binding" => SeamType::Data,
        "triggers_postback" | "manipulates_dom" => SeamType::Event,
        "registers_module" | "registers_handler" | "includes_file" => SeamType::Config,
        _ => SeamType::Api,
    }
}

fn suggest_adapter_pattern(edge_kind: &str) -> String {
    match edge_kind {
        "api_call" | "exposes_web_service" => {
            "Create a facade that wraps the legacy ASMX/WCF endpoint with a modern REST API".into()
        }
        "sql_calls" | "queries_table" => {
            "Create a data translator layer that maps legacy ADO.NET results to modern EF Core entities".into()
        }
        "reads_state" | "writes_state" => {
            "Create a state bridge that synchronizes Session/Application state with modern cache".into()
        }
        "triggers_postback" => {
            "Replace __doPostBack with AJAX calls through a proxy adapter".into()
        }
        _ => "Create an adapter following the appropriate pattern for this dependency type".into(),
    }
}

fn generate_contract_test_template(seam: &Seam) -> String {
    format!(
        r#"// Contract test: {test_name}
// Verifies that migrated code maintains the same behavior as legacy
//
// Legacy: {legacy}
// Modern: {modern}
// Seam type: {seam_type}
//
// [Test]
// public async Task {test_name}()
// {{
//     // Arrange: Set up the same input as legacy code
//     var input = CreateTestInput();
//
//     // Act: Call through modern interface
//     var modernResult = await modernService.Process(input);
//
//     // Assert: Verify output matches legacy behavior
//     Assert.Equal(expectedLegacyOutput, modernResult);
// }}"#,
        test_name = seam.seam_id.replace('-', "_"),
        legacy = seam.legacy_endpoint,
        modern = seam.modern_endpoint,
        seam_type = seam.seam_type,
    )
}

fn generate_adapter_template(legacy: &str, modern: &str, adapter_type: AdapterType) -> String {
    match adapter_type {
        AdapterType::Facade => format!(
            "// Facade adapter: wraps {legacy} behind modern interface\n\
             // public class LegacyFacade : IModernService {{\n\
             //     private readonly LegacyService _legacy;\n\
             //     public async Task<Result> Process(Request req) =>\n\
             //         MapToModern(await _legacy.OldMethod(MapToLegacy(req)));\n\
             // }}"
        ),
        AdapterType::Translator => format!(
            "// Data translator: maps {legacy} data to {modern} format\n\
             // public class DataTranslator {{\n\
             //     public ModernEntity Translate(DataRow legacyRow) => new() {{\n\
             //         Id = (int)legacyRow[\"ID\"],\n\
             //         Name = legacyRow[\"Name\"].ToString()\n\
             //     }};\n\
             // }}"
        ),
        AdapterType::Proxy => format!(
            "// Proxy: forwards calls between {legacy} and {modern}\n\
             // public class ServiceProxy : IModernService {{\n\
             //     public async Task<Response> Call(Request req) {{\n\
             //         using var client = new HttpClient();\n\
             //         return await client.PostAsync(legacyUrl, Serialize(req));\n\
             //     }}\n\
             // }}"
        ),
        AdapterType::StateBridge => format!(
            "// State bridge: syncs state between {legacy} and {modern}\n\
             // public class StateBridge {{\n\
             //     public void SyncToModern(HttpSessionState session, IDistributedCache cache) {{\n\
             //         foreach (string key in session.Keys)\n\
             //             cache.Set(key, Serialize(session[key]));\n\
             //     }}\n\
             // }}"
        ),
        AdapterType::AuthBridge => format!(
            "// Auth bridge: maps {legacy} auth to {modern} auth\n\
             // public class AuthBridge {{\n\
             //     public ClaimsPrincipal MapFromFormsAuth(FormsIdentity identity) =>\n\
             //         new(new ClaimsIdentity(ExtractClaims(identity)));\n\
             // }}"
        ),
    }
}

fn generate_rollback_playbook(waves: &[MigrationWave]) -> RollbackPlaybook {
    let wave_rollbacks: Vec<WaveRollback> = waves
        .iter()
        .map(|w| {
            let steps: Vec<RollbackStep> = vec![
                RollbackStep {
                    order: 1,
                    action: "Disable feature flag".into(),
                    target: format!("wave_{}_flag", w.wave_number),
                    command_template: format!(
                        "az appconfig kv set --name config --key wave{}_enabled --value false",
                        w.wave_number
                    ),
                },
                RollbackStep {
                    order: 2,
                    action: "Revert routing".into(),
                    target: "web.config / appsettings.json".into(),
                    command_template: format!(
                        "git revert --no-commit wave-{}-routing",
                        w.wave_number
                    ),
                },
                RollbackStep {
                    order: 3,
                    action: "Restore legacy endpoints".into(),
                    target: format!("wave {} adapters", w.wave_number),
                    command_template: format!(
                        "kubectl rollout undo deployment/wave-{} --namespace=production",
                        w.wave_number
                    ),
                },
                RollbackStep {
                    order: 4,
                    action: "Verify legacy functionality".into(),
                    target: "smoke tests".into(),
                    command_template: "dotnet test --filter Category=Smoke".into(),
                },
            ];
            WaveRollback {
                wave_number: w.wave_number,
                steps,
                verification: vec![
                    "Run contract tests for this wave".into(),
                    "Verify no 500 errors in application logs".into(),
                    "Check legacy endpoint response times".into(),
                ],
                estimated_rollback_time: match w.risk_level {
                    WaveRisk::Low => "< 15 minutes".into(),
                    WaveRisk::Medium => "15-30 minutes".into(),
                    WaveRisk::High => "30-60 minutes".into(),
                    WaveRisk::Critical => "1-2 hours (manual steps required)".into(),
                },
            }
        })
        .collect();

    RollbackPlaybook {
        waves: wave_rollbacks,
        global_rollback_steps: vec![
            "1. Disable all migration feature flags".into(),
            "2. Revert DNS/routing to legacy infrastructure".into(),
            "3. Restore database from pre-migration backup".into(),
            "4. Clear distributed caches".into(),
            "5. Run full smoke test suite against legacy".into(),
            "6. Monitor error rates for 30 minutes".into(),
        ],
    }
}

fn to_pascal_case(s: &str) -> String {
    s.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + &chars.as_str().to_lowercase(),
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn generates_plan_with_waves() {
        let input = PlanInput {
            project_id: "test".into(),
            boundaries: vec![
                BoundaryCluster {
                    cluster_id: "c1".into(),
                    name: "UserModule".into(),
                    files: vec!["Users.aspx".into(), "Users.aspx.cs".into()],
                    internal_edges: 5,
                    shared_across: vec![],
                },
                BoundaryCluster {
                    cluster_id: "c2".into(),
                    name: "OrderModule".into(),
                    files: vec!["Orders.aspx".into(), "Orders.aspx.cs".into()],
                    internal_edges: 3,
                    shared_across: vec![],
                },
            ],
            cross_boundary_edges: vec![CrossBoundaryEdge {
                source_cluster: "c1".into(),
                target_cluster: "c2".into(),
                source_file: "Users.aspx.cs".into(),
                target_file: "Orders.aspx.cs".into(),
                edge_kind: "api_call".into(),
            }],
            global_state_files: vec![],
            database_files: vec![],
            timestamp_ms: 1234567890,
            solution_structure: None,
        };

        let plan = generate_migration_plan(&input);
        assert!(plan.total_waves >= 2);
        assert!(!plan.rollback_playbook.waves.is_empty());
        assert!(!plan.rollback_playbook.global_rollback_steps.is_empty());
    }

    #[test]
    fn shared_clusters_become_wave_zero() {
        let input = PlanInput {
            project_id: "test".into(),
            boundaries: vec![BoundaryCluster {
                cluster_id: "shared".into(),
                name: "SharedLib".into(),
                files: vec!["Utils.cs".into()],
                internal_edges: 2,
                shared_across: vec!["UserModule".into(), "OrderModule".into()],
            }],
            cross_boundary_edges: vec![],
            global_state_files: vec![],
            database_files: vec![],
            timestamp_ms: 1000,
            solution_structure: None,
        };

        let plan = generate_migration_plan(&input);
        assert!(!plan.waves.is_empty());
        assert_eq!(plan.waves[0].wave_number, 0);
        assert_eq!(plan.waves[0].name, "Shared Infrastructure");
    }

    #[test]
    fn topological_sort_handles_cycles() {
        let clusters = vec![
            BoundaryCluster {
                cluster_id: "a".into(),
                name: "A".into(),
                files: vec![],
                internal_edges: 0,
                shared_across: vec![],
            },
            BoundaryCluster {
                cluster_id: "b".into(),
                name: "B".into(),
                files: vec![],
                internal_edges: 0,
                shared_across: vec![],
            },
        ];
        let edges = vec![
            CrossBoundaryEdge {
                source_cluster: "a".into(),
                target_cluster: "b".into(),
                source_file: "".into(),
                target_file: "".into(),
                edge_kind: "".into(),
            },
            CrossBoundaryEdge {
                source_cluster: "b".into(),
                target_cluster: "a".into(),
                source_file: "".into(),
                target_file: "".into(),
                edge_kind: "".into(),
            },
        ];
        let order = topological_sort_clusters(&clusters, &edges);
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn solution_aware_waves_populate_project_scope_and_deps() {
        use engram_index::solution_parser::build_solution_structure;

        let sln_content = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "WebApp", "WebApp\WebApp.csproj", "{AAA}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "DataLayer", "DataLayer\DataLayer.csproj", "{BBB}"
EndProject
"#;

        let webapp_proj = r#"
<Project>
  <PropertyGroup>
    <RootNamespace>MyWebApp</RootNamespace>
    <AssemblyName>WebApp</AssemblyName>
    <TargetFrameworkVersion>v4.7.2</TargetFrameworkVersion>
  </PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="..\DataLayer\DataLayer.csproj" />
  </ItemGroup>
</Project>"#;

        let data_proj = r#"
<Project>
  <PropertyGroup>
    <RootNamespace>DataLayer</RootNamespace>
    <AssemblyName>DataLayer</AssemblyName>
    <TargetFrameworkVersion>v4.7.2</TargetFrameworkVersion>
  </PropertyGroup>
</Project>"#;

        let mut proj_files = std::collections::HashMap::new();
        proj_files.insert("WebApp".to_string(), webapp_proj.to_string());
        proj_files.insert("DataLayer".to_string(), data_proj.to_string());

        let sln = build_solution_structure(sln_content, &proj_files);

        let input = PlanInput {
            project_id: "test".into(),
            boundaries: vec![
                BoundaryCluster {
                    cluster_id: "c1".into(),
                    name: "WebUI".into(),
                    files: vec![
                        "WebApp/Default.aspx".into(),
                        "WebApp/Default.aspx.cs".into(),
                    ],
                    internal_edges: 3,
                    shared_across: vec![],
                },
                BoundaryCluster {
                    cluster_id: "c2".into(),
                    name: "DataAccess".into(),
                    files: vec![
                        "DataLayer/UserRepository.cs".into(),
                        "DataLayer/OrderRepository.cs".into(),
                    ],
                    internal_edges: 2,
                    shared_across: vec![],
                },
            ],
            cross_boundary_edges: vec![CrossBoundaryEdge {
                source_cluster: "c1".into(),
                target_cluster: "c2".into(),
                source_file: "WebApp/Default.aspx.cs".into(),
                target_file: "DataLayer/UserRepository.cs".into(),
                edge_kind: "dependency".into(),
            }],
            global_state_files: vec![],
            database_files: vec![],
            timestamp_ms: 1000,
            solution_structure: Some(sln),
        };

        let plan = generate_migration_plan(&input);

        // Find the wave whose files are under WebApp/
        let webapp_wave = plan.waves.iter().find(|w| {
            w.items
                .iter()
                .any(|i| i.path.contains("WebApp/Default.aspx"))
        });
        assert!(webapp_wave.is_some(), "Should have a wave for WebApp files");

        let wave = webapp_wave.unwrap();
        // project_scope should be "WebApp" (resolved from file_to_project)
        assert_eq!(wave.project_scope.as_deref(), Some("WebApp"));
        // cross_project_deps should include DataLayer (WebApp references DataLayer)
        assert!(
            wave.cross_project_deps.contains(&"DataLayer".to_string()),
            "WebApp wave should have DataLayer as cross-project dep"
        );

        // DataLayer wave should have no cross-project deps
        let data_wave = plan.waves.iter().find(|w| {
            w.items
                .iter()
                .any(|i| i.path.contains("DataLayer/UserRepository"))
        });
        assert!(data_wave.is_some());
        assert!(
            data_wave.unwrap().cross_project_deps.is_empty(),
            "DataLayer should have no cross-project deps"
        );
    }
}
