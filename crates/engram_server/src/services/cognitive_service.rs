//! Cognitive service: dreaming, immune system, style mimicry, temporal coupling.
//!
//! Ported from v1 Python:
//!  - dreaming.py  → dream_project(), analyze_file_style(), find_temporal_couplings()
//!  - generation.py → LLM-backed insight generation (via DreamingEngine)
//!  - immune actor  → anti-pattern indexing (see actors/immune.rs)

use crate::actors::dreamer::{dream_once, record_cooccurrence};
use crate::state::{AppState, SearchHitLite};
use engram_core::RelPath;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Dreaming
// ---------------------------------------------------------------------------

/// Trigger a manual dream cycle for a project.
///
/// Equivalent to v1's `dream_project()` tool handler.
/// Returns the number of new insights generated.
pub async fn dream_project(state: &AppState, project_id: &str) -> anyhow::Result<usize> {
    dream_once(state, project_id, 2, 3, 5).await
}

/// Record a batch of search hits as co-occurrence data (feeds the dreamer).
pub async fn record_search_session(
    state: &AppState,
    project_id: &str,
    hits: &[SearchHitLite],
) -> anyhow::Result<()> {
    record_cooccurrence(state, project_id, hits).await
}

// ---------------------------------------------------------------------------
// Style analysis (v1 dreaming.py::analyze_file_style)
// ---------------------------------------------------------------------------

/// Result of a style analysis for a single file.
#[derive(Debug, Clone)]
pub struct StyleAnalysisResult {
    /// Actionable style guide bullets (None if insufficient data).
    pub style_guide: Option<String>,
    /// Commit hashes that were analyzed.
    pub analyzed_commits: Vec<String>,
    /// File path that was analyzed.
    pub file_path: String,
    /// Error message if analysis failed.
    pub error: Option<String>,
}

/// Analyze the coding style of a file from its recent git history.
///
/// Uses recent diffs + LLM (if configured) to extract naming conventions,
/// error-handling patterns, import style, etc. — identical to v1's
/// `analyze_file_style()` in dreaming.py.
pub async fn analyze_file_style(
    state: &AppState,
    project_id: &str,
    file_path: &str,
    diff_limit: usize,
) -> StyleAnalysisResult {
    // Locate the project directory.
    let pid = project_id.to_string();
    let reg = state.registry.clone();
    let rec = match tokio::task::spawn_blocking(move || reg.get_project(&pid))
        .await
        .unwrap_or_else(|_| Ok(None))
    {
        Ok(Some(r)) => r,
        _ => {
            return StyleAnalysisResult {
                style_guide: None,
                analyzed_commits: Vec::new(),
                file_path: file_path.to_string(),
                error: Some(format!("project '{project_id}' not found")),
            };
        }
    };

    let directory = std::path::PathBuf::from(&rec.directory);
    let fp = file_path.to_string();
    let limit = diff_limit;

    // CPU-bound git I/O.
    let diffs: Vec<(String, String, String)> =
        tokio::task::spawn_blocking(move || collect_file_diffs(&directory, &fp, limit))
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default();

    if diffs.is_empty() {
        return StyleAnalysisResult {
            style_guide: None,
            analyzed_commits: Vec::new(),
            file_path: file_path.to_string(),
            error: Some("No git history found for this file".into()),
        };
    }

    let analyzed_commits: Vec<String> = diffs.iter().map(|(h, _, _)| h.clone()).collect();

    // Build a combined diff text block (same as v1).
    let mut diffs_text = String::new();
    for (commit_hash, message, diff_content) in &diffs {
        let truncated = if diff_content.len() > 2000 {
            format!("{}\n... (truncated)", &diff_content[..2000])
        } else {
            diff_content.clone()
        };
        diffs_text.push_str(&format!(
            "Commit: {}\nMessage: {}\nDiff:\n{}\n{}\n",
            &commit_hash[..8.min(commit_hash.len())],
            message,
            truncated,
            "-".repeat(40)
        ));
    }

    // Try the AST/regex-based mimicry engine first (always available).
    // Then optionally enhance with LLM if configured.
    let diff_snippets: Vec<String> = diffs.iter().map(|(_, _, d)| d.clone()).collect();
    let mimicry_guide = state
        .mimicry
        .analyze(&diff_snippets, Some(file_path))
        .bullets
        .join("\n");

    // Try LLM enhancement with the style-analysis prompt.
    let llm_guide = try_llm_style_analysis(state, file_path, &diffs_text).await;

    let style_guide = match (llm_guide, mimicry_guide.is_empty()) {
        (Some(llm), _) => Some(llm),
        (None, false) => Some(mimicry_guide),
        (None, true) => None,
    };

    if style_guide.is_none() {
        return StyleAnalysisResult {
            style_guide: None,
            analyzed_commits,
            file_path: file_path.to_string(),
            error: Some("Insufficient data to determine style patterns".into()),
        };
    }

    StyleAnalysisResult {
        style_guide,
        analyzed_commits,
        file_path: file_path.to_string(),
        error: None,
    }
}

/// Prompt template from v1 dreaming.py STYLE_ANALYSIS_PROMPT.
const STYLE_ANALYSIS_PROMPT: &str = r#"You are a code style analyzer. You will be given recent git diffs for a file.
Your task is to extract the coding patterns and conventions used in this file.

Recent changes to {file_path}:

{diffs}

Analyze the above changes and extract:
1. Naming conventions (e.g., snake_case, camelCase, PascalCase)
2. Common patterns (e.g., validation before DB calls, specific libraries/frameworks used)
3. Code organization patterns (e.g., class structure, function ordering)
4. Error handling approaches (e.g., try/except, return None, raise exceptions)
5. Import style (e.g., from X import Y vs import X.Y)
6. Documentation style (e.g., docstrings, inline comments)

Format your response as a concise style guide (3-5 bullet points) that could be prepended to an AI agent's context.
Focus on actionable patterns, not generic advice.

If there are insufficient changes to determine a clear pattern, respond with "INSUFFICIENT_DATA".

Style Guide:
"#;

async fn try_llm_style_analysis(
    state: &AppState,
    file_path: &str,
    diffs_text: &str,
) -> Option<String> {
    let prompt = STYLE_ANALYSIS_PROMPT
        .replace("{file_path}", file_path)
        .replace("{diffs}", diffs_text);

    let result = state
        .dreaming
        .generate_insight("", &prompt, Duration::from_secs(30))
        .await;

    if result.is_empty() || result.contains("INSUFFICIENT_DATA") {
        return None;
    }

    Some(result.trim().to_string())
}

// ---------------------------------------------------------------------------
// Temporal coupling (v1 dreaming.py::find_temporal_couplings)
// ---------------------------------------------------------------------------

/// A pair of files that frequently change together.
#[derive(Debug, Clone)]
pub struct TemporalCoupling {
    pub file_a: String,
    pub file_b: String,
    /// Number of commits where both files changed.
    pub frequency: usize,
}

/// Find files that frequently change together in git history.
///
/// This implements v1's "Temporal Coupling Detection" — ported from
/// dreaming.py::find_temporal_couplings(). Uses the graph's weighted
/// co-change edges that are updated by the git indexer.
pub async fn find_temporal_couplings(
    state: &AppState,
    project_id: &str,
    min_frequency: u32,
    limit: usize,
) -> anyhow::Result<Vec<TemporalCoupling>> {
    let graph = state.graph.clone();
    let pid = project_id.to_string();

    tokio::task::spawn_blocking(move || {
        find_temporal_couplings_blocking(&graph, &pid, min_frequency, limit)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("spawn_blocking panicked: {e}")))
}

fn find_temporal_couplings_blocking(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    min_frequency: u32,
    limit: usize,
) -> anyhow::Result<Vec<TemporalCoupling>> {
    // Query temporal-coupling edges from the graph (set during git history indexing).
    let raw = engram_graph::algorithms::coupling::top_project_couplings(graph, project_id, limit)?;

    let couplings = raw
        .into_iter()
        .filter(|c| c.weight >= min_frequency)
        .map(|c| TemporalCoupling {
            file_a: c.file_node_id,
            file_b: c.neighbor_node_id,
            frequency: c.weight as usize,
        })
        .collect();

    Ok(couplings)
}

// ---------------------------------------------------------------------------
// Git diff collection (CPU-bound helper)
// ---------------------------------------------------------------------------

/// Collect the N most recent diffs for a specific file from the git repo.
/// Returns Vec of (commit_hash, commit_message, diff_text).
fn collect_file_diffs(
    directory: &std::path::Path,
    file_path: &str,
    limit: usize,
) -> anyhow::Result<Vec<(String, String, String)>> {
    use engram_git::{GitWalker, history::MergeCommitPolicy};
    use tokio_util::sync::CancellationToken;

    let repo = GitWalker::open_repo(directory)?;
    let cancel = CancellationToken::new();

    // Walk up to limit*4 commits to find `limit` diffs for this file.
    let oids = GitWalker::walk_new_commits(
        &repo,
        None,
        limit * 4,
        MergeCommitPolicy::FirstParentOnly,
        &cancel,
    )?;

    let target = RelPath::new(file_path);
    let mut out: Vec<(String, String, String)> = Vec::new();

    for oid in oids.iter().rev() {
        if out.len() >= limit {
            break;
        }
        let commit = repo.find_commit(*oid)?;
        let changed = GitWalker::files_changed_in_commit(&repo, *oid)?;
        let touches_file = changed.iter().any(|fc| fc.path() == &target);
        if !touches_file {
            continue;
        }

        let per_file = GitWalker::diff_text_for_commit(&repo, *oid, 4096)?;
        for (path, diff_text) in per_file {
            if path == target && !diff_text.trim().is_empty() {
                let msg = commit.message().unwrap_or("").to_string();
                out.push((oid.to_string(), msg, diff_text));
                break;
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Architecture Mimicry: migration boundary suggestions
// ---------------------------------------------------------------------------

/// Suggest microservice/bounded-context migration boundaries.
///
/// Steps:
/// 1. Query top temporal coupling edges from the graph.
/// 2. Group coupled files into clusters using simple union-find.
/// 3. For each cluster, gather shared state keys and SQL table references.
/// 4. Format text blocks and pass to LLM via DreamingEngine.
/// 5. Return proposed boundaries.
pub async fn suggest_migration_boundaries(
    state: &AppState,
    project_id: &str,
    min_frequency: u32,
    max_clusters: usize,
) -> anyhow::Result<Vec<engram_ml::MigrationBoundary>> {
    let graph = state.graph.clone();
    let pid = project_id.to_string();
    let min_freq = min_frequency;
    let max_cl = max_clusters;

    // Step 1-3: Gather graph data (blocking I/O).
    let (clusters_text, state_text, tables_text) =
        tokio::task::spawn_blocking(move || gather_boundary_data(&graph, &pid, min_freq, max_cl))
            .await
            .unwrap_or_else(|_| Ok(("".into(), "".into(), "".into())))?;

    if clusters_text.is_empty() {
        return Ok(Vec::new());
    }

    // Step 4: Call LLM / fallback.
    let boundaries = state
        .dreaming
        .suggest_boundaries(
            &clusters_text,
            &state_text,
            &tables_text,
            std::time::Duration::from_secs(120),
        )
        .await;

    Ok(boundaries)
}

/// Gather temporal coupling clusters + state/SQL context from the graph.
fn gather_boundary_data(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    min_frequency: u32,
    max_clusters: usize,
) -> anyhow::Result<(String, String, String)> {
    use std::collections::{HashMap, HashSet};

    // Step 1: Get temporal coupling edges.
    let coupling_edges =
        graph.list_edges_by_kind(project_id, engram_graph::EdgeKind::TemporalCoupling, 500)?;

    if coupling_edges.is_empty() {
        return Ok((String::new(), String::new(), String::new()));
    }

    // Filter by minimum frequency (weight).
    let strong_edges: Vec<_> = coupling_edges
        .iter()
        .filter(|e| e.weight >= min_frequency)
        .collect();

    if strong_edges.is_empty() {
        return Ok((String::new(), String::new(), String::new()));
    }

    // Step 2: Simple union-find grouping.
    let mut parent: HashMap<String, String> = HashMap::new();

    fn find(parent: &mut HashMap<String, String>, x: &str) -> String {
        if !parent.contains_key(x) {
            parent.insert(x.to_string(), x.to_string());
            return x.to_string();
        }
        let p = parent[x].clone();
        if p == x {
            return x.to_string();
        }
        let root = find(parent, &p);
        parent.insert(x.to_string(), root.clone());
        root
    }

    fn union(parent: &mut HashMap<String, String>, a: &str, b: &str) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent.insert(ra, rb);
        }
    }

    for e in &strong_edges {
        union(&mut parent, &e.source_id, &e.target_id);
    }

    // Collect clusters.
    let mut clusters: HashMap<String, HashSet<String>> = HashMap::new();
    for key in parent.keys().cloned().collect::<Vec<_>>() {
        let root = find(&mut parent, &key);
        clusters.entry(root).or_default().insert(key);
    }

    // Sort clusters by size (largest first), limit count.
    let mut sorted_clusters: Vec<Vec<String>> = clusters
        .into_values()
        .map(|s| {
            let mut v: Vec<_> = s.into_iter().collect();
            v.sort();
            v
        })
        .collect();
    sorted_clusters.sort_by(|a, b| b.len().cmp(&a.len()));
    sorted_clusters.truncate(max_clusters);

    // Format clusters text.
    let mut clusters_text = String::new();
    for (i, cluster) in sorted_clusters.iter().enumerate() {
        clusters_text.push_str(&format!(
            "Cluster {} ({} files): {}\n",
            i + 1,
            cluster.len(),
            cluster.join(", ")
        ));
    }

    // Step 3: Gather shared state keys and SQL tables.
    let all_files: HashSet<&str> = sorted_clusters
        .iter()
        .flat_map(|c| c.iter().map(|s| s.as_str()))
        .collect();

    let mut state_keys: HashSet<String> = HashSet::new();
    let mut tables: HashSet<String> = HashSet::new();

    // Query state edges.
    if let Ok(state_edges) =
        graph.list_edges_by_kind(project_id, engram_graph::EdgeKind::ReadsState, 200)
    {
        for e in &state_edges {
            if all_files.contains(e.source_id.as_str()) {
                state_keys.insert(e.target_id.clone());
            }
        }
    }
    if let Ok(state_edges) =
        graph.list_edges_by_kind(project_id, engram_graph::EdgeKind::WritesState, 200)
    {
        for e in &state_edges {
            if all_files.contains(e.source_id.as_str()) {
                state_keys.insert(e.target_id.clone());
            }
        }
    }

    // Query SQL table references.
    if let Ok(sql_edges) =
        graph.list_edges_by_kind(project_id, engram_graph::EdgeKind::QueriesTable, 200)
    {
        for e in &sql_edges {
            if all_files.contains(e.source_id.as_str()) {
                tables.insert(e.target_id.clone());
            }
        }
    }

    let state_text = if state_keys.is_empty() {
        "(none detected)".to_string()
    } else {
        let mut keys: Vec<_> = state_keys.into_iter().collect();
        keys.sort();
        keys.join(", ")
    };

    let tables_text = if tables.is_empty() {
        "(none detected)".to_string()
    } else {
        let mut t: Vec<_> = tables.into_iter().collect();
        t.sort();
        t.join(", ")
    };

    Ok((clusters_text, state_text, tables_text))
}
