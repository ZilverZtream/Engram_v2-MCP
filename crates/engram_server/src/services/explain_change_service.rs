//! Explain-change service — the natural dual of `pre_commit_review`.
//!
//! `pre_commit_review` answers *"is this diff safe?"* — verdict plus
//! severity-ranked findings. `explain_change` answers *"what did this
//! diff actually do?"* — a commit message, a PR description, and a
//! changelog entry derived from the same diff plus everything the
//! knowledge graph knows about the affected code.
//!
//! Both tools take identical inputs (raw diff / `staged` / `unstaged`
//! / `head` / `.patch` path) so an agent can pipeline the propose →
//! verify → narrate triangle without reformatting the diff.
//!
//! Design principles mirror `pre_commit_review`:
//!
//! 1. **Deterministic by default.** Classification, scope detection,
//!    rule alignment, and all three output renderers run without any
//!    LLM call. An optional `use_llm` polish pass is a future
//!    extension point, never a requirement for correctness.
//! 2. **Evidence-backed.** Every claim in the output cites its source
//!    — file counts, blast-radius scores, rule IDs, commit SHAs.
//! 3. **Fast.** Same 5-second budget target as `pre_commit_review`.
//!    Heavy work (blast radius per file) is capped at 20 files.
//! 4. **Schema-stable.** The JSON output is versioned and field-typed
//!    so CI pipelines can depend on field names.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use engram_graph::{EdgeKind, GraphStore};
use serde::Serialize;

use crate::services::blast_radius_service::compute_blast_radius;
use crate::services::pre_commit_review_service::{
    ChangeType, DiffFile, is_test_path, parse_unified_diff, path_suffix_match, resolve_diff_source,
    resolve_partner_to_current, resx_dir_stem, resx_family_display,
};
use crate::state::AppState;

// ─── Public types ───────────────────────────────────────────────────────────

/// Conventional-commits change kind. Picked deterministically from the
/// file-kind distribution, graph node delta, and a small keyword
/// heuristic on diff content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Feat,
    Fix,
    Refactor,
    Perf,
    Test,
    Docs,
    Build,
    Style,
    Chore,
}

impl ChangeKind {
    pub fn conventional_prefix(self) -> &'static str {
        match self {
            Self::Feat => "feat",
            Self::Fix => "fix",
            Self::Refactor => "refactor",
            Self::Perf => "perf",
            Self::Test => "test",
            Self::Docs => "docs",
            Self::Build => "build",
            Self::Style => "style",
            Self::Chore => "chore",
        }
    }

    pub fn plain_label(self) -> &'static str {
        match self {
            Self::Feat => "Added",
            Self::Fix => "Fixed",
            Self::Refactor => "Changed",
            Self::Perf => "Changed",
            Self::Test => "Changed",
            Self::Docs => "Changed",
            Self::Build => "Changed",
            Self::Style => "Changed",
            Self::Chore => "Changed",
        }
    }

    /// Which Keep-a-Changelog section this kind falls into. Drives
    /// the changelog renderer.
    pub fn changelog_section(self) -> &'static str {
        match self {
            Self::Feat => "Added",
            Self::Fix => "Fixed",
            Self::Refactor | Self::Perf | Self::Style => "Changed",
            // Test / docs / build / chore don't usually land in a
            // user-facing changelog; we skip emitting an entry.
            Self::Test | Self::Docs | Self::Build | Self::Chore => "",
        }
    }
}

/// Per-file context gathered during narration. The business-logic
/// summary is opportunistic — populated when the service has cached a
/// per-method LLM summary, empty otherwise.
#[derive(Debug, Clone, Serialize)]
pub struct AffectedFile {
    pub path: String,
    pub change_type: String,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub blast_radius: Option<u8>,
    pub blast_risk_band: Option<String>,
    pub downstream: Option<usize>,
    pub is_test_file: bool,
    pub is_immune_flagged: bool,
    /// 1-line summary pulled from the graph node's name OR from a
    /// cached business-logic summary if available.
    pub function_hint: Option<String>,
}

/// A rule (immune / CodeRabbit / generic repo rule) that the change
/// aligns with — evidence the agent can cite in the PR description.
#[derive(Debug, Clone, Serialize)]
pub struct RuleAlignment {
    pub rule_id: String,
    pub rule_text: String,
    pub source: RuleAlignmentSource,
    /// File in the diff that this rule applies to.
    pub file_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAlignmentSource {
    Immune,
    CodeRabbit,
    RepoRule,
}

/// A temporal-coupling partner that is NOT in the diff. Surfaced as a
/// "coupled files note" in the PR description so reviewers can verify
/// the omission is intentional.
#[derive(Debug, Clone, Serialize)]
pub struct CouplingNote {
    pub source_file: String,
    pub partner_file: String,
    pub weight: u32,
}

/// Final structured narrative. Rendered to markdown or returned as
/// JSON; both paths use the same data.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeNarrative {
    pub schema_version: u32,
    pub kind: ChangeKind,
    pub scope: Option<String>,
    pub subject: String,
    pub body_bullets: Vec<String>,
    pub affected_files: Vec<AffectedFile>,
    pub rule_alignments: Vec<RuleAlignment>,
    pub coupling_notes: Vec<CouplingNote>,
    /// `green` / `yellow` / `red` risk badge — mirrors
    /// `pre_commit_review`'s verdict vocabulary so agents can compose
    /// the two outputs without translating terms.
    pub risk_badge: &'static str,
    pub risk_rationale: String,
    pub test_files_changed: usize,
    pub added_line_total: usize,
    pub removed_line_total: usize,
}

impl ChangeNarrative {
    pub const SCHEMA_VERSION: u32 = 1;
}

// ─── Config ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExplainChangeConfig {
    /// Output style for the commit subject. `conventional` uses
    /// `feat(scope): …`; `plain` uses natural prose.
    pub style: SubjectStyle,
    pub include_changelog: bool,
    /// Reserved for a future LLM polish pass. Not used by the current
    /// deterministic renderer — the field is here so adding the LLM
    /// path later doesn't churn the request shape.
    pub use_llm: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectStyle {
    Conventional,
    Plain,
}

impl Default for ExplainChangeConfig {
    fn default() -> Self {
        Self {
            style: SubjectStyle::Conventional,
            include_changelog: true,
            use_llm: false,
        }
    }
}

// ─── Entry point ───────────────────────────────────────────────────────────

pub async fn explain_change(
    state: &AppState,
    project_id: &str,
    project_dir: &Path,
    generation: u64,
    diff_source: &str,
    config: &ExplainChangeConfig,
) -> anyhow::Result<Option<(ChangeNarrative, ExplainRendered)>> {
    // Stage 1: resolve + parse diff.
    let diff_text = resolve_diff_source(project_dir, diff_source)?;
    if diff_text.trim().is_empty() {
        return Ok(None);
    }
    let diff_files = parse_unified_diff(&diff_text);
    if diff_files.is_empty() {
        return Ok(None);
    }

    // Stage 2: gather per-file facts, concurrent with stage 3 data
    // loads where practical. Blast radius is capped at 20 files.
    let changed_paths: HashSet<String> = diff_files.iter().map(|f| f.path.clone()).collect();

    let repo_rules = state
        .registry
        .list_repo_rules(project_id)
        .unwrap_or_default();
    let affected_files = gather_affected_files(
        &state.graph,
        project_id,
        generation,
        &diff_files,
        &repo_rules,
    );

    // Stage 3: classify change kind.
    let kind = classify_change_kind(&diff_files, &affected_files);

    // Stage 4: scope detection.
    let scope = detect_scope(&diff_files);

    // Stage 5: rule alignment — search antipattern namespace for
    // matches against the added content of each file.
    let rule_alignments =
        detect_rule_alignments(state, project_id, generation, &diff_files, &repo_rules).await;

    // Stage 6: coupling notes.
    let coupling_notes = detect_coupling_notes(
        &state.graph,
        project_id,
        project_dir,
        &diff_files,
        &changed_paths,
    );

    // Stage 7: risk badge.
    let (risk_badge, risk_rationale) = compute_risk(&affected_files, &coupling_notes);

    // Stage 8: render narrative.
    let added_total: usize = diff_files.iter().map(|f| f.added_lines.len()).sum();
    let removed_total: usize = diff_files.iter().map(|f| f.removed_lines.len()).sum();
    let test_files = diff_files.iter().filter(|f| is_test_path(&f.path)).count();
    let subject = render_subject(kind, scope.as_deref(), &diff_files, config.style);
    let body_bullets = render_body_bullets(kind, &affected_files, &rule_alignments);

    let narrative = ChangeNarrative {
        schema_version: ChangeNarrative::SCHEMA_VERSION,
        kind,
        scope,
        subject,
        body_bullets,
        affected_files,
        rule_alignments,
        coupling_notes,
        risk_badge,
        risk_rationale,
        test_files_changed: test_files,
        added_line_total: added_total,
        removed_line_total: removed_total,
    };

    // Stage 9: render text outputs.
    let rendered = render_all(&narrative, config);
    Ok(Some((narrative, rendered)))
}

// ─── Stage: classify change kind ───────────────────────────────────────────

/// Pick the single best Conventional Commits kind for this diff. Rules
/// apply top-to-bottom; first match wins. The order is deliberate so
/// "test-only" beats "refactor" and "docs-only" beats "chore".
pub fn classify_change_kind(diff_files: &[DiffFile], affected: &[AffectedFile]) -> ChangeKind {
    let total = diff_files.len();
    if total == 0 {
        return ChangeKind::Chore;
    }
    let test_files = diff_files.iter().filter(|f| is_test_path(&f.path)).count();
    let doc_files = diff_files.iter().filter(|f| is_doc_path(&f.path)).count();
    let build_files = diff_files.iter().filter(|f| is_build_path(&f.path)).count();

    // Rule 1: dominated by test files → `test`.
    if test_files * 5 >= total * 4 {
        return ChangeKind::Test;
    }
    // Rule 2: dominated by doc files → `docs`.
    if doc_files * 5 >= total * 4 {
        return ChangeKind::Docs;
    }
    // Rule 3: dominated by build / config files → `build`.
    if build_files * 5 >= total * 4 {
        return ChangeKind::Build;
    }
    // Rule 4: trivial whitespace-only change → `style`. We treat it as
    // style when every file has zero net line-count change AND the
    // added + removed lines are identical once leading whitespace is
    // stripped. Cheap heuristic; a real semantic diff engine could do
    // better.
    if diff_files.iter().all(is_whitespace_only_diff) {
        return ChangeKind::Style;
    }

    // Rule 5: keyword heuristic from the diff body. `perf` keywords
    // beat everything below because speed-up patterns (async addition,
    // caching, batching) often look like `refactor` structurally.
    if has_perf_keywords(diff_files) {
        return ChangeKind::Perf;
    }

    // Rule 6: graph-delta shape.
    //   - new public symbols added AND no existing ones modified → feat
    //   - only existing symbols modified → refactor
    //   - mix of both → feat (new surface dominates)
    let (added_sym, modified_sym) = graph_delta_counts(affected);
    if added_sym >= 1 {
        return ChangeKind::Feat;
    }
    if modified_sym >= 1 {
        // Distinguish fix vs refactor: if the diff has `fix:` / `bug`
        // / `error` keywords in the added content, it's a fix.
        if has_fix_keywords(diff_files) {
            return ChangeKind::Fix;
        }
        return ChangeKind::Refactor;
    }

    // Rule 7: no symbols touched, non-test / non-doc / non-build — chore.
    ChangeKind::Chore
}

fn is_doc_path(p: &str) -> bool {
    let lower = p.replace('\\', "/").to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    matches!(ext, "md" | "rst" | "txt" | "adoc") || lower.contains("/docs/")
}

fn is_build_path(p: &str) -> bool {
    let lower = p.replace('\\', "/").to_ascii_lowercase();
    let fname = lower.rsplit('/').next().unwrap_or(&lower);
    // Dependency manifests, workflow definitions, container configs.
    matches!(
        fname,
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "pyproject.toml"
            | "setup.py"
            | "requirements.txt"
            | "gemfile"
            | "gemfile.lock"
            | "go.mod"
            | "go.sum"
            | "dockerfile"
            | "makefile"
            | "cmakelists.txt"
            | ".dockerignore"
            | ".gitignore"
            | "rakefile"
    ) || lower.contains(".github/workflows/")
        || lower.contains(".gitlab/")
        || fname.ends_with(".csproj")
        || fname.ends_with(".vbproj")
        || fname.ends_with(".sln")
}

fn is_whitespace_only_diff(f: &DiffFile) -> bool {
    if f.is_binary {
        return false;
    }
    let add_stripped: Vec<String> = f
        .added_lines
        .iter()
        .map(|(_, l)| l.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let rem_stripped: Vec<String> = f
        .removed_lines
        .iter()
        .map(|(_, l)| l.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    add_stripped == rem_stripped
}

fn has_perf_keywords(diff_files: &[DiffFile]) -> bool {
    const HINTS: &[&str] = &[
        "perf:",
        "performance",
        "optimi",
        "speed up",
        "faster",
        "cache",
        "memoi",
        "parallel",
        "async ",
        "batch",
    ];
    diff_files.iter().any(|f| {
        let lower = f.added_content.to_ascii_lowercase();
        HINTS.iter().any(|h| lower.contains(h))
    })
}

fn has_fix_keywords(diff_files: &[DiffFile]) -> bool {
    const HINTS: &[&str] = &[
        "fix:",
        "bugfix",
        "regression",
        "null check",
        "off-by-one",
        "// fixes ",
        "// fix ",
        "/* fix ",
        "// bug",
    ];
    diff_files.iter().any(|f| {
        let lower = f.added_content.to_ascii_lowercase();
        HINTS.iter().any(|h| lower.contains(h))
    })
}

fn graph_delta_counts(affected: &[AffectedFile]) -> (usize, usize) {
    // We don't have a true pre/post graph diff here — approximate by
    // bucketing per-file change type. A file marked `Added` contributes
    // 1 "added symbol" to this count regardless of how many symbols it
    // contains. Modified files contribute to modified-symbol count.
    let added = affected.iter().filter(|f| f.change_type == "added").count();
    let modified = affected
        .iter()
        .filter(|f| f.change_type == "modified")
        .count();
    (added, modified)
}

// ─── Stage: scope detection ────────────────────────────────────────────────

/// Infer the conventional-commits scope (`feat(scope): …`) from the
/// longest common path prefix of the changed files. Returns `None`
/// when files span more than 3 top-level directories — a wide-cutting
/// change has no meaningful scope.
pub fn detect_scope(diff_files: &[DiffFile]) -> Option<String> {
    if diff_files.is_empty() {
        return None;
    }

    // Group by top-level dir segment to detect "too spread out".
    let mut top_dirs: HashSet<String> = HashSet::new();
    for f in diff_files {
        let lower = f.path.replace('\\', "/");
        let top = lower.split('/').next().unwrap_or("").to_string();
        if !top.is_empty() && !top.contains('.') {
            top_dirs.insert(top);
        }
    }
    if top_dirs.len() > 3 {
        return None;
    }

    // Look at every immediate parent directory; if one is shared by
    // ≥60% of files, use its last segment. Skips generic dir names
    // (`src`, `lib`) that carry no domain signal.
    let mut parent_counts: HashMap<String, usize> = HashMap::new();
    for f in diff_files {
        let lower = f.path.replace('\\', "/");
        if let Some(idx) = lower.rfind('/') {
            let parent = &lower[..idx];
            let scope = parent
                .rsplit('/')
                .find(|seg| !is_generic_segment(seg))
                .unwrap_or("");
            if !scope.is_empty() {
                *parent_counts.entry(scope.to_ascii_lowercase()).or_insert(0) += 1;
            }
        }
    }
    let total = diff_files.len();
    parent_counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .filter(|(_, c)| *c * 10 >= total * 6)
        .map(|(k, _)| k)
}

fn is_generic_segment(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "src"
            | "lib"
            | "source"
            | "app"
            | "site"
            | "code"
            | "common"
            | "shared"
            | "main"
            | "www"
            | "public"
            | "test"
            | "tests"
            | "spec"
            | "specs"
            | "doc"
            | "docs"
    )
}

// ─── Stage: per-file facts ─────────────────────────────────────────────────

/// Build AffectedFile records for every file in the diff. Blast
/// radius is capped at 20 files (paths sorted shallowest-first) so
/// this stage stays within the 5-second service budget.
pub fn gather_affected_files(
    graph: &GraphStore,
    project_id: &str,
    generation: u64,
    diff_files: &[DiffFile],
    repo_rules: &[engram_core::registry::RepoRule],
) -> Vec<AffectedFile> {
    const BLAST_CAP: usize = 20;
    let mut sorted_idx: Vec<usize> = (0..diff_files.len()).collect();
    sorted_idx.sort_by_key(|i| diff_files[*i].path.matches('/').count());
    let blast_set: HashSet<usize> = sorted_idx.iter().take(BLAST_CAP).copied().collect();

    diff_files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let change_type = match &f.change_type {
                ChangeType::Added => "added",
                ChangeType::Modified => "modified",
                ChangeType::Deleted => "deleted",
                ChangeType::Renamed(_) => "renamed",
            };
            let is_immune_flagged = repo_rules
                .iter()
                .filter(|r| r.rule_id.starts_with("immune_"))
                .any(|r| path_pattern_matches(&r.file_pattern, &f.path));

            let (blast, band, downstream) = if blast_set.contains(&i)
                && !f.is_binary
                && !matches!(f.change_type, ChangeType::Deleted)
            {
                let target = format!("file:{}", f.path.replace('\\', "/"));
                match compute_blast_radius(graph, project_id, &target, generation, false) {
                    Ok(report) if report.total_incoming + report.total_outgoing > 0 => (
                        Some(report.migration_risk),
                        Some(report.risk_band.as_str().to_string()),
                        Some(report.total_downstream),
                    ),
                    _ => (None, None, None),
                }
            } else {
                (None, None, None)
            };

            // Function hint: first function-node whose file_path
            // matches this diff file. If the graph hasn't been
            // indexed yet for a brand-new file, this is None — that's
            // the correct representation.
            let function_hint = graph
                .query_nodes(
                    project_id,
                    Some("function"),
                    None,
                    Some(&f.path.replace('\\', "/")),
                    5,
                )
                .ok()
                .and_then(|nodes| nodes.into_iter().next().map(|n| n.name));

            AffectedFile {
                path: f.path.clone(),
                change_type: change_type.into(),
                added_lines: f.added_lines.len(),
                removed_lines: f.removed_lines.len(),
                blast_radius: blast,
                blast_risk_band: band,
                downstream,
                is_test_file: is_test_path(&f.path),
                is_immune_flagged,
                function_hint,
            }
        })
        .collect()
}

/// Minimal immune-rule path matcher — same shape as the one
/// `pre_commit_review`'s immune gate uses. Duplicated here rather
/// than re-exported to avoid coupling this service to internals of
/// another service module.
fn path_pattern_matches(file_pattern: &str, target_path: &str) -> bool {
    if file_pattern.is_empty() {
        return false;
    }
    let pat = file_pattern.replace('\\', "/").to_ascii_lowercase();
    let path = target_path.replace('\\', "/").to_ascii_lowercase();
    if pat == path {
        return true;
    }
    if pat.contains('*') || pat.contains('?') {
        let mut re = String::with_capacity(pat.len() + 8);
        re.push('^');
        for c in pat.chars() {
            match c {
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\' => {
                    re.push('\\');
                    re.push(c);
                }
                _ => re.push(c),
            }
        }
        re.push('$');
        regex::Regex::new(&re)
            .map(|r| r.is_match(&path))
            .unwrap_or(false)
    } else {
        path.contains(&pat)
    }
}

// ─── Stage: rule alignment ─────────────────────────────────────────────────

/// Surface rules the diff is plausibly addressing. Two paths:
///
/// - Repo rules (`immune_*` / `cr_*`) whose `file_pattern` matches any
///   changed file. Fast — just pattern-match the registry.
/// - Hybrid search against the `antipattern` namespace using each
///   file's added content as query. Matches with `score > 0.5` tag
///   the rule as "likely addressed".
pub async fn detect_rule_alignments(
    state: &AppState,
    project_id: &str,
    generation: u64,
    diff_files: &[DiffFile],
    repo_rules: &[engram_core::registry::RepoRule],
) -> Vec<RuleAlignment> {
    use engram_index::hybrid::HybridQuery;
    use tokio_util::sync::CancellationToken;

    let mut out: Vec<RuleAlignment> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    // Path-matched repo rules — cheap, always runs.
    for f in diff_files {
        for rule in repo_rules {
            if !path_pattern_matches(&rule.file_pattern, &f.path) {
                continue;
            }
            let source = if rule.rule_id.starts_with("immune_") {
                RuleAlignmentSource::Immune
            } else if rule.rule_id.starts_with("cr_") {
                RuleAlignmentSource::CodeRabbit
            } else {
                RuleAlignmentSource::RepoRule
            };
            let key = (rule.rule_id.clone(), f.path.clone());
            if !seen.insert(key) {
                continue;
            }
            out.push(RuleAlignment {
                rule_id: rule.rule_id.clone(),
                rule_text: rule.rule_text.clone(),
                source,
                file_path: f.path.clone(),
            });
        }
    }

    // Hybrid-search-backed alignment — ENSURE the runtime (a fresh
    // daemon has no cached ProjectState; a cached-only lookup silently
    // skipped this whole branch, same defect as the review's async
    // gates). Failing gracefully keeps the service useful without the
    // index (minimal test setups).
    if let Ok(ps) =
        crate::services::project_service::ensure_project_runtime(state, project_id).await
    {
        for f in diff_files {
            if f.is_binary
                || matches!(f.change_type, ChangeType::Deleted)
                || f.added_content.len() < 50
            {
                continue;
            }
            // Resource/markup text is not code — see AntiPatternGate.
            let lower_path = f.path.to_ascii_lowercase();
            if lower_path.ends_with(".resx")
                || lower_path.ends_with(".xml")
                || lower_path.ends_with(".config")
                || lower_path.ends_with(".css")
            {
                continue;
            }
            let query = crate::utils::text::code_to_query(&f.added_content);
            let q = HybridQuery {
                project_id: project_id.to_string(),
                namespace: "antipattern".into(),
                generation,
                text: query.clone(),
                top_k: 3,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                include_path_suffixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            };
            let cancel = CancellationToken::new();
            let hits = ps
                .search
                .search(&q, None, &cancel)
                .await
                .unwrap_or_default();
            for h in hits {
                // RRF scores (~0.03 at rank 1) make absolute score
                // thresholds dead code — judge by term overlap against
                // the hit's stored content, same as the review gates.
                if h.path.as_str().to_ascii_lowercase().contains(".min.") {
                    continue;
                }
                let content = ps
                    .search
                    .get_doc_by_pk(&h.pk)
                    .ok()
                    .flatten()
                    .map(|(_, _, c, _, _)| c)
                    .unwrap_or_default();
                if content.is_empty() {
                    continue;
                }
                let (matched_n, total_n, _) =
                    crate::services::pre_commit_review_service::gates::query_overlap(
                        &content, &query,
                    );
                if matched_n < 4 || (matched_n as f32 / total_n.max(1) as f32) < 0.3 {
                    continue;
                }
                // CodeRabbit docs land in the antipattern namespace
                // with `author = "coderabbit"`. Without the author
                // field here we infer from path shape — CR clusters
                // use `**/*.ext`-style synthetic paths, revert
                // patterns use real file paths. Not perfect but good
                // enough for the alignment annotation.
                let path_str = h.path.as_str();
                let source = if path_str.contains("**/") {
                    RuleAlignmentSource::CodeRabbit
                } else {
                    RuleAlignmentSource::Immune
                };
                let rule_id = h.doc_id.chars().take(16).collect::<String>();
                let rule_text = h.snippet.unwrap_or_else(|| path_str.to_string());
                let key = (rule_id.clone(), f.path.clone());
                if !seen.insert(key) {
                    continue;
                }
                out.push(RuleAlignment {
                    rule_id,
                    rule_text,
                    source,
                    file_path: f.path.clone(),
                });
            }
        }
    }
    out
}

// ─── Stage: coupling notes ─────────────────────────────────────────────────

pub fn detect_coupling_notes(
    graph: &GraphStore,
    project_id: &str,
    project_dir: &Path,
    diff_files: &[DiffFile],
    changed_paths: &HashSet<String>,
) -> Vec<CouplingNote> {
    // Current-tree file paths — used to re-anchor HISTORICAL partner
    // spellings (pre-restructure paths in co-change history, e.g.
    // `App_Code/x.vb` vs `Site/App_Code/x.vb`) to the spelling that
    // actually exists today. Same rationale as `TemporalGate` in
    // `pre_commit_review_service`.
    let current_files: Vec<String> = graph
        .query_nodes(project_id, Some("file"), None, None, 50_000)
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.file_path.as_str().to_string())
        .collect();
    let mut out: Vec<CouplingNote> = Vec::new();
    for f in diff_files {
        if matches!(f.change_type, ChangeType::Added | ChangeType::Deleted) {
            continue;
        }
        let node_id = format!("file:{}", f.path.replace('\\', "/"));
        let neighbors = graph
            .neighbors(project_id, EdgeKind::TemporalCoupling, &node_id, 10)
            .unwrap_or_default();
        // Multiple historical spellings can resolve to the same current
        // file — note each partner once per source file (neighbors are
        // weight-sorted, so the strongest wins).
        let mut noted: HashSet<String> = HashSet::new();
        for (partner_id, weight) in neighbors {
            // 50 is the "strong coupling" floor we use elsewhere in
            // the codebase — anything under that is background
            // noise and shouldn't clutter the PR description.
            if weight < 50 {
                continue;
            }
            let raw_partner = partner_id.strip_prefix("file:").unwrap_or(&partner_id);
            // Suffix-aware membership: a historical spelling counts as
            // "in the diff" when it component-suffix-matches any changed
            // path — exact string equality misses restructured repos.
            if changed_paths
                .iter()
                .any(|p| path_suffix_match(p, raw_partner))
            {
                continue;
            }
            // Never emit a partner path that doesn't exist in the current
            // tree; when the historical spelling resolves to an existing
            // file, emit the CURRENT spelling instead.
            let Some(partner_path) =
                resolve_partner_to_current(raw_partner, &current_files, project_dir)
            else {
                continue;
            };
            if changed_paths
                .iter()
                .any(|p| path_suffix_match(p, &partner_path))
            {
                continue;
            }
            if !noted.insert(partner_path.clone()) {
                continue;
            }
            out.push(CouplingNote {
                source_file: f.path.clone(),
                partner_file: partner_path,
                weight,
            });
        }
    }
    // Collapse localized .resx language variants of one family into a
    // single note BEFORE the cap — otherwise a 7-locale family eats the
    // whole note budget and hides every other coupled partner. Mirrors
    // `collapse_resx_family_findings` in pre_commit_review: a lone resx
    // partner keeps its own spelling; only 2+ variants become a family.
    let mut collapsed: Vec<CouplingNote> = Vec::new();
    let mut family_slot: HashMap<(String, String, String), (usize, usize)> = HashMap::new();
    for note in out {
        let Some((dir, stem)) = resx_dir_stem(&note.partner_file) else {
            collapsed.push(note);
            continue;
        };
        let key = (note.source_file.clone(), dir.clone(), stem.clone());
        match family_slot.get_mut(&key) {
            Some((i, members)) => {
                *members += 1;
                // 2+ variants: display the family, keep the strongest weight.
                collapsed[*i].partner_file = resx_family_display(&dir, &stem);
                if note.weight > collapsed[*i].weight {
                    collapsed[*i].weight = note.weight;
                }
            }
            None => {
                family_slot.insert(key, (collapsed.len(), 1));
                collapsed.push(note);
            }
        }
    }
    let mut out = collapsed;
    // Cap at 5 — avoid drowning the PR description.
    out.sort_by(|a, b| b.weight.cmp(&a.weight));
    out.truncate(5);
    out
}

// ─── Stage: risk badge ─────────────────────────────────────────────────────

fn compute_risk(affected: &[AffectedFile], couplings: &[CouplingNote]) -> (&'static str, String) {
    let max_blast = affected
        .iter()
        .filter_map(|f| f.blast_radius)
        .max()
        .unwrap_or(0);
    let immune_hits = affected.iter().filter(|f| f.is_immune_flagged).count();
    let file_count = affected.len();

    // Red: any immune-flagged file, OR any blast radius ≥ 8.
    if immune_hits > 0 || max_blast >= 8 {
        let mut reasons = Vec::new();
        if immune_hits > 0 {
            reasons.push(format!("{immune_hits} immune-flagged file(s)"));
        }
        if max_blast >= 8 {
            reasons.push(format!("max blast radius {max_blast}/10"));
        }
        return ("red", reasons.join("; "));
    }
    // Yellow: max blast radius ≥ 5, OR ≥ 10 files changed, OR
    // non-trivial unmatched coupling.
    if max_blast >= 5 || file_count >= 10 || !couplings.is_empty() {
        let mut reasons = Vec::new();
        if max_blast >= 5 {
            reasons.push(format!("max blast radius {max_blast}/10"));
        }
        if file_count >= 10 {
            reasons.push(format!("{file_count} files changed"));
        }
        if !couplings.is_empty() {
            reasons.push(format!("{} coupled file(s) not in diff", couplings.len()));
        }
        return ("yellow", reasons.join("; "));
    }
    (
        "green",
        format!("{file_count} file(s) changed, max blast radius {max_blast}/10"),
    )
}

// ─── Stage: subject + body renderers ───────────────────────────────────────

fn render_subject(
    kind: ChangeKind,
    scope: Option<&str>,
    diff_files: &[DiffFile],
    style: SubjectStyle,
) -> String {
    // Heuristic one-line summary. Prefer the dominant
    // verb-extracted-from-changes. Fall back to a neutral phrase.
    let verb = summarise_verb_phrase(kind, diff_files);
    match style {
        SubjectStyle::Conventional => match scope {
            Some(s) => format!("{prefix}({s}): {verb}", prefix = kind.conventional_prefix()),
            None => format!("{prefix}: {verb}", prefix = kind.conventional_prefix()),
        },
        SubjectStyle::Plain => {
            let label = kind.plain_label();
            match scope {
                Some(s) => format!("{label} in {s}: {verb}"),
                None => format!("{label}: {verb}"),
            }
        }
    }
}

/// Build a short verb phrase for the subject line. Completely
/// deterministic — no LLM. Uses the file-count shape + change kind to
/// produce a sentence like "add OrderService audit hook" or "refactor
/// 3 handler files".
fn summarise_verb_phrase(kind: ChangeKind, diff_files: &[DiffFile]) -> String {
    let added = diff_files
        .iter()
        .filter(|f| matches!(f.change_type, ChangeType::Added))
        .count();
    let modified = diff_files
        .iter()
        .filter(|f| matches!(f.change_type, ChangeType::Modified))
        .count();
    let deleted = diff_files
        .iter()
        .filter(|f| matches!(f.change_type, ChangeType::Deleted))
        .count();

    // Pick the most-modified file's stem as a focal noun when we have
    // one. Helps produce "add fiberjobb_audit helper" instead of
    // "add 1 file".
    let focus = diff_files
        .iter()
        .find(|f| matches!(f.change_type, ChangeType::Added))
        .or_else(|| {
            diff_files
                .iter()
                .find(|f| matches!(f.change_type, ChangeType::Modified))
        })
        .map(|f| file_stem(&f.path));

    let file_word = if diff_files.len() == 1 {
        "file"
    } else {
        "files"
    };
    let n = diff_files.len();
    match kind {
        ChangeKind::Feat => match focus.as_deref() {
            Some(name) if added >= 1 => format!("add {name}"),
            Some(name) => format!("introduce {name}"),
            None => format!("add {n} {file_word}"),
        },
        ChangeKind::Fix => match focus.as_deref() {
            Some(name) => format!("fix {name}"),
            None => format!("fix {n} {file_word}"),
        },
        ChangeKind::Refactor => match focus.as_deref() {
            Some(name) => format!("refactor {name}"),
            None => format!("refactor {n} {file_word}"),
        },
        ChangeKind::Perf => format!("speed up {n} {file_word}"),
        ChangeKind::Test => match focus.as_deref() {
            Some(name) => format!("update tests around {name}"),
            None => format!("update {n} test {file_word}"),
        },
        ChangeKind::Docs => format!("update {n} doc {file_word}"),
        ChangeKind::Build => format!("update {n} build / config {file_word}"),
        ChangeKind::Style => format!("reformat {n} {file_word}"),
        ChangeKind::Chore => {
            if deleted >= added + modified {
                format!("remove {deleted} {file_word}")
            } else {
                format!("update {n} {file_word}")
            }
        }
    }
}

fn file_stem(path: &str) -> String {
    let fname = path.replace('\\', "/");
    let base = fname.rsplit('/').next().unwrap_or(&fname);
    match base.rfind('.') {
        Some(i) if i > 0 => base[..i].to_string(),
        _ => base.to_string(),
    }
}

/// Build up to 5 bullet points for the commit body / PR summary. Each
/// bullet is one fact the diff produces — no prose padding.
fn render_body_bullets(
    kind: ChangeKind,
    affected: &[AffectedFile],
    rules: &[RuleAlignment],
) -> Vec<String> {
    let mut bullets: Vec<String> = Vec::new();

    // Bullet 1: file-shape summary.
    let added_n = affected.iter().filter(|f| f.change_type == "added").count();
    let modified_n = affected
        .iter()
        .filter(|f| f.change_type == "modified")
        .count();
    let deleted_n = affected
        .iter()
        .filter(|f| f.change_type == "deleted")
        .count();
    let renamed_n = affected
        .iter()
        .filter(|f| f.change_type == "renamed")
        .count();
    let mut shape_parts: Vec<String> = Vec::new();
    if added_n > 0 {
        shape_parts.push(format!("{added_n} added"));
    }
    if modified_n > 0 {
        shape_parts.push(format!("{modified_n} modified"));
    }
    if deleted_n > 0 {
        shape_parts.push(format!("{deleted_n} deleted"));
    }
    if renamed_n > 0 {
        shape_parts.push(format!("{renamed_n} renamed"));
    }
    if !shape_parts.is_empty() {
        bullets.push(format!("Files: {}", shape_parts.join(", ")));
    }

    // Bullet 2-ish: the most informative function hints.
    let hint_files: Vec<&AffectedFile> = affected
        .iter()
        .filter(|f| f.function_hint.is_some() && f.change_type != "deleted")
        .take(3)
        .collect();
    for f in hint_files {
        if let Some(hint) = &f.function_hint {
            bullets.push(format!("`{}` — focus: `{}`", f.path, hint));
        }
    }

    // Bullet: rule alignments.
    if !rules.is_empty() {
        let cr_rules = rules
            .iter()
            .filter(|r| r.source == RuleAlignmentSource::CodeRabbit)
            .count();
        let immune = rules
            .iter()
            .filter(|r| r.source == RuleAlignmentSource::Immune)
            .count();
        if cr_rules > 0 || immune > 0 {
            let mut parts = Vec::new();
            if cr_rules > 0 {
                parts.push(format!("{cr_rules} CodeRabbit rule(s)"));
            }
            if immune > 0 {
                parts.push(format!("{immune} immune-flag(s)"));
            }
            bullets.push(format!("Aligns with {}", parts.join(", ")));
        }
    }

    // Bullet: kind-specific closer.
    let closer = match kind {
        ChangeKind::Feat => "New surface added; callers should update",
        ChangeKind::Fix => "Bug fix; preserves previous public behaviour",
        ChangeKind::Refactor => "No behavioural change intended",
        ChangeKind::Perf => "Performance change; verify benchmarks",
        ChangeKind::Test => "Test-only change",
        ChangeKind::Docs => "Documentation-only change",
        ChangeKind::Build => "Build / config change",
        ChangeKind::Style => "Whitespace / formatting only",
        ChangeKind::Chore => "Housekeeping change",
    };
    bullets.push(closer.to_string());

    bullets
}

// ─── Final render ──────────────────────────────────────────────────────────

pub struct ExplainRendered {
    pub commit_message: String,
    pub pr_description: String,
    pub changelog_entry: Option<String>,
}

fn render_all(n: &ChangeNarrative, config: &ExplainChangeConfig) -> ExplainRendered {
    ExplainRendered {
        commit_message: render_commit_message(n),
        pr_description: render_pr_description(n),
        changelog_entry: if config.include_changelog {
            render_changelog_entry(n)
        } else {
            None
        },
    }
}

pub fn render_commit_message(n: &ChangeNarrative) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(n.subject.trim());
    out.push('\n');
    if !n.body_bullets.is_empty() {
        out.push('\n');
        for b in &n.body_bullets {
            out.push_str(b);
            out.push('\n');
        }
    }
    // Footer — cite the rules this change aligns with. Machine
    // readers (changelog generators, deploy bots) can parse the
    // `Addresses:` line.
    if !n.rule_alignments.is_empty() {
        out.push('\n');
        out.push_str("Addresses:\n");
        let mut cited: HashSet<&str> = HashSet::new();
        for r in &n.rule_alignments {
            if !cited.insert(r.rule_id.as_str()) {
                continue;
            }
            out.push_str(&format!(
                "  {} ({})\n",
                r.rule_id,
                rule_source_label(r.source)
            ));
        }
    }
    out
}

pub fn render_pr_description(n: &ChangeNarrative) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(2048);
    let _ = writeln!(out, "# {}", n.subject.trim());
    out.push('\n');

    out.push_str("## Summary\n");
    for b in &n.body_bullets {
        let _ = writeln!(out, "- {b}");
    }
    out.push('\n');

    out.push_str("## Affected files\n");
    // Group by change_type for readability.
    let mut by_kind: BTreeMap<String, Vec<&AffectedFile>> = BTreeMap::new();
    for f in &n.affected_files {
        by_kind.entry(f.change_type.clone()).or_default().push(f);
    }
    for (kind, files) in &by_kind {
        let _ = writeln!(out, "### {}", kind);
        for f in files {
            let blast = match (f.blast_radius, f.blast_risk_band.as_deref()) {
                (Some(r), Some(b)) => format!(" — blast {r}/10 {b}"),
                _ => String::new(),
            };
            let down = match f.downstream {
                Some(d) if d > 0 => format!(", {d} downstream"),
                _ => String::new(),
            };
            let flags = {
                let mut v = Vec::new();
                if f.is_immune_flagged {
                    v.push("🛡 immune");
                }
                if f.is_test_file {
                    v.push("🧪 test");
                }
                if v.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", v.join(" · "))
                }
            };
            let hint = f
                .function_hint
                .as_ref()
                .map(|h| format!(" — `{h}`"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "- `{}` (+{}/-{}){blast}{down}{flags}{hint}",
                f.path, f.added_lines, f.removed_lines
            );
        }
    }
    out.push('\n');

    if !n.rule_alignments.is_empty() {
        out.push_str("## Addresses\n");
        let mut cited: HashSet<&str> = HashSet::new();
        for r in &n.rule_alignments {
            if !cited.insert(r.rule_id.as_str()) {
                continue;
            }
            let emoji = match r.source {
                RuleAlignmentSource::Immune => "🛡",
                RuleAlignmentSource::CodeRabbit => "🐰",
                RuleAlignmentSource::RepoRule => "📌",
            };
            let _ = writeln!(
                out,
                "- {emoji} **{}** ({}): {}",
                rule_source_label(r.source),
                r.rule_id,
                r.rule_text.trim()
            );
        }
        out.push('\n');
    }

    if !n.coupling_notes.is_empty() {
        out.push_str("## Temporal coupling note\n");
        out.push_str("The following files historically change together with files in this diff but are NOT included — verify this is intentional.\n\n");
        for c in &n.coupling_notes {
            let _ = writeln!(
                out,
                "- `{}` + `{}` (coupling weight {})",
                c.source_file, c.partner_file, c.weight
            );
        }
        out.push('\n');
    }

    let risk_emoji = match n.risk_badge {
        "red" => "🔴",
        "yellow" => "🟡",
        _ => "🟢",
    };
    let _ = writeln!(
        out,
        "## Risk\n{risk_emoji} **{}** — {}",
        n.risk_badge.to_uppercase(),
        n.risk_rationale
    );
    out.push('\n');

    let _ = writeln!(
        out,
        "_Generated by Engram `explain_change` (deterministic). Schema v{}._",
        n.schema_version
    );
    out
}

fn render_changelog_entry(n: &ChangeNarrative) -> Option<String> {
    let section = n.kind.changelog_section();
    if section.is_empty() {
        return None;
    }
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "### {section}");
    let scope = n
        .scope
        .as_ref()
        .map(|s| format!("**{s}**: "))
        .unwrap_or_default();
    // Take the first body bullet as the changelog description —
    // typically the most informative one.
    let desc = n
        .body_bullets
        .first()
        .cloned()
        .unwrap_or_else(|| n.subject.clone());
    let _ = writeln!(out, "- {scope}{desc}");
    Some(out)
}

fn rule_source_label(s: RuleAlignmentSource) -> &'static str {
    match s {
        RuleAlignmentSource::Immune => "immune",
        RuleAlignmentSource::CodeRabbit => "coderabbit",
        RuleAlignmentSource::RepoRule => "repo_rule",
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_diff(kind: ChangeType, path: &str, added: &[&str], removed: &[&str]) -> DiffFile {
        let added_lines: Vec<(usize, String)> = added
            .iter()
            .enumerate()
            .map(|(i, l)| (i + 1, l.to_string()))
            .collect();
        let removed_lines: Vec<(usize, String)> = removed
            .iter()
            .enumerate()
            .map(|(i, l)| (i + 1, l.to_string()))
            .collect();
        let added_content = added.join("\n");
        let removed_content = removed.join("\n");
        DiffFile {
            path: path.to_string(),
            change_type: kind,
            added_lines,
            removed_lines,
            added_content,
            removed_content,
            hunks: Vec::new(),
            is_binary: false,
        }
    }

    fn mk_affected(path: &str, kind: &str) -> AffectedFile {
        AffectedFile {
            path: path.to_string(),
            change_type: kind.to_string(),
            added_lines: 10,
            removed_lines: 0,
            blast_radius: None,
            blast_risk_band: None,
            downstream: None,
            is_test_file: super::is_test_path(path),
            is_immune_flagged: false,
            function_hint: None,
        }
    }

    #[test]
    fn classify_test_dominated_diff_returns_test() {
        let diff = vec![
            mk_diff(ChangeType::Modified, "src/foo_test.rs", &["new test"], &[]),
            mk_diff(ChangeType::Modified, "src/bar_test.rs", &["new test"], &[]),
            mk_diff(ChangeType::Modified, "src/baz_test.rs", &["new test"], &[]),
        ];
        let affected = vec![
            mk_affected("src/foo_test.rs", "modified"),
            mk_affected("src/bar_test.rs", "modified"),
            mk_affected("src/baz_test.rs", "modified"),
        ];
        assert_eq!(classify_change_kind(&diff, &affected), ChangeKind::Test);
    }

    #[test]
    fn classify_docs_only_returns_docs() {
        let diff = vec![
            mk_diff(ChangeType::Modified, "README.md", &["new line"], &[]),
            mk_diff(ChangeType::Modified, "docs/usage.md", &["doc"], &[]),
        ];
        let affected = vec![
            mk_affected("README.md", "modified"),
            mk_affected("docs/usage.md", "modified"),
        ];
        assert_eq!(classify_change_kind(&diff, &affected), ChangeKind::Docs);
    }

    #[test]
    fn classify_build_config_returns_build() {
        let diff = vec![
            mk_diff(ChangeType::Modified, "Cargo.toml", &["new dep"], &[]),
            mk_diff(
                ChangeType::Modified,
                ".github/workflows/ci.yml",
                &["step"],
                &[],
            ),
        ];
        let affected = vec![
            mk_affected("Cargo.toml", "modified"),
            mk_affected(".github/workflows/ci.yml", "modified"),
        ];
        assert_eq!(classify_change_kind(&diff, &affected), ChangeKind::Build);
    }

    #[test]
    fn classify_added_file_returns_feat() {
        let diff = vec![mk_diff(
            ChangeType::Added,
            "src/new_feature.rs",
            &["pub fn hello() {}"],
            &[],
        )];
        let affected = vec![mk_affected("src/new_feature.rs", "added")];
        assert_eq!(classify_change_kind(&diff, &affected), ChangeKind::Feat);
    }

    #[test]
    fn classify_modified_only_with_fix_keyword_returns_fix() {
        let diff = vec![mk_diff(
            ChangeType::Modified,
            "src/service.rs",
            &["// fix: null check", "if x.is_none() { return; }"],
            &["if x.unwrap() == 0 {}"],
        )];
        let affected = vec![mk_affected("src/service.rs", "modified")];
        assert_eq!(classify_change_kind(&diff, &affected), ChangeKind::Fix);
    }

    #[test]
    fn classify_modified_only_without_fix_keyword_returns_refactor() {
        let diff = vec![mk_diff(
            ChangeType::Modified,
            "src/service.rs",
            &["fn rename_me() {}"],
            &["fn old_name() {}"],
        )];
        let affected = vec![mk_affected("src/service.rs", "modified")];
        assert_eq!(classify_change_kind(&diff, &affected), ChangeKind::Refactor);
    }

    #[test]
    fn classify_perf_keyword_wins_over_refactor() {
        let diff = vec![mk_diff(
            ChangeType::Modified,
            "src/service.rs",
            &["// perf: batch DB calls to speed up hot path"],
            &["// old"],
        )];
        let affected = vec![mk_affected("src/service.rs", "modified")];
        assert_eq!(classify_change_kind(&diff, &affected), ChangeKind::Perf);
    }

    #[test]
    fn scope_detected_from_common_parent() {
        let diff = vec![
            mk_diff(ChangeType::Modified, "site/orders/service.vb", &["x"], &[]),
            mk_diff(ChangeType::Modified, "site/orders/model.vb", &["x"], &[]),
            mk_diff(ChangeType::Modified, "site/orders/view.vb", &["x"], &[]),
        ];
        assert_eq!(detect_scope(&diff), Some("orders".into()));
    }

    #[test]
    fn scope_skips_generic_segments() {
        // `src/` is a generic segment — the scope should drop to the
        // domain-specific child if one dominates.
        let diff = vec![
            mk_diff(ChangeType::Modified, "src/orders/mod.rs", &["x"], &[]),
            mk_diff(ChangeType::Modified, "src/orders/impl.rs", &["x"], &[]),
        ];
        assert_eq!(detect_scope(&diff), Some("orders".into()));
    }

    #[test]
    fn scope_none_when_too_spread() {
        // 4 distinct top-level directories → no coherent scope.
        let diff = vec![
            mk_diff(ChangeType::Modified, "frontend/app.ts", &["x"], &[]),
            mk_diff(ChangeType::Modified, "backend/server.rs", &["x"], &[]),
            mk_diff(ChangeType::Modified, "shared/proto.proto", &["x"], &[]),
            mk_diff(ChangeType::Modified, "mobile/ios/App.swift", &["x"], &[]),
        ];
        assert_eq!(detect_scope(&diff), None);
    }

    #[test]
    fn subject_uses_conventional_prefix_by_default() {
        let diff = vec![mk_diff(
            ChangeType::Added,
            "site/orders/service.vb",
            &["x"],
            &[],
        )];
        let subject = render_subject(
            ChangeKind::Feat,
            Some("orders"),
            &diff,
            SubjectStyle::Conventional,
        );
        assert!(subject.starts_with("feat(orders):"), "got: {subject}");
    }

    #[test]
    fn subject_plain_style_uses_prose() {
        let diff = vec![mk_diff(
            ChangeType::Added,
            "site/orders/service.vb",
            &["x"],
            &[],
        )];
        let subject = render_subject(ChangeKind::Feat, Some("orders"), &diff, SubjectStyle::Plain);
        assert!(subject.starts_with("Added in orders:"), "got: {subject}");
    }

    #[test]
    fn changelog_entry_skipped_for_test_kind() {
        let narrative = ChangeNarrative {
            schema_version: ChangeNarrative::SCHEMA_VERSION,
            kind: ChangeKind::Test,
            scope: None,
            subject: "test: update foo".into(),
            body_bullets: vec!["x".into()],
            affected_files: Vec::new(),
            rule_alignments: Vec::new(),
            coupling_notes: Vec::new(),
            risk_badge: "green",
            risk_rationale: "".into(),
            test_files_changed: 1,
            added_line_total: 1,
            removed_line_total: 0,
        };
        assert!(render_changelog_entry(&narrative).is_none());
    }

    #[test]
    fn changelog_entry_emitted_for_feat() {
        let narrative = ChangeNarrative {
            schema_version: ChangeNarrative::SCHEMA_VERSION,
            kind: ChangeKind::Feat,
            scope: Some("orders".into()),
            subject: "feat(orders): add audit hook".into(),
            body_bullets: vec!["Files: 1 added".into()],
            affected_files: Vec::new(),
            rule_alignments: Vec::new(),
            coupling_notes: Vec::new(),
            risk_badge: "green",
            risk_rationale: "1 file".into(),
            test_files_changed: 0,
            added_line_total: 5,
            removed_line_total: 0,
        };
        let entry = render_changelog_entry(&narrative).expect("feat must emit entry");
        assert!(entry.contains("### Added"));
        assert!(entry.contains("**orders**"));
    }

    #[test]
    fn coupling_notes_reanchor_historical_spellings_and_drop_gone_partners() {
        // Co-change history stores pre-restructure spellings
        // (`App_Code/x.vb` for today's `Site/App_Code/x.vb`). Exact-match
        // membership used to double-report partners already in the diff
        // and emit stale paths nothing on disk answers to — same defect
        // class fixed in TemporalGate; this pins the ported behavior.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = GraphStore::open(&tmp.path().join("graph.redb")).expect("GraphStore::open");
        let pid = "proj-couple";

        let mk_file = |path: &str| engram_graph::Node {
            node_id: format!("file:{path}"),
            node_type: "file".to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            namespace: "code".to_string(),
            language: "vb".to_string(),
            file_path: engram_core::RelPath::new(path),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        };
        store
            .upsert_nodes(
                pid,
                &[
                    mk_file("Site/App_Code/x.vb"),
                    mk_file("Site/App_Code/x2.vb"),
                    mk_file("Site/App_Code/partner.vb"),
                ],
            )
            .expect("upsert_nodes");

        let mk_edge = |target: &str, weight: u32| engram_graph::Edge {
            source_id: "file:Site/App_Code/x.vb".to_string(),
            target_id: format!("file:{target}"),
            namespace: "code".to_string(),
            language: "vb".to_string(),
            edge_kind: EdgeKind::TemporalCoupling,
            weight,
            generation: 1,
            metadata: None,
            updated_at_ms: 1,
        };
        store
            .upsert_edges(
                pid,
                &[
                    // Deleted long ago: no current file node, nothing on disk.
                    mk_edge("App_Code/gone.vb", 100),
                    // Historical spelling of a file that IS in the diff.
                    mk_edge("App_Code/x2.vb", 90),
                    // Historical spelling of an existing partner not in the diff.
                    mk_edge("App_Code/partner.vb", 80),
                ],
            )
            .expect("upsert_edges");

        let diff = vec![mk_diff(
            ChangeType::Modified,
            "Site/App_Code/x.vb",
            &["Dim a = 1"],
            &[],
        )];
        let changed: HashSet<String> = ["Site/App_Code/x.vb", "Site/App_Code/x2.vb"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let notes = detect_coupling_notes(&store, pid, tmp.path(), &diff, &changed);
        let partners: Vec<&str> = notes.iter().map(|n| n.partner_file.as_str()).collect();
        assert_eq!(
            partners,
            vec!["Site/App_Code/partner.vb"],
            "gone partner dropped, in-diff historical spelling skipped, \
             surviving partner re-anchored to its CURRENT spelling"
        );
        assert_eq!(notes[0].weight, 80);
    }

    #[test]
    fn coupling_notes_collapse_resx_locale_families_before_cap() {
        // A 7-locale resx family must not eat the whole 5-note budget —
        // it collapses to one family note so other coupled partners
        // still surface. A LONE resx partner keeps its own spelling.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = GraphStore::open(&tmp.path().join("graph.redb")).expect("GraphStore::open");
        let pid = "proj-resx";

        let mk_file = |path: &str| engram_graph::Node {
            node_id: format!("file:{path}"),
            node_type: "file".to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            namespace: "code".to_string(),
            language: "vb".to_string(),
            file_path: engram_core::RelPath::new(path),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        };
        let variants = [
            "Site/App_GlobalResources/label.resx",
            "Site/App_GlobalResources/label.en.resx",
            "Site/App_GlobalResources/label.de.resx",
        ];
        let mut nodes = vec![
            mk_file("Site/App_Code/x.vb"),
            mk_file("Site/App_Code/other.vb"),
            mk_file("Site/App_GlobalResources/help.resx"),
        ];
        nodes.extend(variants.iter().map(|v| mk_file(v)));
        store.upsert_nodes(pid, &nodes).expect("upsert_nodes");

        let mk_edge = |target: &str, weight: u32| engram_graph::Edge {
            source_id: "file:Site/App_Code/x.vb".to_string(),
            target_id: format!("file:{target}"),
            namespace: "code".to_string(),
            language: "vb".to_string(),
            edge_kind: EdgeKind::TemporalCoupling,
            weight,
            generation: 1,
            metadata: None,
            updated_at_ms: 1,
        };
        let mut edges: Vec<engram_graph::Edge> = variants.iter().map(|v| mk_edge(v, 200)).collect();
        edges.push(mk_edge("Site/App_GlobalResources/help.resx", 90));
        edges.push(mk_edge("Site/App_Code/other.vb", 60));
        store.upsert_edges(pid, &edges).expect("upsert_edges");

        let diff = vec![mk_diff(
            ChangeType::Modified,
            "Site/App_Code/x.vb",
            &["Dim a = 1"],
            &[],
        )];
        let changed: HashSet<String> = std::iter::once("Site/App_Code/x.vb".to_string()).collect();

        let notes = detect_coupling_notes(&store, pid, tmp.path(), &diff, &changed);
        let mut partners: Vec<&str> = notes.iter().map(|n| n.partner_file.as_str()).collect();
        partners.sort_unstable();
        assert_eq!(
            partners,
            vec![
                "Site/App_Code/other.vb",
                "Site/App_GlobalResources/help.resx",
                "Site/App_GlobalResources/label.*.resx",
            ],
            "3 locale variants collapse to one family note, lone resx keeps \
             its spelling, non-resx partner still fits in the budget"
        );
        let family = notes
            .iter()
            .find(|n| n.partner_file.ends_with("label.*.resx"))
            .expect("family note present");
        assert_eq!(
            family.weight, 200,
            "family note carries the strongest weight"
        );
    }

    #[test]
    fn commit_message_cites_rule_ids_in_footer() {
        let narrative = ChangeNarrative {
            schema_version: ChangeNarrative::SCHEMA_VERSION,
            kind: ChangeKind::Fix,
            scope: Some("orders".into()),
            subject: "fix(orders): add audit log".into(),
            body_bullets: vec!["Files: 1 modified".into()],
            affected_files: Vec::new(),
            rule_alignments: vec![RuleAlignment {
                rule_id: "cr_abc12345".into(),
                rule_text: "Call handelselogg.Create after SubmitChanges".into(),
                source: RuleAlignmentSource::CodeRabbit,
                file_path: "orders/service.vb".into(),
            }],
            coupling_notes: Vec::new(),
            risk_badge: "green",
            risk_rationale: "1 file".into(),
            test_files_changed: 0,
            added_line_total: 5,
            removed_line_total: 0,
        };
        let msg = render_commit_message(&narrative);
        assert!(msg.contains("Addresses:"));
        assert!(msg.contains("cr_abc12345 (coderabbit)"));
    }

    #[test]
    fn risk_red_when_immune_flagged() {
        let mut affected = vec![mk_affected("src/dal/users.vb", "modified")];
        affected[0].is_immune_flagged = true;
        let (badge, _) = compute_risk(&affected, &[]);
        assert_eq!(badge, "red");
    }

    #[test]
    fn risk_yellow_on_many_files() {
        let affected: Vec<AffectedFile> = (0..12)
            .map(|i| mk_affected(&format!("src/f{i}.rs"), "modified"))
            .collect();
        let (badge, _) = compute_risk(&affected, &[]);
        assert_eq!(badge, "yellow");
    }

    #[test]
    fn risk_green_on_small_safe_change() {
        let affected = vec![mk_affected("src/foo.rs", "modified")];
        let (badge, _) = compute_risk(&affected, &[]);
        assert_eq!(badge, "green");
    }

    #[test]
    fn body_bullets_include_file_shape() {
        let affected = vec![
            mk_affected("a.rs", "added"),
            mk_affected("b.rs", "modified"),
            mk_affected("c.rs", "deleted"),
        ];
        let bullets = render_body_bullets(ChangeKind::Refactor, &affected, &[]);
        let joined = bullets.join("\n");
        assert!(joined.contains("1 added, 1 modified, 1 deleted"));
    }

    #[test]
    fn pr_description_groups_by_change_type() {
        let narrative = ChangeNarrative {
            schema_version: ChangeNarrative::SCHEMA_VERSION,
            kind: ChangeKind::Feat,
            scope: Some("orders".into()),
            subject: "feat(orders): introduce service".into(),
            body_bullets: vec!["bullet".into()],
            affected_files: vec![
                mk_affected("site/orders/service.vb", "added"),
                mk_affected("site/orders/existing.vb", "modified"),
            ],
            rule_alignments: Vec::new(),
            coupling_notes: Vec::new(),
            risk_badge: "yellow",
            risk_rationale: "2 files".into(),
            test_files_changed: 0,
            added_line_total: 15,
            removed_line_total: 0,
        };
        let md = render_pr_description(&narrative);
        assert!(md.contains("### added"));
        assert!(md.contains("### modified"));
        assert!(md.contains("🟡"));
    }

    #[test]
    fn whitespace_only_change_classifies_as_style() {
        // Added and removed lines are the same once whitespace is
        // normalised away.
        let diff = vec![mk_diff(
            ChangeType::Modified,
            "src/a.rs",
            &["    fn main() {}"],
            &["fn main() {}"],
        )];
        let affected = vec![mk_affected("src/a.rs", "modified")];
        assert_eq!(classify_change_kind(&diff, &affected), ChangeKind::Style);
    }
}
