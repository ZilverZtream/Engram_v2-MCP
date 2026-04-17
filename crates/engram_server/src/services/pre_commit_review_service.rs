//! Pre-commit review service.
//!
//! Engram's flagship agentic-workflow tool: takes a unified diff (raw text,
//! or one of the "staged"/"unstaged"/"head" shortcuts that read from the
//! project's git repo via `git2`) and runs it through ten deterministic
//! analysis gates powered by the knowledge graph, git history, immune
//! system, temporal couplings, state workflows, and coding-style detectors.
//! The result is a structured review with severity-ranked, actionable
//! findings — every finding carries specific evidence (blast radius,
//! revert hash, coupling weight, reader/writer counts, convention stats).
//!
//! Design goals:
//!
//! 1. **Gate independence.** Each gate runs independently — failure in one
//!    gate doesn't stop the others. Findings merge at the end.
//!
//! 2. **Severity-first output.** Findings sort by severity (CRITICAL first),
//!    not gate order. The most important items surface first.
//!
//! 3. **Evidence-backed only.** Every finding cites the specific graph data
//!    that supports it. No assertions without evidence.
//!
//! 4. **Zero false positives on CRITICAL.** CRITICAL is reserved for immune
//!    violations paired with destructive patterns and for hardcoded secrets
//!    — both represent actual past incidents or current compliance
//!    failures. False CRITICAL erodes trust in the whole tool.
//!
//! 5. **Deterministic and fast.** No LLM / network calls. All analysis
//!    uses the local graph, repo rules, and filesystem. Target: <5s for a
//!    500-line diff on a 50k-node project.
//!
//! 6. **Diff-aware, not file-aware.** Gates analyse the CHANGED code, not
//!    the entire file. Where whole-file context is needed (blast radius,
//!    immune match), the gate uses the file path but scopes findings to
//!    the diff.
//!
//! 7. **Prescriptive.** Every finding includes a specific, actionable
//!    `suggestion` field. Findings without a fix are not findings — they
//!    are noise.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use engram_core::registry::{Registry, RepoRule};
use engram_graph::GraphStore;
use serde::Serialize;

use crate::state::AppState;

// ─── Public types ───────────────────────────────────────────────────────────

/// Severity ladder. Ordered so `Critical < Warning < Info < Style` — that
/// way `Ord::cmp` gives Critical first when sorting ascending, matching
/// how we render output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Production-incident-level: immune violations paired with destructive
    /// code, or hardcoded credentials. Never a false positive.
    Critical,
    /// Should fix before merging: missing audit log, strong temporal
    /// coupling partner not in the diff, missing-test-file gap.
    Warning,
    /// Good to know but not blocking: blast-radius context, state-key
    /// readers/writers touched, moderate coupling.
    Info,
    /// Convention / naming / formatting nits.
    Style,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Style => "style",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "critical" => Some(Self::Critical),
            "warning" => Some(Self::Warning),
            "info" => Some(Self::Info),
            "style" => Some(Self::Style),
            _ => None,
        }
    }

    /// Emoji marker for markdown headings.
    pub fn emoji(self) -> &'static str {
        match self {
            Self::Critical => "🔴",
            Self::Warning => "🟡",
            Self::Info => "🔵",
            Self::Style => "🟣",
        }
    }
}

/// A single finding produced by a gate.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewFinding {
    pub severity: Severity,
    pub gate: &'static str,
    pub file_path: String,
    /// Optional line numbers (within the new file's numbering) where the
    /// finding applies. Empty for whole-file findings.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<usize>,
    /// One-line summary — shown as the finding's header.
    pub title: String,
    /// Longer explanation with context.
    pub detail: String,
    /// Specific, actionable fix. REQUIRED for a finding to be useful; if
    /// you can't write one, the finding shouldn't fire.
    pub suggestion: String,
    /// Evidence references — graph data that supports this finding.
    /// Examples: "blast_radius: 7/10", "revert_hash: abc123...", "coupling_weight: 877".
    pub evidence: Vec<String>,
    /// Optional diff snippet (±2 lines of context around the flagged line).
    /// Populated after aggregation so agents don't need to open the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_snippet: Option<String>,
    /// Optional follow-up tool suggestion — e.g.
    /// `impact_analysis(file_path="…")`. Gives an agent a concrete next
    /// step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_tool: Option<String>,
    /// Stable ID computed from (gate, file, title, lines). CI can diff
    /// this run-to-run to track new vs. persisting findings.
    pub finding_id: String,
}

impl ReviewFinding {
    /// Helper constructor — callers build most of the fields and we fill
    /// `finding_id` + `diff_snippet` / `next_tool` defaults.
    pub fn new(
        severity: Severity,
        gate: &'static str,
        file_path: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
        suggestion: impl Into<String>,
    ) -> Self {
        let file_path = file_path.into();
        let title = title.into();
        let finding_id = stable_finding_id(gate, &file_path, &title, &[]);
        Self {
            severity,
            gate,
            file_path,
            lines: Vec::new(),
            title,
            detail: detail.into(),
            suggestion: suggestion.into(),
            evidence: Vec::new(),
            diff_snippet: None,
            next_tool: None,
            finding_id,
        }
    }

    pub fn with_lines(mut self, lines: Vec<usize>) -> Self {
        self.finding_id = stable_finding_id(self.gate, &self.file_path, &self.title, &lines);
        self.lines = lines;
        self
    }

    pub fn with_evidence(mut self, ev: Vec<String>) -> Self {
        self.evidence = ev;
        self
    }

    pub fn with_next_tool(mut self, t: impl Into<String>) -> Self {
        self.next_tool = Some(t.into());
        self
    }
}

/// Stable 12-hex-char ID derived from the finding's identifying fields.
/// Agents / CI use this to correlate the same finding across runs.
pub fn stable_finding_id(gate: &str, file: &str, title: &str, lines: &[usize]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    gate.hash(&mut hasher);
    file.hash(&mut hasher);
    title.hash(&mut hasher);
    for l in lines {
        l.hash(&mut hasher);
    }
    let h = hasher.finish();
    format!("{h:016x}")[..12].to_string()
}

/// Overall review verdict — the single signal an agent or CI reads to
/// decide what to do. Mirrors `check_edit_safety`'s green/yellow/red.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Nothing of concern — only STYLE findings (or none at all).
    Green,
    /// Review recommended — WARNING or INFO findings present.
    Yellow,
    /// Must not merge as-is — at least one CRITICAL finding.
    Red,
}

impl Verdict {
    pub fn from_findings(findings: &[ReviewFinding]) -> Self {
        if findings.iter().any(|f| f.severity == Severity::Critical) {
            Self::Red
        } else if findings
            .iter()
            .any(|f| matches!(f.severity, Severity::Warning | Severity::Info))
        {
            Self::Yellow
        } else {
            Self::Green
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            Self::Green => "🟢",
            Self::Yellow => "🟡",
            Self::Red => "🔴",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Red => "red",
        }
    }
}

/// Change type for a file in a diff. `Renamed` carries the prior path so
/// downstream gates can still resolve graph nodes by the old name when
/// needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "from", rename_all = "snake_case")]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Renamed(String),
}

/// Parsed unified-diff contribution for a single file.
#[derive(Debug, Clone, Serialize)]
pub struct DiffFile {
    pub path: String,
    pub change_type: ChangeType,
    /// `(new_line_number, content)` — added lines only.
    pub added_lines: Vec<(usize, String)>,
    /// `(old_line_number, content)` — removed lines only.
    pub removed_lines: Vec<(usize, String)>,
    /// Concatenated added-line contents (for substring / regex scans).
    pub added_content: String,
    /// Concatenated removed-line contents.
    pub removed_content: String,
    /// Parsed hunks — kept so renderers can surface ±2 lines of context.
    pub hunks: Vec<DiffHunk>,
    /// True when the diff header marked the file as binary — content
    /// vectors are empty and gates should skip.
    pub is_binary: bool,
}

impl DiffFile {
    pub fn is_test_file(&self) -> bool {
        is_test_path(&self.path)
    }
}

/// A single hunk inside a diff, preserving enough context to render
/// snippets.
#[derive(Debug, Clone, Serialize)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    /// Raw body lines, each prefixed with ` `, `+`, or `-`. Kept verbatim
    /// so the renderer can show the user the exact diff text.
    pub body: Vec<String>,
}

/// Detect whether a path is a test file by common suffix conventions
/// across ecosystems. Used by Gate 9 (test coverage) and Gate 8 (new-file
/// conventions) so they don't disagree.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let fname = lower.rsplit('/').next().unwrap_or(&lower);
    // Rust / Python / Go
    if fname.starts_with("test_") || fname.ends_with("_test.rs") || fname.ends_with("_test.go")
        || fname.ends_with("_test.py")
    {
        return true;
    }
    // JS / TS / Jest / Vitest
    if fname.ends_with(".test.js")
        || fname.ends_with(".test.ts")
        || fname.ends_with(".test.tsx")
        || fname.ends_with(".test.jsx")
        || fname.ends_with(".spec.js")
        || fname.ends_with(".spec.ts")
        || fname.ends_with(".spec.tsx")
        || fname.ends_with(".spec.jsx")
    {
        return true;
    }
    // .NET / Java
    if fname.ends_with("tests.cs") || fname.ends_with("tests.vb") || fname.ends_with("test.cs")
        || fname.ends_with("test.java") || fname.ends_with("tests.java")
    {
        return true;
    }
    // Path-based
    lower.contains("/tests/") || lower.contains("/__tests__/") || lower.contains("/spec/")
}

// ─── Gate trait ─────────────────────────────────────────────────────────────

/// Context passed to each gate. Carries everything a gate might need —
/// gates borrow from `GateContext` rather than capturing individual args,
/// which keeps the trait object-safe and makes it trivial to add new
/// dependencies without touching every gate.
///
/// Several fields are **pre-computed once** per review and shared across
/// all gates (`repo_rules`, `files_by_parent`, `audit_function`) — this
/// removes ~2–5× duplicate graph scans on multi-file diffs compared to
/// each gate running its own lookup.
pub struct GateContext<'a> {
    pub state: &'a AppState,
    pub graph: Arc<GraphStore>,
    pub registry: Arc<Registry>,
    pub project_id: &'a str,
    pub project_dir: &'a Path,
    pub generation: u64,
    pub diff_files: &'a [DiffFile],
    /// Cached set of changed paths — avoids rebuilding it per gate.
    pub changed_paths: &'a HashSet<String>,
    /// Total commits in the project (for threshold auto-tuning). Best-effort.
    pub total_commits: u32,
    /// All repo rules for the project, loaded once. Gate 1 (immune)
    /// filters this rather than re-running `list_repo_rules` per gate.
    pub repo_rules: Arc<Vec<RepoRule>>,
    /// File-node paths grouped by parent directory, loaded once. Gate 8
    /// (new-file) uses this instead of running `query_nodes(limit=1000)`
    /// for every added file.
    pub files_by_parent: Arc<HashMap<String, Vec<String>>>,
    /// Audit function name detected by querying function nodes once.
    /// `None` when the project has no detectable audit convention —
    /// Gate 6 (audit) short-circuits in that case.
    pub audit_function: Option<String>,
}

/// Gate trait. Every gate has a stable name + an implementation that
/// returns findings. Sync gates return from `run`; async gates (Gate 7,
/// anti-pattern search) override `run_async`.
///
/// `#[async_trait]` on the trait itself is what makes it `dyn`-safe —
/// the macro boxes the returned future so `Vec<Box<dyn Gate>>` works.
#[async_trait::async_trait]
pub trait Gate: Send + Sync {
    fn name(&self) -> &'static str;

    /// Default sync implementation — gates that need async override
    /// `run_async` instead.
    fn run(&self, _ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        Ok(Vec::new())
    }

    /// Async entry point. Default implementation delegates to the sync
    /// `run` so most gates only need to implement the sync version.
    async fn run_async(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        self.run(ctx)
    }
}

// ─── Diff parser ────────────────────────────────────────────────────────────

/// Parse a unified diff into `DiffFile`s.
///
/// Handles: new files (`--- /dev/null`), deleted files (`+++ /dev/null`),
/// renamed files (`rename from` / `rename to` headers), and binary files
/// (marked so downstream skips them). Multiple hunks per file are
/// concatenated into `added_content` / `removed_content` for fast
/// substring / regex scans, while `hunks` keeps the raw structure for
/// renderers.
///
/// Performance: `added_content` / `removed_content` buffers are
/// pre-sized from the total diff length so large inputs don't incur
/// `String::reserve` growth costs mid-parse. Files / hunks vectors are
/// pre-sized from `diff --git` header density.
pub fn parse_unified_diff(diff_text: &str) -> Vec<DiffFile> {
    // Heuristic pre-sizing: average 40 bytes per line in unified diffs,
    // and one `diff --git` header per ~30 lines on typical repos. The
    // overestimates are cheap — undersizing forces multiple re-grows.
    let approx_lines = diff_text.len() / 40;
    let approx_files = diff_text.matches("\ndiff --git ").count()
        + usize::from(diff_text.starts_with("diff --git "));
    let mut files: Vec<DiffFile> = Vec::with_capacity(approx_files.max(1));
    let mut current: Option<DiffFile> = None;
    let mut current_hunk: Option<DiffHunk> = None;
    let mut new_line = 0usize;
    let mut old_line = 0usize;
    let _ = approx_lines; // kept for future: per-file line-count reservation

    for raw_line in diff_text.lines() {
        // ── File header: `diff --git a/PATH b/PATH` ───────────────────
        if let Some(rest) = raw_line.strip_prefix("diff --git ") {
            // Flush previous file.
            if let Some(mut f) = current.take() {
                if let Some(h) = current_hunk.take() {
                    f.hunks.push(h);
                }
                files.push(f);
            }
            // `a/foo b/foo` → path
            let path = rest
                .split_whitespace()
                .last()
                .and_then(|s| s.strip_prefix("b/").or_else(|| s.strip_prefix("a/")))
                .unwrap_or(rest)
                .to_string();
            current = Some(DiffFile {
                path,
                change_type: ChangeType::Modified,
                // Pre-size assuming ~64 added + 64 removed lines for an
                // average-sized file change. Small overallocation is
                // cheap; avoiding 6–8 Vec re-grows on big files is not.
                added_lines: Vec::with_capacity(64),
                removed_lines: Vec::with_capacity(64),
                added_content: String::with_capacity(2 * 1024),
                removed_content: String::with_capacity(2 * 1024),
                hunks: Vec::with_capacity(4),
                is_binary: false,
            });
            current_hunk = None;
            new_line = 0;
            old_line = 0;
            continue;
        }

        let Some(f) = current.as_mut() else { continue };

        // Binary marker — git emits `Binary files ... differ`.
        if raw_line.starts_with("Binary files ") || raw_line.starts_with("GIT binary patch") {
            f.is_binary = true;
            continue;
        }

        // Rename markers.
        if let Some(rest) = raw_line.strip_prefix("rename from ") {
            f.change_type = ChangeType::Renamed(rest.to_string());
            continue;
        }
        if let Some(_rest) = raw_line.strip_prefix("rename to ") {
            // Path was already captured from `diff --git b/…`.
            continue;
        }

        // `--- /dev/null` → Added. `+++ /dev/null` → Deleted.
        if let Some(rest) = raw_line.strip_prefix("--- ") {
            if rest.trim() == "/dev/null" {
                f.change_type = ChangeType::Added;
            }
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix("+++ ") {
            if rest.trim() == "/dev/null" {
                f.change_type = ChangeType::Deleted;
            }
            continue;
        }

        // Hunk header: `@@ -old_start,old_count +new_start,new_count @@ context`.
        if let Some(hh) = parse_hunk_header(raw_line) {
            if let Some(h) = current_hunk.take() {
                f.hunks.push(h);
            }
            old_line = hh.old_start;
            new_line = hh.new_start;
            current_hunk = Some(hh);
            continue;
        }

        // Content lines inside a hunk.
        let Some(hunk) = current_hunk.as_mut() else { continue };
        hunk.body.push(raw_line.to_string());

        if let Some(rest) = raw_line.strip_prefix('+') {
            // A leading `+++` header would have been caught above; only
            // content `+` prefixes reach here.
            if !rest.starts_with('+') {
                f.added_lines.push((new_line, rest.to_string()));
                if !f.added_content.is_empty() {
                    f.added_content.push('\n');
                }
                f.added_content.push_str(rest);
                new_line += 1;
            }
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix('-') {
            if !rest.starts_with('-') {
                f.removed_lines.push((old_line, rest.to_string()));
                if !f.removed_content.is_empty() {
                    f.removed_content.push('\n');
                }
                f.removed_content.push_str(rest);
                old_line += 1;
            }
            continue;
        }
        if raw_line.starts_with(' ') || raw_line.is_empty() {
            // Context line — both sides advance.
            old_line += 1;
            new_line += 1;
            continue;
        }
    }

    if let Some(mut f) = current.take() {
        if let Some(h) = current_hunk.take() {
            f.hunks.push(h);
        }
        files.push(f);
    }

    files
}

fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    // `@@ -a,b +c,d @@ context`
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let head = &rest[..end];
    let mut parts = head.split_whitespace();
    let old_spec = parts.next()?.strip_prefix('-')?;
    let new_spec = parts.next()?.strip_prefix('+')?;

    fn split_spec(s: &str) -> (usize, usize) {
        let mut it = s.splitn(2, ',');
        let start = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let count = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
        (start, count)
    }
    let (old_start, old_count) = split_spec(old_spec);
    let (new_start, new_count) = split_spec(new_spec);

    Some(DiffHunk {
        old_start,
        old_count,
        new_start,
        new_count,
        body: Vec::new(),
    })
}

// ─── Diff source resolver (git2) ────────────────────────────────────────────

/// Resolve the `diff` field of a request into raw unified-diff text.
///
/// Supported forms:
/// - `"staged"` — `git diff --staged` equivalent (HEAD tree vs. index)
/// - `"unstaged"` — `git diff` equivalent (index vs. working tree)
/// - `"head"` — `git diff HEAD~1` equivalent (last commit)
/// - a path ending in `.patch` or `.diff` — read from disk
/// - anything else — treated as raw unified-diff text
///
/// Uses `git2` (already a dep via `engram_git`) — no shell calls.
pub fn resolve_diff_source(project_dir: &Path, diff_input: &str) -> anyhow::Result<String> {
    match diff_input.trim() {
        "staged" => git_diff_staged(project_dir),
        "unstaged" => git_diff_unstaged(project_dir),
        "head" => git_diff_head(project_dir),
        path if path.ends_with(".patch") || path.ends_with(".diff") => {
            let p = project_dir.join(path);
            std::fs::read_to_string(&p)
                .map_err(|e| anyhow::anyhow!("failed to read diff file {}: {e}", p.display()))
        }
        other => Ok(other.to_string()),
    }
}

fn diff_to_patch_text(diff: &git2::Diff<'_>) -> anyhow::Result<String> {
    let mut text = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        let origin = line.origin();
        // `git2` passes single-byte origin markers for content lines
        // (` `, `+`, `-`) and letters (`F`, `H`, ...) for headers — the
        // letters would corrupt the diff output if we wrote them, so
        // only prepend the real content prefixes.
        match origin {
            ' ' | '+' | '-' => text.push(origin),
            _ => {}
        }
        if let Ok(s) = std::str::from_utf8(line.content()) {
            text.push_str(s);
        }
        true
    })?;
    Ok(text)
}

fn git_diff_staged(project_dir: &Path) -> anyhow::Result<String> {
    let repo = git2::Repository::discover(project_dir)?;
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let mut opts = git2::DiffOptions::new();
    opts.include_untracked(false);
    let diff = repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))?;
    diff_to_patch_text(&diff)
}

fn git_diff_unstaged(project_dir: &Path) -> anyhow::Result<String> {
    let repo = git2::Repository::discover(project_dir)?;
    let mut opts = git2::DiffOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let diff = repo.diff_index_to_workdir(None, Some(&mut opts))?;
    diff_to_patch_text(&diff)
}

fn git_diff_head(project_dir: &Path) -> anyhow::Result<String> {
    let repo = git2::Repository::discover(project_dir)?;
    let head_commit = repo.head()?.peel_to_commit()?;
    let parent = head_commit.parent(0).ok();
    let new_tree = head_commit.tree()?;
    let old_tree = parent.as_ref().and_then(|p| p.tree().ok());
    let diff = repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)?;
    diff_to_patch_text(&diff)
}

// ─── Structured conventions ─────────────────────────────────────────────────

/// Structured convention extracted from a file. Gates consume this rather
/// than parsing the bullet strings emitted by
/// `cognitive_service::static_analyze_file_style` — parsing prose is
/// fragile, structured extraction lets us evolve the bullet wording
/// without breaking gates.
#[derive(Debug, Clone)]
pub struct DetectedConvention {
    pub category: ConventionCategory,
    /// Canonical value — e.g., `"PascalCase"`, `"camelCase"`, `"spaces"`,
    /// `"4"` (indent width), `"double"` (string quotes).
    pub value: String,
    /// Sample size that supported this decision.
    pub sample_count: usize,
    /// Total observations (winner / total as a fraction).
    pub total_count: usize,
}

impl DetectedConvention {
    /// Confidence = winner-fraction × log2(sample_count + 1) / 10, clipped
    /// to [0, 1]. Big samples with a clear winner give confidence ~1.
    /// Small samples give low confidence, so gates skip them.
    pub fn confidence(&self) -> f32 {
        if self.total_count == 0 {
            return 0.0;
        }
        let frac = self.sample_count as f32 / self.total_count as f32;
        let log_factor = (self.sample_count as f32 + 1.0).log2() / 8.0;
        (frac * log_factor).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConventionCategory {
    MethodNaming,
    ClassNaming,
    VariableNaming,
    Indentation,
    StringQuotes,
    ErrorHandling,
    ModuleSystem,
    Semicolons,
    ContextInjection,
    ResourceOwnership,
    NullGuard,
    AuditLog,
    RedirectPattern,
    InterfacePrefix,
    CastStyle,
    PrivateFieldPrefix,
}

/// Extract structured conventions from a file's content. Mirrors the
/// dispatch in `static_analyze_file_style` but returns structured data
/// instead of human-readable bullets.
///
/// Scoped intentionally narrow: we only extract the categories that the
/// style-compliance gate can act on. Adding a category here automatically
/// makes it available to future gates — don't put it in here unless a
/// gate actually checks it.
pub fn extract_conventions(content: &str, file_path: &str) -> Vec<DetectedConvention> {
    let lower = file_path.to_ascii_lowercase();
    let mut out: Vec<DetectedConvention> = Vec::new();

    if lower.ends_with(".vb") {
        extract_vb_conventions(content, &mut out);
    } else if lower.ends_with(".cs") {
        extract_cs_conventions(content, &mut out);
    } else if lower.ends_with(".ts") || lower.ends_with(".tsx") {
        extract_ts_conventions(content, &mut out);
    } else if lower.ends_with(".js") || lower.ends_with(".jsx") || lower.ends_with(".mjs") {
        extract_js_conventions(content, &mut out);
    } else if lower.ends_with(".sql") {
        extract_sql_conventions(content, &mut out);
    } else if lower.ends_with(".py") {
        extract_py_conventions(content, &mut out);
    } else if lower.ends_with(".rs") {
        extract_rust_conventions(content, &mut out);
    }

    // Universal: indentation style applies to any text file.
    extract_indent_convention(content, &mut out);
    out
}

fn observe_casing(name: &str, buckets: &mut [usize; 3]) {
    // [pascal, camel, snake]
    if name.is_empty() {
        return;
    }
    if name.contains('_') {
        buckets[2] += 1;
    } else if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        buckets[0] += 1;
    } else if name.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
        buckets[1] += 1;
    }
}

fn publish_casing(
    out: &mut Vec<DetectedConvention>,
    category: ConventionCategory,
    buckets: [usize; 3],
) {
    let total = buckets.iter().sum::<usize>();
    if total < 3 {
        return;
    }
    let (idx, count) = buckets
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| **c)
        .map(|(i, c)| (i, *c))
        .unwrap();
    let value = match idx {
        0 => "PascalCase",
        1 => "camelCase",
        _ => "snake_case",
    };
    out.push(DetectedConvention {
        category,
        value: value.to_string(),
        sample_count: count,
        total_count: total,
    });
}

fn extract_vb_conventions(content: &str, out: &mut Vec<DetectedConvention>) {
    use regex::Regex;
    use std::sync::LazyLock;
    static METHOD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"(?im)^\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Sub|Function)\s+(\w+)\s*\(",
        )
        .ok()
    });
    static OPTIONAL_CTX_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?i)\bOptional\s+(?:ByVal\s+|ByRef\s+)?\w+\s+As\s+(\w+(?:DataContext|Context|Db))\s*=\s*Nothing").ok()
    });
    static USING_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*Using\s+").ok());
    static TRY_CATCH_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*Try\s*$|^\s*Catch\b").ok());
    static ON_ERROR_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*On\s+Error\s+Resume\s+Next\b").ok());
    static SAFE_REDIRECT_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)\bSafeRedirect\s*\(").ok());

    let mut buckets = [0usize; 3];
    if let Some(re) = METHOD_RE.as_ref() {
        for cap in re.captures_iter(content).take(500) {
            if let Some(m) = cap.get(1) {
                observe_casing(m.as_str(), &mut buckets);
            }
        }
    }
    publish_casing(out, ConventionCategory::MethodNaming, buckets);

    // Context injection — Optional db As <DataContext> = Nothing.
    let ctx_count = OPTIONAL_CTX_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if ctx_count >= 2 {
        out.push(DetectedConvention {
            category: ConventionCategory::ContextInjection,
            value: "Optional db = Nothing".into(),
            sample_count: ctx_count,
            total_count: ctx_count,
        });
    }

    // Using discipline.
    let using_count = USING_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if using_count >= 3 {
        out.push(DetectedConvention {
            category: ConventionCategory::ResourceOwnership,
            value: "Using".into(),
            sample_count: using_count,
            total_count: using_count,
        });
    }

    // Error handling — Try/Catch vs On Error.
    let try_catch = TRY_CATCH_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let on_error = ON_ERROR_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if try_catch > 0 && on_error == 0 {
        out.push(DetectedConvention {
            category: ConventionCategory::ErrorHandling,
            value: "Try/Catch".into(),
            sample_count: try_catch,
            total_count: try_catch,
        });
    }

    // SafeRedirect convention (OciusX-style — generic test: "project uses a
    // SafeRedirect helper"). Only emitted when we see ≥3 uses; one-off calls
    // don't constitute a convention.
    let sr = SAFE_REDIRECT_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if sr >= 3 {
        out.push(DetectedConvention {
            category: ConventionCategory::RedirectPattern,
            value: "SafeRedirect".into(),
            sample_count: sr,
            total_count: sr,
        });
    }
}

fn extract_cs_conventions(content: &str, out: &mut Vec<DetectedConvention>) {
    use regex::Regex;
    use std::sync::LazyLock;
    static METHOD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^\s*(?:public|private|protected|internal|static|virtual|override|async|sealed|abstract|new|partial)\s+(?:[\w<>\[\],\?\s]+?\s+)?(\w+)\s*\(",
        )
        .ok()
    });
    let mut buckets = [0usize; 3];
    if let Some(re) = METHOD_RE.as_ref() {
        for cap in re.captures_iter(content).take(500) {
            if let Some(m) = cap.get(1) {
                observe_casing(m.as_str(), &mut buckets);
            }
        }
    }
    publish_casing(out, ConventionCategory::MethodNaming, buckets);
}

fn extract_ts_conventions(content: &str, out: &mut Vec<DetectedConvention>) {
    extract_js_conventions(content, out);
    // Interface prefix: `I`-prefix count vs plain.
    use regex::Regex;
    use std::sync::LazyLock;
    static IFACE_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:export\s+)?interface\s+(\w+)").ok());
    let mut i_pref = 0usize;
    let mut plain = 0usize;
    if let Some(re) = IFACE_RE.as_ref() {
        for cap in re.captures_iter(content).take(500) {
            if let Some(m) = cap.get(1) {
                let name = m.as_str();
                let chars: Vec<char> = name.chars().collect();
                if chars.len() >= 2 && chars[0] == 'I' && chars[1].is_ascii_uppercase() {
                    i_pref += 1;
                } else {
                    plain += 1;
                }
            }
        }
    }
    let total = i_pref + plain;
    if total >= 3 {
        if i_pref > plain {
            out.push(DetectedConvention {
                category: ConventionCategory::InterfacePrefix,
                value: "I-prefix".into(),
                sample_count: i_pref,
                total_count: total,
            });
        } else if plain > i_pref * 2 {
            out.push(DetectedConvention {
                category: ConventionCategory::InterfacePrefix,
                value: "no-prefix".into(),
                sample_count: plain,
                total_count: total,
            });
        }
    }

    // Cast style — angle-bracket vs `as`.
    static ANGLE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"<(?:HTML\w+|SVG\w+|any|unknown|[A-Z]\w*(?:\s*\[\s*\])?)>[\w\(]").ok()
    });
    static AS_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"\bas\s+(?:HTML\w+|SVG\w+|[A-Z]\w*(?:\s*\[\s*\])?)\b").ok()
    });
    let angle = ANGLE_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let as_c = AS_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let tot = angle + as_c;
    if tot >= 4 {
        if angle > as_c + 2 {
            out.push(DetectedConvention {
                category: ConventionCategory::CastStyle,
                value: "angle-bracket".into(),
                sample_count: angle,
                total_count: tot,
            });
        } else if as_c > angle + 2 {
            out.push(DetectedConvention {
                category: ConventionCategory::CastStyle,
                value: "as".into(),
                sample_count: as_c,
                total_count: tot,
            });
        }
    }
}

fn extract_js_conventions(content: &str, out: &mut Vec<DetectedConvention>) {
    use regex::Regex;
    use std::sync::LazyLock;

    // Function naming
    static FUNC_DECL_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)\s*\(").ok()
    });
    static ARROW_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?const\s+(\w+)\s*=\s*(?:async\s+)?\(").ok()
    });
    let mut buckets = [0usize; 3];
    for re in [FUNC_DECL_RE.as_ref(), ARROW_RE.as_ref()].iter().flatten() {
        for cap in re.captures_iter(content).take(500) {
            if let Some(m) = cap.get(1) {
                observe_casing(m.as_str(), &mut buckets);
            }
        }
    }
    publish_casing(out, ConventionCategory::MethodNaming, buckets);

    // String quotes
    let dbl = content.matches('"').count() / 2;
    let sng = content.matches('\'').count() / 2;
    let tot = dbl + sng;
    if tot >= 10 {
        let (winner, count) = if dbl > sng { ("double", dbl) } else { ("single", sng) };
        let frac = count as f32 / tot as f32;
        if frac >= 0.7 {
            out.push(DetectedConvention {
                category: ConventionCategory::StringQuotes,
                value: winner.into(),
                sample_count: count,
                total_count: tot,
            });
        }
    }

    // Semicolons
    let code_lines = content
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("//") && !t.starts_with("*") && !t.starts_with("/*")
        })
        .count();
    let semi_lines = content
        .lines()
        .filter(|l| l.trim_end().ends_with(';'))
        .count();
    if code_lines > 20 {
        let ratio = semi_lines as f32 / code_lines as f32;
        if ratio > 0.7 {
            out.push(DetectedConvention {
                category: ConventionCategory::Semicolons,
                value: "required".into(),
                sample_count: semi_lines,
                total_count: code_lines,
            });
        } else if ratio < 0.1 {
            out.push(DetectedConvention {
                category: ConventionCategory::Semicolons,
                value: "omitted".into(),
                sample_count: code_lines - semi_lines,
                total_count: code_lines,
            });
        }
    }

    // Module system — triple-slash vs ES6.
    let es6 = content.matches("import {").count()
        + content.matches("import *").count()
        + content.matches("import type ").count();
    let triple = content.matches("/// <reference").count();
    if es6 + triple >= 3 {
        if triple >= es6 && triple >= 3 {
            out.push(DetectedConvention {
                category: ConventionCategory::ModuleSystem,
                value: "triple-slash".into(),
                sample_count: triple,
                total_count: es6 + triple,
            });
        } else if es6 > triple {
            out.push(DetectedConvention {
                category: ConventionCategory::ModuleSystem,
                value: "es6-import".into(),
                sample_count: es6,
                total_count: es6 + triple,
            });
        }
    }

    // Private-field underscore convention.
    static UNDERSCORE_FIELD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:private|protected|public|static|readonly|\s)*\s*_\w+\s*[:=]").ok()
    });
    let uf = UNDERSCORE_FIELD_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if uf >= 4 {
        out.push(DetectedConvention {
            category: ConventionCategory::PrivateFieldPrefix,
            value: "_underscore".into(),
            sample_count: uf,
            total_count: uf,
        });
    }
}

fn extract_sql_conventions(content: &str, out: &mut Vec<DetectedConvention>) {
    // Keyword casing.
    let upper = ["SELECT ", "FROM ", "INSERT ", "UPDATE ", "DELETE ", "WHERE ", "JOIN "]
        .iter()
        .map(|k| content.matches(k).count())
        .sum::<usize>();
    let lower = ["select ", "from ", "insert ", "update ", "delete ", "where ", "join "]
        .iter()
        .map(|k| content.matches(k).count())
        .sum::<usize>();
    let tot = upper + lower;
    if tot >= 5 {
        let (winner, count) = if upper > lower {
            ("UPPERCASE", upper)
        } else {
            ("lowercase", lower)
        };
        let frac = count as f32 / tot as f32;
        if frac >= 0.7 {
            out.push(DetectedConvention {
                category: ConventionCategory::ErrorHandling, // piggyback — SQL has no real "err" cat
                value: winner.into(),
                sample_count: count,
                total_count: tot,
            });
        }
    }
}

fn extract_py_conventions(content: &str, out: &mut Vec<DetectedConvention>) {
    use regex::Regex;
    use std::sync::LazyLock;
    static DEF_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:async\s+)?def\s+(\w+)\s*\(").ok());
    let mut buckets = [0usize; 3];
    if let Some(re) = DEF_RE.as_ref() {
        for cap in re.captures_iter(content).take(500) {
            if let Some(m) = cap.get(1) {
                observe_casing(m.as_str(), &mut buckets);
            }
        }
    }
    publish_casing(out, ConventionCategory::MethodNaming, buckets);
}

fn extract_rust_conventions(content: &str, out: &mut Vec<DetectedConvention>) {
    use regex::Regex;
    use std::sync::LazyLock;
    static FN_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\(crate\))?\s+)?(?:async\s+)?fn\s+(\w+)").ok()
    });
    let mut buckets = [0usize; 3];
    if let Some(re) = FN_RE.as_ref() {
        for cap in re.captures_iter(content).take(500) {
            if let Some(m) = cap.get(1) {
                observe_casing(m.as_str(), &mut buckets);
            }
        }
    }
    publish_casing(out, ConventionCategory::MethodNaming, buckets);
}

fn extract_indent_convention(content: &str, out: &mut Vec<DetectedConvention>) {
    let tab_lines = content.lines().filter(|l| l.starts_with('\t')).count();
    let space_lines = content
        .lines()
        .filter(|l| l.starts_with("    ") || l.starts_with("  "))
        .count();
    if tab_lines + space_lines < 10 {
        return;
    }
    let (value, count, total) = if tab_lines > space_lines * 2 {
        ("tabs", tab_lines, tab_lines + space_lines)
    } else if space_lines > tab_lines * 2 {
        // Look up the dominant space width.
        let mut two = 0usize;
        let mut four = 0usize;
        for l in content.lines() {
            if l.starts_with("    ") && !l.starts_with("     ") {
                four += 1;
            } else if l.starts_with("  ") && !l.starts_with("   ") {
                two += 1;
            }
        }
        let v = if four >= two { "spaces-4" } else { "spaces-2" };
        (v, space_lines, tab_lines + space_lines)
    } else {
        return;
    };
    out.push(DetectedConvention {
        category: ConventionCategory::Indentation,
        value: value.into(),
        sample_count: count,
        total_count: total,
    });
}

// ─── Aggregation ────────────────────────────────────────────────────────────

/// Finalise the finding list: dedup by `finding_id`, sort, attach diff
/// snippets from the originating hunks, and emit a meta-finding when a
/// single file is flagged by three or more gates.
pub fn aggregate_findings(
    mut findings: Vec<ReviewFinding>,
    diff_files: &[DiffFile],
    min_severity: Severity,
    max_findings: usize,
) -> Vec<ReviewFinding> {
    // Dedup by finding_id.
    let mut seen: HashSet<String> = HashSet::new();
    findings.retain(|f| seen.insert(f.finding_id.clone()));

    // Count per-file, per-gate.
    let mut file_gates: HashMap<String, HashSet<&'static str>> = HashMap::new();
    for f in &findings {
        file_gates
            .entry(f.file_path.clone())
            .or_default()
            .insert(f.gate);
    }

    // Emit cross-gate corroboration meta-findings.
    let mut meta: Vec<ReviewFinding> = Vec::new();
    for (file, gates) in &file_gates {
        if gates.len() >= 3 {
            let gate_list = {
                let mut v: Vec<&&str> = gates.iter().collect();
                v.sort();
                v.iter().map(|g| format!("`{g}`")).collect::<Vec<_>>().join(", ")
            };
            // Take the highest severity already reported on this file and
            // escalate one step (Style → Info, Info → Warning, Warning →
            // Warning). Critical stays Critical.
            let existing = findings
                .iter()
                .filter(|f| &f.file_path == file)
                .map(|f| f.severity)
                .min()
                .unwrap_or(Severity::Info);
            let escalated = match existing {
                Severity::Critical => Severity::Critical,
                Severity::Warning => Severity::Warning,
                Severity::Info => Severity::Warning,
                Severity::Style => Severity::Info,
            };
            meta.push(
                ReviewFinding::new(
                    escalated,
                    "corroboration",
                    file.clone(),
                    format!("{} gates flagged this file", gates.len()),
                    format!(
                        "Multiple independent gates raised findings on `{file}`: {gate_list}. \
                         Treat this file as the primary review focus for this commit."
                    ),
                    "Investigate findings on this file first — agreement across gates \
                     is a strong signal the change deserves extra scrutiny."
                        .to_string(),
                )
                .with_evidence(vec![format!(
                    "gates = {}",
                    gate_list.replace('`', "")
                )]),
            );
        }
    }
    findings.extend(meta);

    // Attach diff snippets to findings that name specific lines.
    attach_diff_snippets(&mut findings, diff_files);

    // Filter by severity.
    findings.retain(|f| f.severity <= min_severity);

    // Stable sort: severity first, then gate name, then file path.
    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.gate.cmp(b.gate))
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.title.cmp(&b.title))
    });

    findings.truncate(max_findings);
    findings
}

fn attach_diff_snippets(findings: &mut [ReviewFinding], diff_files: &[DiffFile]) {
    // Index diff files by path.
    let by_path: HashMap<&str, &DiffFile> =
        diff_files.iter().map(|f| (f.path.as_str(), f)).collect();

    for f in findings.iter_mut() {
        if f.lines.is_empty() {
            continue;
        }
        let Some(df) = by_path.get(f.file_path.as_str()) else {
            continue;
        };
        let target_line = f.lines[0];
        // Find the hunk containing this line and surface ±2 lines of
        // context from its body.
        for hunk in &df.hunks {
            let hunk_end = hunk.new_start + hunk.new_count;
            if target_line < hunk.new_start || target_line >= hunk_end {
                continue;
            }
            // Walk the body, tracking new-file line numbers, and grab a
            // 5-line window centred on `target_line`.
            let mut new_ln = hunk.new_start;
            let mut window: Vec<(usize, String)> = Vec::new();
            for body in &hunk.body {
                if body.starts_with('-') {
                    // Old-side only — don't advance new_ln.
                    window.push((0, body.clone()));
                } else {
                    window.push((new_ln, body.clone()));
                    new_ln += 1;
                }
            }
            // Window ±2 around target_line.
            let mut picks: Vec<String> = Vec::new();
            for (ln, body) in &window {
                if *ln > 0 && (*ln as isize - target_line as isize).abs() <= 2 {
                    picks.push(format!("  {ln:>4} {body}"));
                } else if *ln == 0 {
                    // removed line — include only if sandwiched inside
                    // the window
                    if let Some(last) = picks.last() {
                        let last_ln: Option<usize> =
                            last[..8].trim().parse().ok();
                        if let Some(l) = last_ln
                            && (l as isize - target_line as isize).abs() <= 2
                        {
                            picks.push(format!("       {body}"));
                        }
                    }
                }
            }
            if !picks.is_empty() {
                f.diff_snippet = Some(picks.join("\n"));
            }
            break;
        }
    }
}

// ─── Rendering ──────────────────────────────────────────────────────────────

/// Markdown payload returned from the handler when the caller did not ask
/// for JSON.
pub fn render_markdown(
    findings: &[ReviewFinding],
    files_analysed: usize,
    gates_run: usize,
    elapsed_ms: u128,
) -> String {
    let verdict = Verdict::from_findings(findings);
    let mut out = String::new();
    out.push_str(&format!(
        "# Pre-Commit Review — {emoji} **{verdict}**\n\n",
        emoji = verdict.emoji(),
        verdict = match verdict {
            Verdict::Green => "GREEN — safe to commit",
            Verdict::Yellow => "YELLOW — review recommended",
            Verdict::Red => "RED — do not merge as-is",
        },
    ));
    let mut counts: BTreeMap<Severity, usize> = BTreeMap::new();
    for f in findings {
        *counts.entry(f.severity).or_insert(0) += 1;
    }
    let total = findings.len();
    out.push_str(&format!(
        "**Findings**: {total} total ({crit} critical · {warn} warning · {info} info · {style} style) \
         | **Files analysed**: {files_analysed} | **Gates run**: {gates_run}/10 | **Time**: {elapsed_ms}ms\n\n",
        crit = counts.get(&Severity::Critical).copied().unwrap_or(0),
        warn = counts.get(&Severity::Warning).copied().unwrap_or(0),
        info = counts.get(&Severity::Info).copied().unwrap_or(0),
        style = counts.get(&Severity::Style).copied().unwrap_or(0),
    ));

    if total == 0 {
        out.push_str(
            "_No findings — diff passed all gates cleanly. Verify manually before merging._\n",
        );
        return out;
    }

    // Group findings by severity.
    let mut by_sev: BTreeMap<Severity, Vec<&ReviewFinding>> = BTreeMap::new();
    for f in findings {
        by_sev.entry(f.severity).or_default().push(f);
    }
    for (sev, items) in by_sev {
        out.push_str(&format!(
            "\n## {emoji} {label} ({count})\n\n",
            emoji = sev.emoji(),
            label = sev.as_str().to_uppercase(),
            count = items.len()
        ));
        for f in items {
            out.push_str(&format!("### {}\n\n", f.title));
            out.push_str(&format!("**File**: `{}`", f.file_path));
            if !f.lines.is_empty() {
                let ls = f
                    .lines
                    .iter()
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!(" **Lines**: {ls}"));
            }
            out.push('\n');
            out.push_str(&format!("**Gate**: `{}`\n", f.gate));
            out.push_str(&format!(
                "**ID**: `{}` — CI can track this finding across runs\n",
                f.finding_id
            ));
            if !f.evidence.is_empty() {
                out.push_str("**Evidence**:\n");
                for e in &f.evidence {
                    out.push_str(&format!("- {e}\n"));
                }
            }
            if !f.detail.is_empty() {
                out.push_str(&format!("\n{}\n", f.detail));
            }
            out.push_str(&format!("\n**Fix**: {}\n", f.suggestion));
            if let Some(tool) = &f.next_tool {
                out.push_str(&format!("**Next**: `{tool}`\n"));
            }
            if let Some(snip) = &f.diff_snippet {
                out.push_str("\n```diff\n");
                out.push_str(snip);
                if !snip.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n");
            }
            out.push_str("\n---\n");
        }
    }

    out.push_str(&format!(
        "\n_Review generated by engram `pre_commit_review` — no LLM calls. {elapsed_ms}ms._\n"
    ));
    out
}

/// JSON payload — same data, machine-readable.
#[derive(Debug, Serialize)]
pub struct ReviewJson {
    pub verdict: Verdict,
    pub summary: ReviewSummary,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Debug, Serialize)]
pub struct ReviewSummary {
    pub total_findings: usize,
    pub critical: usize,
    pub warning: usize,
    pub info: usize,
    pub style: usize,
    pub files_analysed: usize,
    pub gates_run: usize,
    pub elapsed_ms: u128,
}

pub fn render_json(
    findings: Vec<ReviewFinding>,
    files_analysed: usize,
    gates_run: usize,
    elapsed_ms: u128,
) -> ReviewJson {
    let verdict = Verdict::from_findings(&findings);
    let mut s = ReviewSummary {
        total_findings: findings.len(),
        critical: 0,
        warning: 0,
        info: 0,
        style: 0,
        files_analysed,
        gates_run,
        elapsed_ms,
    };
    for f in &findings {
        match f.severity {
            Severity::Critical => s.critical += 1,
            Severity::Warning => s.warning += 1,
            Severity::Info => s.info += 1,
            Severity::Style => s.style += 1,
        }
    }
    ReviewJson {
        verdict,
        summary: s,
        findings,
    }
}

// ─── Gate implementations ───────────────────────────────────────────────────

pub mod gates;

// ─── Service entrypoint ─────────────────────────────────────────────────────

/// Configuration controlling which gates run and how findings are filtered.
#[derive(Debug, Clone)]
pub struct ReviewConfig {
    pub max_findings: usize,
    pub min_severity: Severity,
    pub skip_gates: HashSet<String>,
    pub output_json: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            max_findings: 30,
            min_severity: Severity::Style,
            skip_gates: HashSet::new(),
            output_json: false,
        }
    }
}

/// Run the full review. Returns the findings + the number of gates that
/// actually executed (for telemetry).
///
/// Performance characteristics:
/// - Pre-computes shared data (repo rules, file-by-parent index, audit
///   function) ONCE — downstream gates reuse these instead of each
///   running their own graph queries.
/// - Sync gates run concurrently via `spawn_blocking` + `futures::join_all`,
///   so wall-clock time ≈ slowest single gate, not the sum.
/// - The async antipattern gate runs on the current runtime in parallel
///   with the sync bucket, joining at the end.
pub async fn run_pre_commit_review(
    state: &AppState,
    project_id: &str,
    project_dir: &Path,
    generation: u64,
    diff_text: &str,
    config: &ReviewConfig,
) -> anyhow::Result<(Vec<ReviewFinding>, usize, usize)> {
    let diff_files = parse_unified_diff(diff_text);
    if diff_files.is_empty() {
        return Ok((Vec::new(), 0, 0));
    }

    // ── Pre-computed shared data (loaded once) ────────────────────────
    //
    // `HashSet::with_capacity` avoids rehashing on projects with
    // many changed files. `diff_files.len()` is an exact upper bound.
    let changed_paths: Arc<HashSet<String>> = {
        let mut s = HashSet::with_capacity(diff_files.len());
        for f in &diff_files {
            s.insert(f.path.clone());
        }
        Arc::new(s)
    };
    let total_commits = state
        .registry
        .get_meta(project_id, "total_commits")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or_else(|| count_commits_best_effort(project_dir).unwrap_or(0));
    let repo_rules: Arc<Vec<RepoRule>> = Arc::new(
        state
            .registry
            .list_repo_rules(project_id)
            .unwrap_or_default(),
    );
    let files_by_parent: Arc<HashMap<String, Vec<String>>> =
        Arc::new(build_files_by_parent(&state.graph, project_id));
    let audit_function = detect_audit_function(&state.graph, project_id);

    // Context shared across all gates. Wrapped in Arc so we can clone
    // cheaply into spawn_blocking closures.
    let shared = Arc::new(SharedGateData {
        graph: state.graph.clone(),
        registry: state.registry.clone(),
        project_id: project_id.to_string(),
        project_dir: project_dir.to_path_buf(),
        generation,
        diff_files: Arc::new(diff_files.clone()),
        changed_paths: changed_paths.clone(),
        total_commits,
        repo_rules: repo_rules.clone(),
        files_by_parent: files_by_parent.clone(),
        audit_function: audit_function.clone(),
    });

    // ── Gate dispatch ─────────────────────────────────────────────────
    //
    // Sync gates are CPU / Redb-I/O bound and don't share mutable state
    // — perfect for `spawn_blocking`. They execute concurrently.
    //
    // The async antipattern gate runs on the runtime in parallel with
    // the sync bucket.
    let mut sync_handles: Vec<(
        &'static str,
        tokio::task::JoinHandle<anyhow::Result<Vec<ReviewFinding>>>,
    )> = Vec::new();
    let mut async_gate: Option<Box<dyn Gate>> = None;
    let mut gates_run = 0usize;

    for gate in gates::all_gates() {
        let name = gate.name();
        if config.skip_gates.iter().any(|s| s.as_str() == name) {
            continue;
        }
        gates_run += 1;

        // The antipattern gate is the only one that must run async (it
        // performs hybrid search). Everything else is sync — hand it to
        // spawn_blocking for true parallelism.
        if name == "antipattern" {
            async_gate = Some(gate);
            continue;
        }
        let shared = shared.clone();
        let state_clone = state.clone(); // AppState is Clone (all Arc fields)
        let handle = tokio::task::spawn_blocking(move || {
            let ctx = shared.as_borrowed(&state_clone);
            gate.run(&ctx)
        });
        sync_handles.push((name, handle));
    }

    let sync_future = async move {
        let mut out: Vec<ReviewFinding> = Vec::new();
        for (name, h) in sync_handles {
            match h.await {
                Ok(Ok(fs)) => out.extend(fs),
                Ok(Err(e)) => {
                    tracing::warn!(gate = %name, "pre_commit_review gate failed: {e}");
                }
                Err(e) => {
                    tracing::warn!(gate = %name, "pre_commit_review gate panicked: {e}");
                }
            }
        }
        out
    };

    let async_future = async {
        let Some(gate) = async_gate else {
            return Vec::new();
        };
        let ctx = shared.as_borrowed(state);
        match gate.run_async(&ctx).await {
            Ok(fs) => fs,
            Err(e) => {
                tracing::warn!(gate = gate.name(), "pre_commit_review gate failed: {e}");
                Vec::new()
            }
        }
    };

    let (sync_findings, async_findings) = tokio::join!(sync_future, async_future);
    let mut findings = sync_findings;
    findings.extend(async_findings);

    let finalised = aggregate_findings(
        findings,
        &diff_files,
        config.min_severity,
        config.max_findings,
    );
    Ok((finalised, gates_run, diff_files.len()))
}

/// Owned snapshot of everything a gate might need. Cheap to clone
/// (every field is either `Arc`'d or a small value) — we clone one per
/// sync gate so each can run in its own `spawn_blocking` task.
///
/// The separate `as_borrowed(state)` step builds a `GateContext<'_>`
/// that the gate actually consumes; we keep the borrowed-context shape
/// from the original API so gate implementations stay unchanged.
#[derive(Clone)]
struct SharedGateData {
    graph: Arc<GraphStore>,
    registry: Arc<Registry>,
    project_id: String,
    project_dir: std::path::PathBuf,
    generation: u64,
    diff_files: Arc<Vec<DiffFile>>,
    changed_paths: Arc<HashSet<String>>,
    total_commits: u32,
    repo_rules: Arc<Vec<RepoRule>>,
    files_by_parent: Arc<HashMap<String, Vec<String>>>,
    audit_function: Option<String>,
}

impl SharedGateData {
    fn as_borrowed<'a>(&'a self, state: &'a AppState) -> GateContext<'a> {
        GateContext {
            state,
            graph: self.graph.clone(),
            registry: self.registry.clone(),
            project_id: &self.project_id,
            project_dir: self.project_dir.as_path(),
            generation: self.generation,
            diff_files: self.diff_files.as_slice(),
            changed_paths: &self.changed_paths,
            total_commits: self.total_commits,
            repo_rules: self.repo_rules.clone(),
            files_by_parent: self.files_by_parent.clone(),
            audit_function: self.audit_function.clone(),
        }
    }
}

// ─── Pre-computed data builders ─────────────────────────────────────────────

/// Build a `parent_dir → [file_path]` index for every file node in the
/// project. Called once per review so Gate 8 (new-file convention)
/// doesn't hit the graph on every added file.
fn build_files_by_parent(
    graph: &GraphStore,
    project_id: &str,
) -> HashMap<String, Vec<String>> {
    let nodes = graph
        .query_nodes(project_id, Some("file"), None, None, 50_000)
        .unwrap_or_default();
    let mut by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for n in nodes {
        let p = n.file_path.as_str().to_string();
        let parent = match p.rfind('/') {
            Some(i) => p[..i].to_string(),
            None => String::new(),
        };
        by_parent.entry(parent).or_default().push(p);
    }
    by_parent
}

/// Detect the project's audit-log convention by name. Searches for
/// function nodes whose names contain common audit identifiers.
/// Returns the most-specific name (longest match) or `None` when no
/// convention exists.
fn detect_audit_function(graph: &GraphStore, project_id: &str) -> Option<String> {
    const AUDIT_PATTERNS: &[&str] = &[
        "handelselogg",
        "AuditLog",
        "audit_log",
        "LogActivity",
        "AuditTrail",
    ];
    for pat in AUDIT_PATTERNS {
        let matches = graph
            .query_nodes(project_id, Some("function"), Some(pat), None, 10)
            .unwrap_or_default();
        if !matches.is_empty() {
            return matches
                .iter()
                .max_by_key(|n| n.name.len())
                .map(|n| n.name.clone());
        }
    }
    None
}

fn count_commits_best_effort(project_dir: &Path) -> anyhow::Result<u32> {
    let repo = git2::Repository::discover(project_dir)?;
    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    Ok(revwalk.take(10_000).count() as u32)
}

// ─── Helpers used by gates ──────────────────────────────────────────────────

/// Return the node id for a file path, using the `file:…` prefix the
/// graph uses.
pub fn file_node_id(file_path: &str) -> String {
    format!("file:{}", file_path.replace('\\', "/"))
}

/// Look up a file's full content from disk, relative to the project dir.
/// Returns None if the read fails (e.g. deleted / not on disk yet).
pub fn read_file_content(project_dir: &Path, rel_path: &str) -> Option<String> {
    let p = project_dir.join(rel_path);
    std::fs::read_to_string(p).ok()
}

// Re-export commonly-used items for the gates module.
pub use gates::all_gates;

#[cfg(test)]
pub(crate) fn test_parse(diff: &str) -> Vec<DiffFile> {
    parse_unified_diff(diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_green_when_only_style() {
        let f = ReviewFinding::new(Severity::Style, "g", "a", "t", "d", "s");
        assert_eq!(Verdict::from_findings(&[f]), Verdict::Green);
    }

    #[test]
    fn verdict_yellow_when_warning_or_info() {
        let f = ReviewFinding::new(Severity::Warning, "g", "a", "t", "d", "s");
        assert_eq!(Verdict::from_findings(&[f]), Verdict::Yellow);
    }

    #[test]
    fn verdict_red_when_critical() {
        let a = ReviewFinding::new(Severity::Critical, "g", "a", "t", "d", "s");
        let b = ReviewFinding::new(Severity::Warning, "g", "b", "t", "d", "s");
        assert_eq!(Verdict::from_findings(&[a, b]), Verdict::Red);
    }

    #[test]
    fn parse_simple_diff_captures_added_lines() {
        let diff = "\
diff --git a/foo.rs b/foo.rs
--- a/foo.rs
+++ b/foo.rs
@@ -1,3 +1,4 @@
 fn main() {
-    let x = 1;
+    let x = 2;
+    println!(\"hi\");
 }
";
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1, "expected one file, got {files:#?}");
        assert_eq!(files[0].path, "foo.rs");
        assert_eq!(files[0].change_type, ChangeType::Modified);
        assert_eq!(files[0].added_lines.len(), 2);
        assert!(files[0].added_content.contains("let x = 2"));
        assert!(files[0].added_content.contains("println"));
        assert_eq!(files[0].removed_lines.len(), 1);
    }

    #[test]
    fn parse_new_file_marks_added() {
        let diff = "\
diff --git a/new.rs b/new.rs
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+fn hi() {}
+fn bye() {}
";
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].change_type, ChangeType::Added);
        assert_eq!(files[0].added_lines.len(), 2);
    }

    #[test]
    fn parse_deleted_file_marks_deleted() {
        let diff = "\
diff --git a/old.rs b/old.rs
--- a/old.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-fn gone() {}
";
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].change_type, ChangeType::Deleted);
    }

    #[test]
    fn stable_finding_id_is_deterministic_and_short() {
        let a = stable_finding_id("immune", "foo.rs", "bad", &[10, 11]);
        let b = stable_finding_id("immune", "foo.rs", "bad", &[10, 11]);
        let c = stable_finding_id("immune", "foo.rs", "bad", &[10, 12]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 12);
    }

    #[test]
    fn is_test_path_covers_conventions() {
        assert!(is_test_path("src/foo_test.rs"));
        assert!(is_test_path("src/test_foo.py"));
        assert!(is_test_path("app/__tests__/foo.ts"));
        assert!(is_test_path("x/ThingTests.cs"));
        assert!(is_test_path("x/Thing.test.ts"));
        assert!(is_test_path("x/Thing.spec.tsx"));
        assert!(!is_test_path("src/lib.rs"));
        assert!(!is_test_path("src/handler.ts"));
    }

    #[test]
    fn ts_interface_prefix_convention_detected() {
        let src = r#"
interface IUser { id: number; }
interface IOrder { id: number; }
interface IProduct { id: number; }
"#;
        let cs = extract_conventions(src, "types.ts");
        assert!(cs
            .iter()
            .any(|c| c.category == ConventionCategory::InterfacePrefix
                && c.value == "I-prefix"));
    }

    #[test]
    fn aggregate_emits_corroboration_when_3_gates_hit_same_file() {
        let f1 = ReviewFinding::new(Severity::Info, "a", "foo.rs", "x1", "", "fix");
        let f2 = ReviewFinding::new(Severity::Info, "b", "foo.rs", "x2", "", "fix");
        let f3 = ReviewFinding::new(Severity::Info, "c", "foo.rs", "x3", "", "fix");
        let finalised = aggregate_findings(vec![f1, f2, f3], &[], Severity::Style, 100);
        assert!(finalised.iter().any(|f| f.gate == "corroboration"));
    }
}
