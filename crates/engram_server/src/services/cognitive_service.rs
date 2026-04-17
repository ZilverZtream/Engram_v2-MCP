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

    // Always read the current file content — the mimicry engine's
    // language-specific detectors and the new static fallback both need
    // the full file body to produce a rich guide. Before this fix the
    // handler passed the short `file_path` string into `current_file`
    // (misusing the parameter contract) so mimicry only ever saw the
    // diff text, and for stable files like `sharedfunc.vb` the git
    // history is thin — yielding a near-empty style guide.
    let file_content: Option<String> = {
        let dir = std::path::PathBuf::from(&rec.directory);
        let fp = file_path.to_string();
        tokio::task::spawn_blocking(move || match engram_core::safe_join(&dir, &fp) {
            Ok(full) => std::fs::read_to_string(full).ok(),
            Err(_) => None,
        })
        .await
        .ok()
        .flatten()
    };

    if diffs.is_empty() && file_content.is_none() {
        return StyleAnalysisResult {
            style_guide: None,
            analyzed_commits: Vec::new(),
            file_path: file_path.to_string(),
            error: Some(
                "No git history found and file could not be read for static fallback".into(),
            ),
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
    // Pass the actual file CONTENT as `current_file` (not the file path,
    // which was the old bug) so the mimicry engine can analyse the body
    // even when git history is shallow. Then optionally enhance with LLM.
    let diff_snippets: Vec<String> = diffs.iter().map(|(_, _, d)| d.clone()).collect();
    let mimicry_guide = state
        .mimicry
        .analyze(&diff_snippets, file_content.as_deref())
        .bullets;

    // Static fallback: when git history is shallow (< 5 diffs) and we
    // have the file body, run a light language-specific static pass
    // that emits concrete patterns (Optional-param shape, Using block
    // discipline, Is-Nothing guards, etc.). Static bullets are MERGED
    // after git/mimicry bullets — git signals take priority because
    // they represent active choices by the team, static fills the gaps.
    const SHALLOW_HISTORY_THRESHOLD: usize = 5;
    let mut merged_bullets: Vec<String> = mimicry_guide;
    if diffs.len() < SHALLOW_HISTORY_THRESHOLD
        && let Some(ref content) = file_content
    {
        let static_bullets = static_analyze_file_style(content, file_path);
        for b in static_bullets {
            // Dedup on exact bullet text — cheap because counts are small.
            if !merged_bullets.iter().any(|existing| existing == &b) {
                merged_bullets.push(b);
            }
        }
    }

    let mimicry_combined = merged_bullets.join("\n");

    // Try LLM enhancement with the style-analysis prompt.
    let llm_guide = try_llm_style_analysis(state, file_path, &diffs_text).await;

    // Merge policy:
    //
    // The deterministic mimicry + static-VB pass produces CONCRETE,
    // verifiable rules ("Optional db As iFaltDataContext = Nothing
    // appears in 14 methods"). The LLM pass produces NARRATIVE context
    // ("the file is a shared-helpers module; team prefers explicit
    // disposal via Using").
    //
    // These complement each other — discarding the rules when the LLM
    // is reachable (the previous behaviour) silently removed the most
    // actionable output from every style report on projects where an
    // LLM backend is configured. The merged output leads with rules
    // (so a downstream agent sees the actionable list first) and
    // appends the LLM narrative as supplementary context.
    let style_guide = match (llm_guide, mimicry_combined.is_empty()) {
        (Some(llm), true) => Some(llm),
        (Some(llm), false) => Some(format!(
            "{mimicry_combined}\n\n---\n### LLM Analysis\n{llm}"
        )),
        (None, false) => Some(mimicry_combined),
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

// ── Static fallback: language-aware pattern detection ────────────────────────
//
// Runs when git history is shallow. For VB.NET files we look for the
// OciusX-style patterns a reader would expect the style guide to call
// out — `Optional db As <Context> = Nothing`, `Using db …`, `Is Nothing`
// guards, Module vs Class declaration, Handles-clause discipline, etc.
// For C# / Rust / other we emit a generic pass. Every detector returns a
// single bullet; detectors that don't fire contribute nothing.

/// Entry point — dispatches on file extension.
pub fn static_analyze_file_style(content: &str, file_path: &str) -> Vec<String> {
    let lower = file_path.to_ascii_lowercase();
    if lower.ends_with(".vb") {
        static_analyze_vb(content)
    } else if lower.ends_with(".cs") {
        static_analyze_cs(content)
    } else {
        static_analyze_generic(content)
    }
}

fn static_analyze_vb(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static METHOD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?im)^\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Sub|Function)\s+(\w+)\s*\(")
            .ok()
    });
    static OPTIONAL_CONTEXT_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r#"(?i)\bOptional\s+(?:ByVal\s+|ByRef\s+)?(\w+)\s+As\s+(\w+(?:DataContext|Context|Db))\s*=\s*Nothing"#)
            .ok()
    });
    static USING_CONTEXT_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r#"(?im)^\s*Using\s+\w+(?:\s+As\s+(?:New\s+)?\w+|\s*=\s*(?:If\(.+?,\s*New\s+\w+)?)"#,
        )
        .ok()
    });
    static IS_NOTHING_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?i)\bIf\s+\w[\w.]*\s+Is\s+Nothing\b").ok());
    static ON_ERROR_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*On\s+Error\s+Resume\s+Next\b").ok());
    static TRY_CATCH_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*Try\s*$|^\s*Catch\b").ok());
    static MODULE_DECL_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*(?:Public\s+|Friend\s+)?Module\s+(\w+)").ok());
    static CLASS_DECL_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?im)^\s*(?:Public\s+|Friend\s+|Partial\s+)*Class\s+(\w+)").ok()
    });
    static HANDLES_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)\bHandles\s+\w[\w.]*(?:\s*,\s*\w[\w.]*)*").ok());
    static SAFEREDIRECT_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)SafeRedirect\s*\([^\)]*\)\s*\n?\s*Return\b").ok());
    static XML_DOC_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"(?m)^\s*'''").ok());
    static IMPORTS_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*Imports\s+([\w.]+)").ok());

    let mut bullets = Vec::new();

    // Method naming convention (PascalCase / camelCase / other).
    if let Some(re) = METHOD_RE.as_ref() {
        let mut pascal = 0u32;
        let mut camel = 0u32;
        let mut other = 0u32;
        let mut samples: Vec<String> = Vec::new();
        for cap in re.captures_iter(content).take(200) {
            if let Some(m) = cap.get(1) {
                let name = m.as_str();
                let first = name.chars().next().unwrap_or('_');
                if first.is_ascii_uppercase() {
                    pascal += 1;
                } else if first.is_ascii_lowercase() {
                    camel += 1;
                } else {
                    other += 1;
                }
                if samples.len() < 3 {
                    samples.push(name.to_string());
                }
            }
        }
        let total = pascal + camel + other;
        if total >= 3 {
            let (conv, count) = if pascal >= camel.max(other) {
                ("PascalCase", pascal)
            } else if camel >= pascal.max(other) {
                ("camelCase", camel)
            } else {
                ("mixed", other)
            };
            bullets.push(format!(
                "Method naming: **{conv}** ({count}/{total} methods). Examples: {}",
                samples.join(", ")
            ));
        }
    }

    // `Optional db As <DataContext> = Nothing` context-injection pattern.
    if let Some(re) = OPTIONAL_CONTEXT_RE.as_ref() {
        let mut hits: Vec<(String, String)> = Vec::new();
        for cap in re.captures_iter(content).take(20) {
            if let (Some(n), Some(t)) = (cap.get(1), cap.get(2)) {
                hits.push((n.as_str().into(), t.as_str().into()));
            }
        }
        if !hits.is_empty() {
            let (param, ctx) = &hits[0];
            bullets.push(format!(
                "Data-context injection: `Optional {param} As {ctx} = Nothing` — \
                 seen in {} method(s). New methods should follow the same shape.",
                hits.len()
            ));
        }
    }

    // Using-block discipline.
    if let Some(re) = USING_CONTEXT_RE.as_ref() {
        let using_count = re.find_iter(content).count();
        if using_count >= 2 {
            bullets.push(format!(
                "`Using` block for context ownership (seen {using_count} times). \
                 Never declare a bare `Dim db As New …Context` without a `Using`."
            ));
        }
    }

    // `Is Nothing` guard before LINQ access.
    if let Some(re) = IS_NOTHING_RE.as_ref() {
        let n = re.find_iter(content).count();
        if n >= 3 {
            bullets.push(format!(
                "Guard pattern: `If x Is Nothing Then …` (seen {n} times) — always \
                 validate nullable references before touching them."
            ));
        }
    }

    // Error-handling: On Error Resume Next vs Try/Catch.
    let on_error = ON_ERROR_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let try_catch = TRY_CATCH_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if try_catch > 0 && on_error == 0 {
        bullets.push(format!(
            "Error handling: **`Try/Catch` only** ({try_catch} occurrences). \
             Do NOT introduce `On Error Resume Next` — keep errors explicit."
        ));
    } else if on_error > 0 {
        bullets.push(format!(
            "Error handling: legacy `On Error Resume Next` present ({on_error}). \
             Flag as risk — prefer migrating to `Try/Catch`."
        ));
    }

    // Module vs Class declaration style.
    let has_module = MODULE_DECL_RE
        .as_ref()
        .and_then(|re| re.captures(content))
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()));
    let has_class = CLASS_DECL_RE
        .as_ref()
        .is_some_and(|re| re.is_match(content));
    if let Some(name) = has_module {
        bullets.push(format!(
            "Declaration style: **`Module {name}`** (shared helpers, no instance state)."
        ));
    } else if has_class {
        bullets.push("Declaration style: `Class` (instance state or inheritance)".into());
    }

    // `Handles` clauses — event wiring style.
    if let Some(re) = HANDLES_RE.as_ref() {
        let n = re.find_iter(content).count();
        if n >= 2 {
            bullets.push(format!(
                "Event wiring: `Handles` clauses ({n} occurrences) — prefer attaching \
                 handlers via `Handles` over `AddHandler` for this file's conventions."
            ));
        }
    }

    // `SafeRedirect(...) : Return` mandatory pair (OciusX convention).
    if let Some(re) = SAFEREDIRECT_RE.as_ref()
        && re.is_match(content)
    {
        bullets.push(
            "`SafeRedirect(...)` MUST be followed by `Return` on the next line — \
             the redirect doesn't short-circuit on its own."
                .into(),
        );
    }

    // XML-doc vs inline comment style.
    if let Some(re) = XML_DOC_RE.as_ref() {
        let doc_lines = re.find_iter(content).count();
        if doc_lines >= 3 {
            bullets.push(format!(
                "Documentation: XML doc comments (`'''`) on public API — {doc_lines} lines. \
                 Follow the same shape for new public methods."
            ));
        }
    }

    // Imports convention — top-N most common namespaces.
    if let Some(re) = IMPORTS_RE.as_ref() {
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for cap in re.captures_iter(content).take(100) {
            if let Some(m) = cap.get(1) {
                *counts.entry(m.as_str().to_string()).or_insert(0) += 1;
            }
        }
        if counts.len() >= 3 {
            let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
            pairs.sort_by(|a, b| b.1.cmp(&a.1));
            let top: Vec<String> = pairs.iter().take(4).map(|(n, _)| n.clone()).collect();
            bullets.push(format!(
                "Imports: top-level `Imports` directives — {}",
                top.join(", ")
            ));
        }
    }

    bullets
}

fn static_analyze_cs(content: &str) -> Vec<String> {
    // C# pass is intentionally lighter — mimicry's AST detectors cover
    // most of the C# shape already. Extend here if OciusX-like C# codebases
    // grow their own conventions worth calling out.
    let mut bullets = Vec::new();
    if content.contains("using (") || content.contains("using var ") {
        bullets.push(
            "Resource ownership: `using` statements for IDisposable handles \
             (keep the pattern for new code)."
                .into(),
        );
    }
    if content.contains("async Task") || content.contains("async ValueTask") {
        bullets.push("Async style: `async Task` / `async ValueTask` methods present.".into());
    }
    bullets
}

fn static_analyze_generic(content: &str) -> Vec<String> {
    // Last-resort: check for trailing whitespace, tab-vs-space preferences
    // that mimicry's `detect_indent` may have missed when diffs were empty.
    let mut bullets = Vec::new();
    let tab_lines = content.lines().filter(|l| l.starts_with('\t')).count();
    let space_lines = content
        .lines()
        .filter(|l| l.starts_with("    ") || l.starts_with("  "))
        .count();
    if tab_lines > space_lines * 2 {
        bullets.push("Indentation: tabs (keep consistent).".into());
    } else if space_lines > tab_lines * 2 {
        bullets.push("Indentation: spaces (keep consistent).".into());
    }
    bullets
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
    let is_dir = std::path::Path::new(directory).join(file_path).is_dir();
    let mut out: Vec<(String, String, String)> = Vec::new();

    for oid in oids.iter().rev() {
        if out.len() >= limit {
            break;
        }
        let commit = repo.find_commit(*oid)?;
        let changed = GitWalker::files_changed_in_commit(&repo, *oid)?;

        let touches_target = changed.iter().any(|fc| {
            let p = fc.path().as_str();
            if is_dir {
                p.starts_with(target.as_str())
            } else {
                fc.path() == &target
            }
        });

        if !touches_target {
            continue;
        }

        let per_file = GitWalker::diff_text_for_commit(&repo, *oid, 4096)?;
        for (path, diff_text) in per_file {
            let matched = if is_dir {
                path.as_str().starts_with(target.as_str())
            } else {
                path == target
            };

            if matched && !diff_text.trim().is_empty() {
                let msg = commit.message().unwrap_or("").to_string();
                out.push((oid.to_string(), msg, diff_text));
                // For directories, we might want multiple files from same commit,
                // but let's stick to one diff text block for simplicity like v1.
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
/// 2. Group coupled files into clusters using iterative union-find with rank heuristic.
/// 3. For each cluster, gather shared state keys and SQL table references.
/// 4. Perform cross-cluster dependency analysis.
/// 5. Format text blocks and pass to LLM via DreamingEngine.
/// 6. Return proposed boundaries with cross-cluster annotations.
pub async fn suggest_migration_boundaries(
    state: &AppState,
    project_id: &str,
    min_frequency: u32,
    max_clusters: usize,
    timeout_secs: u64,
    include_cross_cluster_deps: bool,
) -> anyhow::Result<Vec<engram_ml::MigrationBoundary>> {
    let graph = state.graph.clone();
    let pid = project_id.to_string();
    let min_freq = min_frequency;
    let max_cl = max_clusters;

    // Step 1-3: Gather graph data (blocking I/O).
    let boundary_data =
        tokio::task::spawn_blocking(move || gather_boundary_data(&graph, &pid, min_freq, max_cl))
            .await
            .unwrap_or_else(|_| Ok(BoundaryData::empty()))?;

    if boundary_data.clusters_text.is_empty() {
        return Ok(Vec::new());
    }

    // Step 4-5: Call LLM / fallback with configurable timeout.
    let mut boundaries = state
        .dreaming
        .suggest_boundaries(
            &boundary_data.clusters_text,
            &boundary_data.state_text,
            &boundary_data.tables_text,
            Duration::from_secs(timeout_secs),
        )
        .await;

    // Step 6: Cross-cluster dependency annotation.
    if include_cross_cluster_deps && boundaries.len() > 1 {
        annotate_cross_cluster_deps(&mut boundaries, &boundary_data);
    }

    Ok(boundaries)
}

/// Internal data gathered from the graph for boundary analysis.
struct BoundaryData {
    clusters_text: String,
    state_text: String,
    tables_text: String,
    /// file -> set of state keys it touches.
    file_state_keys: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// file -> set of SQL tables it references.
    file_table_refs: std::collections::HashMap<String, std::collections::HashSet<String>>,
}

impl BoundaryData {
    fn empty() -> Self {
        Self {
            clusters_text: String::new(),
            state_text: String::new(),
            tables_text: String::new(),
            file_state_keys: std::collections::HashMap::new(),
            file_table_refs: std::collections::HashMap::new(),
        }
    }
}

/// Annotate boundaries with cross-cluster shared state/table dependencies.
fn annotate_cross_cluster_deps(
    boundaries: &mut [engram_ml::MigrationBoundary],
    data: &BoundaryData,
) {
    use std::collections::{HashMap, HashSet};

    // Build map: data_key -> set of context indices that reference it.
    let mut data_owners: HashMap<String, HashSet<usize>> = HashMap::new();

    for (idx, boundary) in boundaries.iter().enumerate() {
        let boundary_files: HashSet<&str> = boundary.files.iter().map(|s| s.as_str()).collect();
        // Check which state keys files in this boundary touch.
        for f in &boundary_files {
            if let Some(keys) = data.file_state_keys.get(*f) {
                for k in keys {
                    data_owners
                        .entry(format!("state:{k}"))
                        .or_default()
                        .insert(idx);
                }
            }
            if let Some(tables) = data.file_table_refs.get(*f) {
                for t in tables {
                    data_owners
                        .entry(format!("table:{t}"))
                        .or_default()
                        .insert(idx);
                }
            }
        }
    }

    // For each boundary, find which other boundaries share its data.
    let names: Vec<String> = boundaries.iter().map(|b| b.context_name.clone()).collect();
    for (idx, boundary) in boundaries.iter_mut().enumerate() {
        let mut shared: HashSet<String> = HashSet::new();
        let boundary_files: HashSet<&str> = boundary.files.iter().map(|s| s.as_str()).collect();
        for f in &boundary_files {
            if let Some(keys) = data.file_state_keys.get(*f) {
                for k in keys {
                    if let Some(owners) = data_owners.get(&format!("state:{k}")) {
                        for &o in owners {
                            if o != idx {
                                shared.insert(names[o].clone());
                            }
                        }
                    }
                }
            }
            if let Some(tables) = data.file_table_refs.get(*f) {
                for t in tables {
                    if let Some(owners) = data_owners.get(&format!("table:{t}")) {
                        for &o in owners {
                            if o != idx {
                                shared.insert(names[o].clone());
                            }
                        }
                    }
                }
            }
        }
        if !shared.is_empty() {
            boundary.shared_across = shared.into_iter().collect();
            boundary.shared_across.sort();
            // Elevate risk if data is shared across boundaries.
            if boundary.risk != "HIGH" {
                boundary.risk = "HIGH".into();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Iterative union-find with rank heuristic (no stack overflow risk)
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: std::collections::HashMap<String, String>,
    rank: std::collections::HashMap<String, usize>,
}

impl UnionFind {
    fn new() -> Self {
        Self {
            parent: std::collections::HashMap::new(),
            rank: std::collections::HashMap::new(),
        }
    }

    fn make_set(&mut self, x: &str) {
        if !self.parent.contains_key(x) {
            self.parent.insert(x.to_string(), x.to_string());
            self.rank.insert(x.to_string(), 0);
        }
    }

    /// Iterative find with path compression — O(alpha(n)) amortized, no stack overflow.
    fn find(&mut self, x: &str) -> String {
        self.make_set(x);

        // Walk to root.
        let mut current = x.to_string();
        while self.parent[&current] != current {
            current = self.parent[&current].clone();
        }
        let root = current;

        // Path compression: re-walk and point everything to root.
        let mut current = x.to_string();
        while current != root {
            let next = self.parent[&current].clone();
            self.parent.insert(current, root.clone());
            current = next;
        }

        root
    }

    /// Union by rank: attach smaller tree under larger tree.
    fn union(&mut self, a: &str, b: &str) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        let rank_a = self.rank[&ra];
        let rank_b = self.rank[&rb];
        if rank_a < rank_b {
            self.parent.insert(ra, rb);
        } else if rank_a > rank_b {
            self.parent.insert(rb, ra);
        } else {
            self.parent.insert(rb, ra.clone());
            *self.rank.entry(ra).or_insert(0) += 1;
        }
    }

    fn keys(&self) -> Vec<String> {
        self.parent.keys().cloned().collect()
    }
}

/// Gather temporal coupling clusters + state/SQL context from the graph.
fn gather_boundary_data(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    min_frequency: u32,
    max_clusters: usize,
) -> anyhow::Result<BoundaryData> {
    use std::collections::{HashMap, HashSet};

    // Step 1: Get temporal coupling edges.
    let coupling_edges =
        graph.list_edges_by_kind(project_id, engram_graph::EdgeKind::TemporalCoupling, 500)?;

    if coupling_edges.is_empty() {
        return Ok(BoundaryData::empty());
    }

    // Filter by minimum frequency (weight).
    let strong_edges: Vec<_> = coupling_edges
        .iter()
        .filter(|e| e.weight >= min_frequency)
        .collect();

    if strong_edges.is_empty() {
        return Ok(BoundaryData::empty());
    }

    // Step 2: Iterative union-find with rank heuristic.
    let mut uf = UnionFind::new();
    for e in &strong_edges {
        uf.union(&e.source_id, &e.target_id);
    }

    // Collect clusters.
    let mut clusters: HashMap<String, HashSet<String>> = HashMap::new();
    for key in uf.keys() {
        let root = uf.find(&key);
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
    sorted_clusters.sort_by_key(|b| std::cmp::Reverse(b.len()));
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

    // Step 3: Gather shared state keys and SQL tables (per-file granularity for cross-cluster).
    let all_files: HashSet<&str> = sorted_clusters
        .iter()
        .flat_map(|c| c.iter().map(|s| s.as_str()))
        .collect();

    let mut global_state_keys: HashSet<String> = HashSet::new();
    let mut global_tables: HashSet<String> = HashSet::new();
    let mut file_state_keys: HashMap<String, HashSet<String>> = HashMap::new();
    let mut file_table_refs: HashMap<String, HashSet<String>> = HashMap::new();

    // Query state edges.
    for kind in &[
        engram_graph::EdgeKind::ReadsState,
        engram_graph::EdgeKind::WritesState,
    ] {
        if let Ok(state_edges) = graph.list_edges_by_kind(project_id, kind.clone(), 200) {
            for e in &state_edges {
                if all_files.contains(e.source_id.as_str()) {
                    global_state_keys.insert(e.target_id.clone());
                    file_state_keys
                        .entry(e.source_id.clone())
                        .or_default()
                        .insert(e.target_id.clone());
                }
            }
        }
    }

    // Query SQL table references.
    if let Ok(sql_edges) =
        graph.list_edges_by_kind(project_id, engram_graph::EdgeKind::QueriesTable, 200)
    {
        for e in &sql_edges {
            if all_files.contains(e.source_id.as_str()) {
                global_tables.insert(e.target_id.clone());
                file_table_refs
                    .entry(e.source_id.clone())
                    .or_default()
                    .insert(e.target_id.clone());
            }
        }
    }

    let state_text = if global_state_keys.is_empty() {
        "(none detected)".to_string()
    } else {
        let mut keys: Vec<_> = global_state_keys.into_iter().collect();
        keys.sort();
        keys.join(", ")
    };

    let tables_text = if global_tables.is_empty() {
        "(none detected)".to_string()
    } else {
        let mut t: Vec<_> = global_tables.into_iter().collect();
        t.sort();
        t.join(", ")
    };

    // sorted_clusters is consumed into clusters_text; per-file maps are used
    // for cross-cluster dependency analysis.
    let _ = sorted_clusters;

    Ok(BoundaryData {
        clusters_text,
        state_text,
        tables_text,
        file_state_keys,
        file_table_refs,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::static_analyze_file_style;

    const OCIUSX_VB_SAMPLE: &str = r#"
Imports System
Imports System.Web
Imports System.Linq

Module sharedfunc
    ''' <summary>
    ''' Redirects and short-circuits the calling method.
    ''' </summary>
    Public Sub SafeRedirect(url As String)
        HttpContext.Current.Response.Redirect(url)
    End Sub

    Public Function GetValidFileName(name As String) As String
        If name Is Nothing Then
            Return String.Empty
        End If
        Return name.Trim()
    End Function

    Public Function GetUser(id As Integer, Optional db As iFaltDataContext = Nothing) As Object
        Using ctx = If(db, New iFaltDataContext())
            Dim row = ctx.Users.FirstOrDefault(Function(u) u.Id = id)
            If row Is Nothing Then
                Return Nothing
            End If
            Return row
        End Using
    End Function

    Public Function SaveUser(user As Object, Optional db As iFaltDataContext = Nothing) As Boolean
        Using ctx = If(db, New iFaltDataContext())
            Try
                ctx.SubmitChanges()
                Return True
            Catch ex As Exception
                Return False
            End Try
        End Using
    End Function

    Public Sub Redirect(url As String)
        SafeRedirect(url)
        Return
    End Sub

    Public Sub TranslateUnit(v As Double)
        If v Is Nothing Then
            Return
        End If
    End Sub
End Module
"#;

    #[test]
    fn vb_static_analyzer_detects_multiple_patterns() {
        let bullets = static_analyze_file_style(OCIUSX_VB_SAMPLE, "sharedfunc.vb");
        // Sanity: we get at least 5 distinct rule bullets on the OciusX shape,
        // covering naming + context injection + Using + Is Nothing + Module.
        assert!(
            bullets.len() >= 5,
            "expected ≥5 bullets, got {}: {bullets:#?}",
            bullets.len()
        );

        let joined = bullets.join(" ");
        assert!(
            joined.contains("PascalCase"),
            "method naming convention must be detected (PascalCase), got: {joined}"
        );
        assert!(
            joined.contains("Optional") && joined.contains("iFaltDataContext"),
            "optional-context-injection pattern must be called out"
        );
        assert!(
            joined.contains("Using"),
            "Using-block discipline must be called out"
        );
        assert!(
            joined.contains("Is Nothing"),
            "Is Nothing guard must be called out"
        );
        assert!(
            joined.contains("Module sharedfunc"),
            "declaration style must cite `Module sharedfunc`, got: {joined}"
        );
    }

    #[test]
    fn vb_static_analyzer_flags_safe_redirect_return_pair() {
        let bullets = static_analyze_file_style(OCIUSX_VB_SAMPLE, "sharedfunc.vb");
        assert!(
            bullets
                .iter()
                .any(|b| b.contains("SafeRedirect") && b.contains("Return")),
            "SafeRedirect+Return pair rule must fire, got: {bullets:#?}"
        );
    }

    #[test]
    fn vb_static_analyzer_prefers_try_catch_when_no_on_error() {
        let bullets = static_analyze_file_style(OCIUSX_VB_SAMPLE, "sharedfunc.vb");
        // The "Try/Catch only" bullet describes the current style AND
        // advises against introducing `On Error Resume Next`, so both
        // phrases appear in the same bullet. We only care that:
        //   - the `Try/Catch only` rule fired, and
        //   - the legacy `On Error Resume Next present` risk bullet did NOT fire.
        assert!(
            bullets.iter().any(|b| b.contains("`Try/Catch` only")),
            "Try/Catch-only rule must fire, got: {bullets:#?}"
        );
        assert!(
            !bullets
                .iter()
                .any(|b| b.contains("On Error Resume Next present")),
            "legacy On Error risk bullet must NOT fire for clean Try/Catch code"
        );
    }

    #[test]
    fn vb_static_analyzer_flags_legacy_on_error() {
        let legacy = r#"
Module Legacy
    Public Sub Do()
        On Error Resume Next
        DoStuff()
    End Sub
End Module
"#;
        let bullets = static_analyze_file_style(legacy, "legacy.vb");
        assert!(
            bullets
                .iter()
                .any(|b| b.contains("On Error Resume Next") && b.contains("risk")),
            "legacy On Error must be flagged as risk, got: {bullets:#?}"
        );
    }

    #[test]
    fn cs_static_analyzer_emits_generic_bullets() {
        let cs = r#"
using System;
public class Foo {
    public async Task Bar() {
        using (var db = new Context()) {
            await db.SaveChangesAsync();
        }
    }
}
"#;
        let bullets = static_analyze_file_style(cs, "Foo.cs");
        assert!(
            bullets.iter().any(|b| b.contains("using")),
            "C# using-pattern bullet expected, got: {bullets:#?}"
        );
        assert!(
            bullets.iter().any(|b| b.contains("async")),
            "C# async-pattern bullet expected"
        );
    }

    #[test]
    fn generic_static_analyzer_detects_indent_style() {
        let tabbed = "\tfn a() {\n\t\tlet x = 1;\n\t}\n";
        let bullets = static_analyze_file_style(tabbed, "unknown.xyz");
        assert!(
            bullets.iter().any(|b| b.contains("tabs")),
            "tab indentation must be detected in generic fallback"
        );
    }

    #[test]
    fn vb_static_analyzer_returns_empty_on_tiny_file() {
        let tiny = "Module X\nEnd Module\n";
        let bullets = static_analyze_file_style(tiny, "x.vb");
        // Module declaration fires but nothing else — that's fine and
        // prevents the caller from claiming rich style on trivial files.
        assert!(bullets.len() <= 2);
    }
}
