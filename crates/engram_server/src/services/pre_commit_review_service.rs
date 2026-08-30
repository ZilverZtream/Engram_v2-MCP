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

/// What one gate did during a review (row-3 audit A1). A gate that errored,
/// panicked or was skipped is MISSING EVIDENCE, not a clean pass.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum GateStatus {
    Passed,
    Findings(usize),
    Failed(String),
    Panicked(String),
    Skipped(String),
    /// The gate ran and returned, but a provider it depends on failed
    /// inside it (file unreadable, graph/search error, runtime missing ⇒
    /// regex-only fallback). Its findings are real; its silence is not.
    Degraded {
        findings: usize,
        notes: Vec<String>,
    },
}

impl GateStatus {
    /// Status from a gate's findings and the provider-failure notes it
    /// recorded on its context while running.
    pub fn from_run(findings: usize, notes: Vec<String>) -> Self {
        if !notes.is_empty() {
            Self::Degraded { findings, notes }
        } else if findings == 0 {
            Self::Passed
        } else {
            Self::Findings(findings)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GateOutcome {
    pub name: &'static str,
    pub status: GateStatus,
    pub elapsed_ms: u128,
    /// Internal caps the gate hit while running (row-3 A4): "looked at 20
    /// of 25 files (FILE_CAP)". A clean gate that stopped looking says so.
    #[serde(default)]
    pub caps: Vec<String>,
}

impl GateOutcome {
    pub fn did_not_run(&self) -> bool {
        matches!(
            self.status,
            GateStatus::Failed(_) | GateStatus::Panicked(_) | GateStatus::Skipped(_)
        )
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self.status, GateStatus::Degraded { .. })
    }
}

impl Verdict {
    /// Verdict from the findings AND the gate outcomes: evidence a gate did
    /// not deliver cannot make the diff green (row-3 audit A2).
    pub fn with_outcomes(findings: &[ReviewFinding], outcomes: &[GateOutcome]) -> Self {
        let base = Self::from_findings(findings);
        let missing = outcomes.iter().any(|o| {
            matches!(
                o.status,
                GateStatus::Failed(_) | GateStatus::Panicked(_) | GateStatus::Degraded { .. }
            )
        });
        if base == Self::Green && missing {
            Self::Yellow
        } else {
            base
        }
    }

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
    if fname.starts_with("test_")
        || fname.ends_with("_test.rs")
        || fname.ends_with("_test.go")
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
    if fname.ends_with("tests.cs")
        || fname.ends_with("tests.vb")
        || fname.ends_with("test.cs")
        || fname.ends_with("test.java")
        || fname.ends_with("tests.java")
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
    /// Provider failures the running gate chose to survive (row-3 A3).
    /// Drained by the runner into `GateStatus::Degraded`.
    /// External audit 2026-08-29 P0-4: when the published search generation is
    /// incomplete, every search-backed gate degrades itself with this note
    /// instead of passing against a fraction of the corpus.
    pub search_index_note: Option<String>,
    pub degraded: std::sync::Mutex<Vec<String>>,
    /// Internal caps the running gate hit (row-3 A4). Drained by the
    /// runner into `GateOutcome::caps`.
    pub caps: std::sync::Mutex<Vec<String>>,
}

impl GateContext<'_> {
    /// Record that a provider this gate depends on failed, so the gate's
    /// (partial) result is reported as DEGRADED instead of clean.
    pub fn degrade(&self, note: impl Into<String>) {
        let note = note.into();
        if let Ok(mut v) = self.degraded.lock()
            && !v.iter().any(|n| *n == note)
        {
            v.push(note);
        }
    }

    /// Record an internal cap the gate hit ("looked at 20 of 25 files").
    pub fn note_cap(&self, note: impl Into<String>) {
        let note = note.into();
        if let Ok(mut v) = self.caps.lock()
            && !v.iter().any(|n| *n == note)
        {
            v.push(note);
        }
    }

    pub fn take_caps(&self) -> Vec<String> {
        self.caps
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }

    pub fn take_degraded(&self) -> Vec<String> {
        self.degraded
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }

    /// Read a working-tree file for a gate; an unreadable file is a
    /// provider failure (degrades the gate), never silently "empty".
    pub fn read_project_file(&self, rel: &str) -> Vec<u8> {
        match std::fs::read(self.project_dir.join(rel)) {
            Ok(b) => b,
            Err(e) => {
                self.degrade(format!("could not read {rel} from the working tree: {e}"));
                Vec::new()
            }
        }
    }
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
        let Some(hunk) = current_hunk.as_mut() else {
            continue;
        };
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
            // Legacy codebases routinely contain non-UTF-8 bytes (a single
            // cp1252/latin-1 curly-quote byte in a vendored JS bundle is
            // enough). `read_to_string` hard-fails on the FIRST such byte
            // ("stream did not contain valid UTF-8") and kills the WHOLE
            // review — read raw bytes and decode lossily (U+FFFD
            // replacement) instead. A mangled character in one hunk beats
            // no review at all.
            let bytes = std::fs::read(&p)
                .map_err(|e| anyhow::anyhow!("failed to read diff file {}: {e}", p.display()))?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
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
        // Lossy on purpose: a non-UTF-8 byte in one file's hunk must not
        // silently DROP that diff line (the old `from_utf8` + `if let Ok`
        // skipped it entirely) — decode with U+FFFD so line accounting
        // stays correct for legacy sources.
        text.push_str(&String::from_utf8_lossy(line.content()));
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
    // Infallible on a fixed-size bucket array, but make it airtight: an
    // empty iterator yields a harmless default instead of a panic.
    let (idx, count) = buckets
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| **c)
        .map(|(i, c)| (i, *c))
        .unwrap_or((0, 0));
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

    // SafeRedirect convention (pilot-style — generic test: "project uses a
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
    static AS_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\bas\s+(?:HTML\w+|SVG\w+|[A-Z]\w*(?:\s*\[\s*\])?)\b").ok());
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
        let (winner, count) = if dbl > sng {
            ("double", dbl)
        } else {
            ("single", sng)
        };
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
    let upper = [
        "SELECT ", "FROM ", "INSERT ", "UPDATE ", "DELETE ", "WHERE ", "JOIN ",
    ]
    .iter()
    .map(|k| content.matches(k).count())
    .sum::<usize>();
    let lower = [
        "select ", "from ", "insert ", "update ", "delete ", "where ", "join ",
    ]
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

// ─── Generated-file exemption ───────────────────────────────────────────────
//
// A `.designer.vb` / `.Designer.cs` partial-class file (or anything else
// carrying an `<auto-generated>`-style header) is machine output — its
// indentation and naming come from a code generator, not a developer, so
// style/naming/indentation gates flagging it is pure noise. One real
// pre-commit review saw 71 of its 200 findings (56 duplicate indentation
// flags + 15 naming flags) fire on a SINGLE generated designer file. The
// fix is generic detection (no per-project file lists): a filename
// pattern OR a generated-code header marker in the first ~20 lines.
//
// Detection is deliberately conservative about WHAT it exempts: only the
// Style-class findings a generated file would otherwise drown gates in.
// Any Warning/Critical finding from the same check (e.g. "On Error Resume
// Next reintroduced") still fires — a real bug doesn't stop being a bug
// because it lives in generated code.

/// Filename suffixes (checked against the basename, case-insensitively)
/// that mark a file as machine-generated by a known code-generation
/// convention.
const GENERATED_FILENAME_SUFFIXES: &[&str] = &[".designer.vb", ".designer.cs", ".g.cs", ".g.i.cs"];

/// Returns true when `path`'s filename matches a known generated-code
/// naming pattern: `*.designer.vb`, `*.Designer.cs`, `*.g.cs`, or
/// `*.generated.*` (any extension after a literal `.generated.` segment).
pub fn is_generated_filename(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let fname = lower.rsplit('/').next().unwrap_or(&lower);
    GENERATED_FILENAME_SUFFIXES
        .iter()
        .any(|s| fname.ends_with(s))
        || fname.contains(".generated.")
}

/// Case-insensitive markers that appear in a generated-code header
/// comment, scoped to the first ~20 lines of a file — a marker deep
/// inside an otherwise hand-written file (e.g. quoted in a docstring)
/// shouldn't count.
const GENERATED_HEADER_MARKERS: &[&str] =
    &["<auto-generated", "this code was generated", "do not edit"];

/// Returns true when `content`'s first ~20 lines contain a generated-code
/// header marker.
pub fn has_generated_header(content: &str) -> bool {
    let head = content
        .lines()
        .take(20)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    GENERATED_HEADER_MARKERS.iter().any(|m| head.contains(m))
}

/// Apply the generated-file exemption to a set of would-be Style-class
/// findings for one file.
///
/// - Non-generated files: `would_be` passes through untouched.
/// - Generated file with no would-be findings: nothing to report, stays
///   silent (an exemption notice for a clean file is just more noise).
/// - Generated file WITH would-be findings: collapse to a single `Info`
///   finding stating how many were suppressed, so the signal ("this file
///   had style deviations") survives without the per-line spam.
pub fn apply_generated_exemption(
    gate: &'static str,
    file_path: &str,
    is_generated: bool,
    would_be: Vec<ReviewFinding>,
) -> Vec<ReviewFinding> {
    if !is_generated || would_be.is_empty() {
        return would_be;
    }
    vec![ReviewFinding::new(
        Severity::Info,
        gate,
        file_path.to_string(),
        "Generated file — style checks skipped",
        format!(
            "`{file_path}` looks machine-generated ({} would-be style/naming finding(s) \
             suppressed). Convention checks don't apply to generated code — indentation and \
             naming come from the generator, not a developer.",
            would_be.len()
        ),
        "If this file is actually hand-maintained, rename it away from the generated \
         naming pattern (e.g. `*.designer.vb`) or remove the generated-code header comment \
         so convention checks apply to it."
            .to_string(),
    )]
}

// ─── Aggregation ────────────────────────────────────────────────────────────

/// Finalise the finding list: dedup by `finding_id`, collapse resx-family
/// and per-file style spam, cap each gate's share of the budget, sort,
/// attach diff snippets from the originating hunks, and emit a
/// meta-finding when a single file is flagged by three or more gates.
pub fn aggregate_findings(
    mut findings: Vec<ReviewFinding>,
    diff_files: &[DiffFile],
    min_severity: Severity,
    max_findings: usize,
) -> Vec<ReviewFinding> {
    // Dedup by finding_id.
    let mut seen: HashSet<String> = HashSet::new();
    findings.retain(|f| seen.insert(f.finding_id.clone()));

    // Finding-budget hygiene, in order: collapse resx-language-variant
    // spam, then collapse same-kind style spam per file, then cap each
    // gate's share of the overall budget so one noisy gate can't starve
    // the rest. All three run BEFORE corroboration/sort/truncate so those
    // later stages see the already-hygienic set.
    findings = collapse_resx_family_findings(findings);
    findings = collapse_style_findings(findings);
    findings = apply_per_gate_budget(findings, max_findings);

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
                v.iter()
                    .map(|g| format!("`{g}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
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
                .with_evidence(vec![format!("gates = {}", gate_list.replace('`', ""))]),
            );
        }
    }
    findings.extend(meta);

    // Attach diff snippets to findings that name specific lines.
    attach_diff_snippets(&mut findings, diff_files);

    // Filter by severity.
    findings.retain(|f| f.severity <= min_severity);

    // Stable sort: severity first (Critical > Warning > Info > Style, so
    // truncation below drops Style findings first), then primaries before
    // corroboration meta-findings within the same severity tier (a meta
    // finding must never displace the primary findings it references),
    // then gate name, then file path.
    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| (a.gate == "corroboration").cmp(&(b.gate == "corroboration")))
            .then_with(|| a.gate.cmp(b.gate))
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.title.cmp(&b.title))
    });

    findings.truncate(max_findings);
    findings
}

// ─── Finding-budget hygiene ─────────────────────────────────────────────────

/// Directory + first-dot stem of a `.resx` path — the same "family" key
/// `handle_get_change_set` uses to group localized resource siblings
/// (`label.resx`, `label.en.resx`, `label.de.resx`, … all share
/// `stem = "label"`). Returns `None` for non-`.resx` paths or a `.resx`
/// with an empty stem.
pub(crate) fn resx_dir_stem(path: &str) -> Option<(String, String)> {
    let p = path.replace('\\', "/");
    if !p.to_ascii_lowercase().ends_with(".resx") {
        return None;
    }
    let (dir, fname) = match p.rfind('/') {
        Some(i) => (p[..i].to_string(), p[i + 1..].to_string()),
        None => (String::new(), p.clone()),
    };
    let stem = fname.split('.').next().unwrap_or("").to_string();
    if stem.is_empty() {
        None
    } else {
        Some((dir, stem))
    }
}

pub(crate) fn resx_family_display(dir: &str, stem: &str) -> String {
    if dir.is_empty() {
        format!("{stem}.*.resx")
    } else {
        format!("{dir}/{stem}.*.resx")
    }
}

/// Collapse findings that differ only by localized `.resx` language
/// variant into one finding naming the family + member count. Lightweight
/// reimplementation of `handle_get_change_set`'s resx-family grouping,
/// scoped to review findings instead of change-set file lists.
///
/// Two shapes are recognised:
/// 1. The finding's own `file_path` IS a `.resx` file (e.g. a per-file
///    naming/extension finding that fires once per language sibling) —
///    grouped by (gate, severity, title, dir, stem).
/// 2. The finding's `file_path` is constant but its title references a
///    `.resx` file via a backtick-quoted span (e.g. "Coupled file `X` not
///    in diff" firing once per co-changed language sibling) — grouped by
///    (gate, severity, host file, title-with-reference-masked, dir, stem).
///
/// A group only collapses when it has 2+ members; a lone resx finding is
/// left untouched (nothing to collapse).
fn collapse_resx_family_findings(findings: Vec<ReviewFinding>) -> Vec<ReviewFinding> {
    use regex::Regex;
    use std::sync::LazyLock;
    static RESX_REF_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"`([^`]+?\.[Rr][Ee][Ss][Xx])`").expect("valid regex"));

    #[derive(Hash, PartialEq, Eq, Clone)]
    struct GroupKey {
        gate: &'static str,
        severity: Severity,
        host: String,
        title_template: String,
        dir: String,
        stem: String,
    }

    let mut groups: HashMap<GroupKey, Vec<usize>> = HashMap::new();
    for (i, f) in findings.iter().enumerate() {
        let key = if let Some((dir, stem)) = resx_dir_stem(&f.file_path) {
            GroupKey {
                gate: f.gate,
                severity: f.severity,
                host: String::new(),
                title_template: f.title.clone(),
                dir,
                stem,
            }
        } else if let Some(cap) = RESX_REF_RE.captures(&f.title) {
            let referenced = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
            let Some((dir, stem)) = resx_dir_stem(referenced) else {
                continue;
            };
            let full_match = cap.get(0).map(|m| m.as_str()).unwrap_or_default();
            let title_template = f.title.replacen(full_match, "\u{0}", 1);
            GroupKey {
                gate: f.gate,
                severity: f.severity,
                host: f.file_path.clone(),
                title_template,
                dir,
                stem,
            }
        } else {
            continue;
        };
        groups.entry(key).or_default().push(i);
    }
    groups.retain(|_, idxs| idxs.len() >= 2);
    if groups.is_empty() {
        return findings;
    }

    let idx_to_key: HashMap<usize, GroupKey> = groups
        .iter()
        .flat_map(|(k, idxs)| idxs.iter().map(move |&i| (i, k.clone())))
        .collect();
    let to_collapse: HashSet<usize> = idx_to_key.keys().copied().collect();

    let mut out = Vec::with_capacity(findings.len());
    let mut emitted: HashSet<GroupKey> = HashSet::new();
    for (i, f) in findings.into_iter().enumerate() {
        let Some(key) = idx_to_key.get(&i) else {
            out.push(f);
            continue;
        };
        if !to_collapse.contains(&i) || !emitted.insert(key.clone()) {
            continue;
        }
        let count = groups[key].len();
        let family_display = resx_family_display(&key.dir, &key.stem);
        let title = if key.host.is_empty() {
            format!(
                "{} — {count} localized .resx variants ({family_display})",
                key.title_template
            )
        } else {
            format!(
                "{} ({count} localized variants)",
                key.title_template
                    .replace('\u{0}', &format!("`{family_display}`"))
            )
        };
        let file_path = if key.host.is_empty() {
            family_display.clone()
        } else {
            key.host.clone()
        };
        let detail = format!(
            "{count} findings differed only by localized `.resx` language variant of \
             `{family_display}` and were collapsed into this one. Treat the family as \
             atomic — the language variants all need the same decision."
        );
        let suggestion = f.suggestion.clone();
        let next_tool = f.next_tool.clone();
        let mut collapsed =
            ReviewFinding::new(f.severity, f.gate, file_path, title, detail, suggestion);
        collapsed.evidence = vec![
            format!("resx_family = {family_display}"),
            format!("collapsed_count = {count}"),
        ];
        if let Some(t) = next_tool {
            collapsed = collapsed.with_next_tool(t);
        }
        out.push(collapsed);
    }
    out
}

/// Collapse repeated findings that share the exact same (gate, file,
/// title) and differ only by line number, into one finding carrying an
/// explicit occurrence count. Scoped to `Severity::Style` — that's the
/// noisy class (bulk indentation / naming spam repeated once per line);
/// Warning/Info findings are rare enough per file that collapsing them
/// would hide detail that actually matters.
fn collapse_style_findings(findings: Vec<ReviewFinding>) -> Vec<ReviewFinding> {
    #[derive(Hash, PartialEq, Eq, Clone)]
    struct Key {
        gate: &'static str,
        file: String,
        title: String,
    }

    let mut groups: HashMap<Key, Vec<usize>> = HashMap::new();
    for (i, f) in findings.iter().enumerate() {
        if f.severity != Severity::Style {
            continue;
        }
        groups
            .entry(Key {
                gate: f.gate,
                file: f.file_path.clone(),
                title: f.title.clone(),
            })
            .or_default()
            .push(i);
    }
    groups.retain(|_, idxs| idxs.len() >= 2);
    if groups.is_empty() {
        return findings;
    }

    // For each group, precompute the sorted line list + which member is
    // the representative (lowest line number, falling back to lowest
    // index when no finding in the group names a line).
    struct Summary {
        rep_idx: usize,
        count: usize,
        lines_preview: String,
        first_line: Option<usize>,
    }
    let mut summaries: HashMap<Key, Summary> = HashMap::new();
    for (key, idxs) in &groups {
        let mut lines: Vec<(usize, usize)> = idxs
            .iter()
            .filter_map(|&j| findings[j].lines.first().map(|l| (*l, j)))
            .collect();
        lines.sort_unstable();
        let rep_idx = lines.first().map(|(_, j)| *j).unwrap_or(idxs[0]);
        let first_line = lines.first().map(|(l, _)| *l);
        let preview = if lines.len() > 10 {
            let head = lines[..10]
                .iter()
                .map(|(l, _)| l.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{head}, … (+{} more)", lines.len() - 10)
        } else {
            lines
                .iter()
                .map(|(l, _)| l.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        summaries.insert(
            key.clone(),
            Summary {
                rep_idx,
                count: idxs.len(),
                lines_preview: preview,
                first_line,
            },
        );
    }

    let idx_to_key: HashMap<usize, Key> = groups
        .iter()
        .flat_map(|(k, idxs)| idxs.iter().map(move |&i| (i, k.clone())))
        .collect();

    let mut out = Vec::with_capacity(findings.len());
    for (i, f) in findings.into_iter().enumerate() {
        let Some(key) = idx_to_key.get(&i) else {
            out.push(f);
            continue;
        };
        let summary = &summaries[key];
        if i != summary.rep_idx {
            continue; // folds into the collapsed finding built at rep_idx
        }
        let title = match summary.first_line {
            Some(l) => format!(
                "{} ×{} in {} (first at L{l})",
                f.title, summary.count, f.file_path
            ),
            None => format!("{} ×{} in {}", f.title, summary.count, f.file_path),
        };
        let detail = format!(
            "{}\n\nThis exact style finding recurred {} times in `{}` — lines: {}.",
            f.detail, summary.count, f.file_path, summary.lines_preview
        );
        let mut collapsed = ReviewFinding::new(
            f.severity,
            f.gate,
            f.file_path.clone(),
            title,
            detail,
            f.suggestion.clone(),
        );
        if let Some(l) = summary.first_line {
            collapsed = collapsed.with_lines(vec![l]);
        }
        collapsed.evidence = vec![
            format!("collapsed_count = {}", summary.count),
            format!("lines = {}", summary.lines_preview),
        ];
        out.push(collapsed);
    }
    out
}

/// Cap each gate's share of `max_findings` so a single noisy gate can't
/// crowd out every other gate's findings before the final truncation.
/// Budget is split across the gates that actually produced findings
/// (not the full configured gate list — an idle gate shouldn't shrink
/// everyone else's share), with a floor of 10 so a small `max_findings`
/// doesn't starve every gate down to nothing.
///
/// Only applies when the total actually exceeds `max_findings` — under
/// budget nothing is being crowded out, and capping anyway silently
/// deletes real findings (a gate with 39 findings would lose 6 to a
/// 200-budget cap while the whole run emitted 75).
///
/// Within a gate that exceeds its share, findings are kept by severity
/// first (a gate's own Warning/Info findings survive over its own Style
/// spam) and then by file/title for determinism.
fn apply_per_gate_budget(findings: Vec<ReviewFinding>, max_findings: usize) -> Vec<ReviewFinding> {
    if findings.len() <= max_findings {
        return findings;
    }
    let num_gates = findings
        .iter()
        .map(|f| f.gate)
        .collect::<HashSet<_>>()
        .len()
        .max(1);
    let per_gate_cap = (max_findings / num_gates).max(10);

    let mut by_gate: HashMap<&'static str, Vec<ReviewFinding>> = HashMap::new();
    for f in findings {
        by_gate.entry(f.gate).or_default().push(f);
    }

    let mut out = Vec::new();
    for (_, mut items) in by_gate {
        if items.len() > per_gate_cap {
            items.sort_by(|a, b| {
                a.severity
                    .cmp(&b.severity)
                    .then_with(|| a.file_path.cmp(&b.file_path))
                    .then_with(|| a.title.cmp(&b.title))
            });
            items.truncate(per_gate_cap);
        }
        out.extend(items);
    }
    out
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
                        let last_ln: Option<usize> = last[..8].trim().parse().ok();
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
    outcomes: &[GateOutcome],
) -> String {
    let verdict = Verdict::with_outcomes(findings, outcomes);
    let not_run: Vec<&GateOutcome> = outcomes.iter().filter(|o| o.did_not_run()).collect();
    let degraded: Vec<&GateOutcome> = outcomes.iter().filter(|o| o.is_degraded()).collect();
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
         | **Files analysed**: {files_analysed} | **Gates run**: {gates_run}/{gates_total}{not_run_note} | **Time**: {elapsed_ms}ms\n\n",
        gates_total = gates::all_gates().len(),
        not_run_note = match (not_run.len(), degraded.len()) {
            (0, 0) => String::new(),
            (n, 0) => format!(" ({n} did not run)"),
            (0, d) => format!(" ({d} DEGRADED)"),
            (n, d) => format!(" ({n} did not run, {d} DEGRADED)"),
        },
        crit = counts.get(&Severity::Critical).copied().unwrap_or(0),
        warn = counts.get(&Severity::Warning).copied().unwrap_or(0),
        info = counts.get(&Severity::Info).copied().unwrap_or(0),
        style = counts.get(&Severity::Style).copied().unwrap_or(0),
    ));

    if !not_run.is_empty() {
        out.push_str("## ⚠ Gates that did not run — evidence is INCOMPLETE\n\n");
        for o in &not_run {
            let what = match &o.status {
                GateStatus::Failed(r) => format!("FAILED — {r}"),
                GateStatus::Panicked(r) => format!("PANICKED — {r}"),
                GateStatus::Skipped(r) => format!("skipped — {r}"),
                _ => String::new(),
            };
            out.push_str(&format!("- `{}`: {what}\n", o.name));
        }
        out.push('\n');
    }

    let capped: Vec<&GateOutcome> = outcomes.iter().filter(|o| !o.caps.is_empty()).collect();
    if !capped.is_empty() {
        out.push_str("## Caps hit — these gates stopped looking at a limit\n\n");
        for o in &capped {
            for c in &o.caps {
                out.push_str(&format!("- `{}`: {c}\n", o.name));
            }
        }
        out.push('\n');
    }

    if !degraded.is_empty() {
        out.push_str(
            "## ⚠ Gates that ran DEGRADED — a provider failed inside them, evidence is PARTIAL\n\n",
        );
        for o in &degraded {
            if let GateStatus::Degraded { findings, notes } = &o.status {
                out.push_str(&format!(
                    "- `{}` ({findings} finding(s) from what it could see):\n",
                    o.name
                ));
                for n in notes {
                    out.push_str(&format!("  - {n}\n"));
                }
            }
        }
        out.push('\n');
    }

    if total == 0 {
        if not_run.is_empty() && degraded.is_empty() {
            out.push_str(
                "_No findings — diff passed all gates cleanly. Verify manually before merging._\n",
            );
        } else {
            out.push_str(&format!(
                "_No findings from the {} gate(s) that ran fully; {} did not run, {} ran degraded (above) — this is NOT a clean bill._\n",
                gates_run.saturating_sub(not_run.len() + degraded.len()),
                not_run.len(),
                degraded.len()
            ));
        }
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
    /// Per-gate outcome (row-3 audit A1): passed / findings / failed /
    /// panicked / skipped, with elapsed time.
    pub gate_status: Vec<GateOutcome>,
}

#[derive(Debug, Serialize)]
pub struct ReviewSummary {
    pub total_findings: usize,
    pub critical: usize,
    pub warning: usize,
    pub info: usize,
    pub style: usize,
    pub files_analysed: usize,
    /// Gates dispatched (skips excluded).
    pub gates_run: usize,
    pub gates_failed: usize,
    pub gates_panicked: usize,
    pub gates_skipped: usize,
    pub gates_degraded: usize,
    pub elapsed_ms: u128,
}

pub fn render_json(
    findings: Vec<ReviewFinding>,
    files_analysed: usize,
    gates_run: usize,
    elapsed_ms: u128,
    outcomes: &[GateOutcome],
) -> ReviewJson {
    let verdict = Verdict::with_outcomes(&findings, outcomes);
    let mut s = ReviewSummary {
        total_findings: findings.len(),
        critical: 0,
        warning: 0,
        info: 0,
        style: 0,
        files_analysed,
        gates_run,
        gates_failed: outcomes
            .iter()
            .filter(|o| matches!(o.status, GateStatus::Failed(_)))
            .count(),
        gates_panicked: outcomes
            .iter()
            .filter(|o| matches!(o.status, GateStatus::Panicked(_)))
            .count(),
        gates_skipped: outcomes
            .iter()
            .filter(|o| matches!(o.status, GateStatus::Skipped(_)))
            .count(),
        gates_degraded: outcomes.iter().filter(|o| o.is_degraded()).count(),
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
        gate_status: outcomes.to_vec(),
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
) -> anyhow::Result<(Vec<ReviewFinding>, usize, usize, Vec<GateOutcome>)> {
    run_pre_commit_review_with(
        state,
        project_id,
        project_dir,
        generation,
        diff_text,
        config,
        gates::all_gates(),
    )
    .await
}

/// The review with an explicit gate list (tests inject failing gates).
/// Returns (findings, gates dispatched, files analysed, per-gate outcomes).
pub async fn run_pre_commit_review_with(
    state: &AppState,
    project_id: &str,
    project_dir: &Path,
    generation: u64,
    diff_text: &str,
    config: &ReviewConfig,
    gate_list: Vec<Box<dyn Gate>>,
) -> anyhow::Result<(Vec<ReviewFinding>, usize, usize, Vec<GateOutcome>)> {
    let diff_files = parse_unified_diff(diff_text);
    if diff_files.is_empty() {
        return Ok((Vec::new(), 0, 0, Vec::new()));
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

    // External audit 2026-08-29 P0-4: a structurally incomplete index returns
    // no error, so the review checks generation completeness ONCE and every
    // search-backed gate degrades itself on the verdict.
    let search_index_note = match crate::handlers::project_tools::generation_completeness_for(
        state, project_id, generation,
    )
    .await
    {
        Ok(c) if !c.complete => Some(format!(
            "search index generation {} is INCOMPLETE ({} of {} eligible paths missing, cross-store mismatch {}) — searched evidence is unreliable",
            c.generation, c.missing, c.expected_paths, c.cross_store_mismatch
        )),
        Ok(_) => None,
        Err(e) => Some(format!("search index completeness unknown: {e}")),
    };

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
        search_index_note,
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
        tokio::task::JoinHandle<(
            anyhow::Result<Vec<ReviewFinding>>,
            u128,
            Vec<String>,
            Vec<String>,
        )>,
    )> = Vec::new();
    let mut async_gates: Vec<Box<dyn Gate>> = Vec::new();
    let mut gates_run = 0usize;

    // Gates that perform hybrid search must run on the async runtime
    // (they await the search engine); everything else is sync and goes
    // to spawn_blocking for true parallelism.
    const ASYNC_GATES: &[&str] = &["antipattern", "product_intent", "co_added_family"];

    let mut outcomes: Vec<GateOutcome> = Vec::new();
    for gate in gate_list {
        let name = gate.name();
        if config.skip_gates.iter().any(|s| s.as_str() == name) {
            outcomes.push(GateOutcome {
                name,
                status: GateStatus::Skipped("skip_gates".into()),
                caps: Vec::new(),
                elapsed_ms: 0,
            });
            continue;
        }
        gates_run += 1;

        if ASYNC_GATES.contains(&name) {
            async_gates.push(gate);
            continue;
        }
        let shared = shared.clone();
        let state_clone = state.clone(); // AppState is Clone (all Arc fields)
        let handle = tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let ctx = shared.as_borrowed(&state_clone);
            let r = gate.run(&ctx);
            (
                r,
                started.elapsed().as_millis(),
                ctx.take_degraded(),
                ctx.take_caps(),
            )
        });
        sync_handles.push((name, handle));
    }

    let sync_future = async move {
        let mut out: Vec<ReviewFinding> = Vec::new();
        let mut sync_outcomes: Vec<GateOutcome> = Vec::new();
        for (name, h) in sync_handles {
            match h.await {
                Ok((Ok(fs), ms, notes, caps)) => {
                    sync_outcomes.push(GateOutcome {
                        name,
                        status: GateStatus::from_run(fs.len(), notes),
                        elapsed_ms: ms,
                        caps,
                    });
                    out.extend(fs);
                }
                Ok((Err(e), ms, _notes, _caps)) => {
                    tracing::warn!(gate = %name, "pre_commit_review gate failed: {e}");
                    sync_outcomes.push(GateOutcome {
                        name,
                        status: GateStatus::Failed(e.to_string()),
                        caps: Vec::new(),
                        elapsed_ms: ms,
                    });
                }
                Err(e) => {
                    tracing::warn!(gate = %name, "pre_commit_review gate panicked: {e}");
                    sync_outcomes.push(GateOutcome {
                        name,
                        status: GateStatus::Panicked(e.to_string()),
                        caps: Vec::new(),
                        elapsed_ms: 0,
                    });
                }
            }
        }
        (out, sync_outcomes)
    };

    let async_future = async {
        use futures::FutureExt;
        let mut out: Vec<ReviewFinding> = Vec::new();
        let mut async_outcomes: Vec<GateOutcome> = Vec::new();
        for gate in async_gates {
            let name = gate.name();
            let started = std::time::Instant::now();
            let ctx = shared.as_borrowed(state);
            let result = std::panic::AssertUnwindSafe(gate.run_async(&ctx))
                .catch_unwind()
                .await;
            let ms = started.elapsed().as_millis();
            let notes = ctx.take_degraded();
            let caps = ctx.take_caps();
            match result {
                Ok(Ok(fs)) => {
                    async_outcomes.push(GateOutcome {
                        name,
                        status: GateStatus::from_run(fs.len(), notes),
                        elapsed_ms: ms,
                        caps,
                    });
                    out.extend(fs);
                }
                Ok(Err(e)) => {
                    tracing::warn!(gate = name, "pre_commit_review gate failed: {e}");
                    async_outcomes.push(GateOutcome {
                        name,
                        status: GateStatus::Failed(e.to_string()),
                        caps: Vec::new(),
                        elapsed_ms: ms,
                    });
                }
                Err(payload) => {
                    let reason = payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "panic".into());
                    tracing::warn!(gate = name, "pre_commit_review gate panicked: {reason}");
                    async_outcomes.push(GateOutcome {
                        name,
                        status: GateStatus::Panicked(reason),
                        caps: Vec::new(),
                        elapsed_ms: ms,
                    });
                }
            }
        }
        (out, async_outcomes)
    };

    let ((sync_findings, sync_outcomes), (async_findings, async_outcomes)) =
        tokio::join!(sync_future, async_future);
    let mut findings = sync_findings;
    findings.extend(async_findings);
    outcomes.extend(sync_outcomes);
    outcomes.extend(async_outcomes);

    let finalised = aggregate_findings(
        findings,
        &diff_files,
        config.min_severity,
        config.max_findings,
    );
    Ok((finalised, gates_run, diff_files.len(), outcomes))
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
    search_index_note: Option<String>,
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
            search_index_note: self.search_index_note.clone(),
            degraded: std::sync::Mutex::new(Vec::new()),
            caps: std::sync::Mutex::new(Vec::new()),
        }
    }
}

// ─── Pre-computed data builders ─────────────────────────────────────────────

/// Build a `parent_dir → [file_path]` index for every file node in the
/// project. Called once per review so Gate 8 (new-file convention)
/// doesn't hit the graph on every added file.
fn build_files_by_parent(graph: &GraphStore, project_id: &str) -> HashMap<String, Vec<String>> {
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
///
/// Decodes lossily: legacy sources with stray cp1252/latin-1 bytes must
/// degrade to U+FFFD, not vanish from every gate that reads file content.
pub fn read_file_content(project_dir: &Path, rel_path: &str) -> Option<String> {
    let p = project_dir.join(rel_path);
    std::fs::read(p)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

// ─── Co-change / temporal path identity ─────────────────────────────────────
//
// Git history (TemporalCoupling edges, co-change counts) is keyed by
// whatever path spelling existed AT COMMIT TIME. When a repo gets
// restructured (e.g. everything moved under `Site/`), history keeps the
// OLD spelling (`App_Code/x.vb`) even though the file now lives at
// `Site/App_Code/x.vb`. Comparing those raw strings against the current
// diff's paths produces two failure modes:
//
// 1. False positive: a partner that IS in the diff (under the current
//    spelling) gets reported as "not in diff" because the strings don't
//    match character-for-character.
// 2. Suppressed true positive: a partner path built from the stale
//    spelling doesn't exist anywhere in the current tree, so an agent
//    who acts on the finding can't find the file it names.
//
// The two helpers below are the single, shared fix for both gates.rs
// (pre_commit_review's TemporalGate / TestCoverageGate) and
// handle_detect_incomplete_changes (planning_tools.rs) — every co-change
// partner check in the codebase should route through these rather than
// re-implementing raw string comparison.

/// Component-aligned, ASCII-case-insensitive path-suffix identity.
///
/// Two paths refer to the same file when one is a path-suffix of the
/// other, aligned on a `/` component boundary — so `App_Code/x.vb`
/// matches `Site/App_Code/x.vb`, but `Code/x.vb` does NOT match
/// `App_Code/x.vb` (that's a partial path component, not a real prefix,
/// and must never be treated as the same file).
pub fn path_suffix_match(a: &str, b: &str) -> bool {
    fn norm(p: &str) -> String {
        p.replace('\\', "/").trim_start_matches('/').to_string()
    }
    fn suffix_eq(long: &str, short: &str) -> bool {
        let lb = long.as_bytes();
        let sb = short.as_bytes();
        if sb.is_empty() || lb.len() < sb.len() {
            return false;
        }
        let split = lb.len() - sb.len();
        // Byte-wise ASCII-case-insensitive compare — panic-free on any
        // (even non-char-boundary) split, and Windows-friendly casing.
        if !lb[split..].eq_ignore_ascii_case(sb) {
            return false;
        }
        // Component alignment: the suffix must start at the beginning of
        // the path or right after a separator — this is what rejects
        // `Code/x.vb` matching `App_Code/x.vb`.
        split == 0 || lb[split - 1] == b'/'
    }
    let a = norm(a);
    let b = norm(b);
    suffix_eq(&a, &b) || suffix_eq(&b, &a)
}

/// Resolve a (possibly historical) co-change partner path to its
/// current-tree spelling, or `None` when the file no longer exists.
///
/// - a current-tree file that suffix-matches the historical spelling
///   wins, and the CURRENT spelling is returned — never the stale one.
///   Ties break to the shortest match, then lexicographically smallest,
///   so resolution is deterministic.
/// - when the index has no match, a direct disk probe keeps a genuinely
///   existing partner alive even against a stale/partial graph.
/// - otherwise the partner is gone from the tree entirely — callers
///   must drop it rather than emit a path nothing on disk answers to.
pub fn resolve_partner_to_current(
    partner: &str,
    current_files: &[String],
    project_dir: &Path,
) -> Option<String> {
    let mut best: Option<&str> = None;
    for cf in current_files {
        if !path_suffix_match(partner, cf) {
            continue;
        }
        let better = match best {
            None => true,
            Some(b) => cf.len() < b.len() || (cf.len() == b.len() && cf.as_str() < b),
        };
        if better {
            best = Some(cf.as_str());
        }
    }
    if let Some(b) = best {
        return Some(b.replace('\\', "/"));
    }
    let cleaned = partner.replace('\\', "/");
    let cleaned = cleaned.trim_start_matches('/').to_string();
    if project_dir.join(&cleaned).is_file() {
        return Some(cleaned);
    }
    None
}

// Re-export commonly-used items for the gates module.
pub use gates::all_gates;

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

    // ── P0: lossy decode on non-UTF-8 file content ──────────────────────

    #[test]
    fn resolve_diff_source_lossy_decodes_invalid_utf8_patch_file() {
        let dir = std::env::temp_dir().join(format!("engram_p0_lossy_diff_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let patch_path = dir.join("bad.patch");
        // A single cp1252 0x92 byte (curly apostrophe) is invalid UTF-8 on
        // its own — this is the exact byte class that killed the whole
        // review with "stream did not contain valid UTF-8" before the fix.
        let mut bytes =
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+don\x92t crash\n".to_vec();
        std::fs::write(&patch_path, &bytes).unwrap();

        let result = resolve_diff_source(&dir, "bad.patch");
        assert!(
            result.is_ok(),
            "lossy decode must never fail on invalid UTF-8: {result:?}"
        );
        let text = result.unwrap();
        assert!(
            text.contains('\u{FFFD}'),
            "invalid byte should decode to U+FFFD"
        );
        assert!(
            text.contains("don"),
            "surrounding valid content must survive"
        );
        assert!(
            text.contains("t crash"),
            "surrounding valid content must survive"
        );

        bytes.clear(); // silence unused-mut-after-write lints on some toolchains
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_content_lossy_decodes_invalid_utf8() {
        let dir = std::env::temp_dir().join(format!("engram_p0_lossy_read_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("legacy.js"), b"var s = \x92curly\x92;").unwrap();

        let content = read_file_content(&dir, "legacy.js");
        assert!(content.is_some(), "must not fail on non-UTF-8 bytes");
        let content = content.unwrap();
        assert!(content.contains('\u{FFFD}'));
        assert!(content.contains("curly"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── P1: component-aligned path-suffix identity ──────────────────────

    #[test]
    fn path_suffix_match_positive_historical_vs_current_spelling() {
        // Pre-restructure spelling matches the post-restructure spelling.
        assert!(path_suffix_match(
            "App_Code/iFalt.designer.vb",
            "Site/App_Code/iFalt.designer.vb"
        ));
        // Direction shouldn't matter.
        assert!(path_suffix_match(
            "Site/App_Code/iFalt.designer.vb",
            "App_Code/iFalt.designer.vb"
        ));
        // Exact match is trivially a match.
        assert!(path_suffix_match("a/b/c.vb", "a/b/c.vb"));
        // Case-insensitive (Windows-style paths / casing drift).
        assert!(path_suffix_match("APP_CODE/X.VB", "app_code/x.vb"));
        // Backslash-normalised.
        assert!(path_suffix_match(
            "App_Code\\iFalt.designer.vb",
            "Site/App_Code/iFalt.designer.vb"
        ));
    }

    #[test]
    fn path_suffix_match_negative_partial_component_does_not_match() {
        // "Code/x.vb" must NOT match "App_Code/x.vb" — partial path
        // component, not a real directory-boundary prefix.
        assert!(!path_suffix_match("Code/x.vb", "App_Code/x.vb"));
        assert!(!path_suffix_match("App_Code/x.vb", "Code/x.vb"));
        // Unrelated paths never match.
        assert!(!path_suffix_match(
            "Site/App_Code/Other.vb",
            "Site/App_Code/iFalt.designer.vb"
        ));
        // Same filename, different directory family.
        assert!(!path_suffix_match("Scripts/x.vb", "App_Code/x.vb"));
    }

    #[test]
    fn resolve_partner_to_current_reanchors_to_current_spelling() {
        let current = vec![
            "Site/App_Code/shared-code/SystemSettingStore.vb".to_string(),
            "Site/App_GlobalResources/label.en.resx".to_string(),
        ];
        let dir = std::env::temp_dir();
        let resolved = resolve_partner_to_current(
            "App_Code/shared-code/SystemSettingStore.vb",
            &current,
            &dir,
        );
        assert_eq!(
            resolved.as_deref(),
            Some("Site/App_Code/shared-code/SystemSettingStore.vb")
        );
    }

    #[test]
    fn resolve_partner_to_current_none_when_absent_from_tree() {
        let current = vec!["Site/App_Code/Foo.vb".to_string()];
        let dir =
            std::env::temp_dir().join(format!("engram_p1_resolve_absent_{}", std::process::id()));
        // Directory need not even exist — the file definitely isn't on
        // disk under the historical spelling either.
        let resolved = resolve_partner_to_current("App_Code/DeletedLongAgo.vb", &current, &dir);
        assert!(
            resolved.is_none(),
            "must never emit a partner path absent from the current tree"
        );
    }

    #[test]
    fn resolve_partner_to_current_falls_back_to_disk_probe() {
        // Not in the (stale/partial) graph-derived current_files list, but
        // it genuinely exists on disk under the historical spelling.
        let dir = std::env::temp_dir().join(format!(
            "engram_p1_resolve_disk_probe_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("still-here.vb"), b"' still here").unwrap();

        let current: Vec<String> = Vec::new();
        let resolved = resolve_partner_to_current("still-here.vb", &current, &dir);
        assert_eq!(resolved.as_deref(), Some("still-here.vb"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Finding-budget hygiene: generated-file detection ────────────────

    #[test]
    fn is_generated_filename_matches_known_patterns() {
        assert!(is_generated_filename("Site/App_Code/iFalt.designer.vb"));
        assert!(
            is_generated_filename("Foo/Bar.Designer.cs"),
            "case-insensitive"
        );
        assert!(is_generated_filename("Foo/Bar.g.cs"));
        assert!(is_generated_filename("Foo/Bar.generated.ts"));
        assert!(
            is_generated_filename("styles.Generated.CSS"),
            "case-insensitive segment match"
        );
    }

    #[test]
    fn is_generated_filename_negative_on_normal_files() {
        assert!(!is_generated_filename("Site/App_Code/iFalt.vb"));
        assert!(!is_generated_filename("Site/Default.aspx.vb"));
        assert!(!is_generated_filename("src/handler.ts"));
        assert!(
            !is_generated_filename("gadget.cs"),
            "must not match on substring 'g'"
        );
    }

    #[test]
    fn has_generated_header_detects_marker_within_first_20_lines() {
        let content = "Option Strict On\n// <auto-generated>\n// This file was machine-generated.\n// </auto-generated>\nPartial Class Foo\nEnd Class\n";
        assert!(has_generated_header(content));

        let content2 = "// This code was generated by a tool.\nPartial Class Bar\nEnd Class\n";
        assert!(has_generated_header(content2));

        let content3 = "// DO NOT EDIT this file directly.\nPartial Class Baz\nEnd Class\n";
        assert!(has_generated_header(content3));
    }

    #[test]
    fn has_generated_header_ignores_marker_beyond_first_20_lines() {
        let mut content = String::new();
        for i in 0..25 {
            content.push_str(&format!("' line {i}\n"));
        }
        content.push_str("' <auto-generated>\n");
        assert!(
            !has_generated_header(&content),
            "a marker past line 20 must not count"
        );
    }

    #[test]
    fn has_generated_header_negative_on_normal_file() {
        let content = "Public Class Foo\n    Public Sub Bar()\n    End Sub\nEnd Class\n";
        assert!(!has_generated_header(content));
    }

    #[test]
    fn apply_generated_exemption_passthrough_when_not_generated() {
        let would_be = vec![ReviewFinding::new(
            Severity::Style,
            "style",
            "foo.vb",
            "t",
            "d",
            "s",
        )];
        let out = apply_generated_exemption("style", "foo.vb", false, would_be.clone());
        assert_eq!(out.len(), would_be.len());
        assert_eq!(out[0].title, would_be[0].title);
    }

    #[test]
    fn apply_generated_exemption_silent_when_generated_with_no_findings() {
        let out = apply_generated_exemption("style", "foo.designer.vb", true, Vec::new());
        assert!(out.is_empty(), "no would-be findings means no skip notice");
    }

    #[test]
    fn apply_generated_exemption_collapses_to_one_info_finding() {
        let would_be = vec![
            ReviewFinding::new(Severity::Style, "style", "foo.designer.vb", "t1", "d", "s"),
            ReviewFinding::new(Severity::Style, "style", "foo.designer.vb", "t2", "d", "s"),
            ReviewFinding::new(Severity::Style, "style", "foo.designer.vb", "t3", "d", "s"),
        ];
        let out = apply_generated_exemption("style", "foo.designer.vb", true, would_be);
        assert_eq!(out.len(), 1, "must collapse to exactly one finding");
        assert_eq!(out[0].severity, Severity::Info);
        assert!(out[0].title.to_lowercase().contains("generated"));
        assert!(
            out[0].detail.contains('3'),
            "must cite the suppressed count"
        );
    }

    // ── Finding-budget hygiene: per-file style collapse ─────────────────

    #[test]
    fn collapse_style_findings_collapses_same_title_same_file() {
        let mut findings = Vec::new();
        for line in [994, 995, 996] {
            findings.push(
                ReviewFinding::new(
                    Severity::Style,
                    "style",
                    "Site/App_Code/iFalt.designer.vb",
                    "Indentation mismatch — space on tab-indented file",
                    "d",
                    "s",
                )
                .with_lines(vec![line]),
            );
        }
        let out = collapse_style_findings(findings);
        assert_eq!(
            out.len(),
            1,
            "3 identical-title style findings collapse to 1"
        );
        assert!(out[0].title.contains("×3"));
        assert!(out[0].title.contains("L994"), "must cite the first line");
    }

    #[test]
    fn collapse_style_findings_leaves_singleton_untouched() {
        let findings = vec![
            ReviewFinding::new(
                Severity::Style,
                "style",
                "foo.vb",
                "Indentation mismatch",
                "d",
                "s",
            )
            .with_lines(vec![10]),
        ];
        let out = collapse_style_findings(findings.clone());
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].title, findings[0].title,
            "a lone finding isn't rewritten"
        );
    }

    #[test]
    fn collapse_style_findings_keeps_different_titles_separate() {
        let findings = vec![
            ReviewFinding::new(
                Severity::Style,
                "style",
                "foo.vb",
                "Method `A` doesn't match",
                "d",
                "s",
            ),
            ReviewFinding::new(
                Severity::Style,
                "style",
                "foo.vb",
                "Method `B` doesn't match",
                "d",
                "s",
            ),
        ];
        let out = collapse_style_findings(findings);
        assert_eq!(
            out.len(),
            2,
            "distinct method names must not collapse together"
        );
    }

    #[test]
    fn collapse_style_findings_ignores_non_style_severity() {
        let findings = vec![
            ReviewFinding::new(Severity::Warning, "style", "foo.vb", "Same title", "d", "s"),
            ReviewFinding::new(Severity::Warning, "style", "foo.vb", "Same title", "d", "s"),
        ];
        let out = collapse_style_findings(findings);
        assert_eq!(out.len(), 2, "Warning findings are never collapsed");
    }

    // ── Finding-budget hygiene: resx-family collapse ────────────────────

    #[test]
    fn collapse_resx_family_findings_collapses_own_path_variants() {
        let langs = ["", ".en", ".de", ".es"];
        let findings: Vec<ReviewFinding> = langs
            .iter()
            .map(|lang| {
                ReviewFinding::new(
                    Severity::Style,
                    "new_file",
                    format!("Site/App_GlobalResources/label{lang}.resx"),
                    "File naming doesn't match `sys_*` convention",
                    "d",
                    "s",
                )
            })
            .collect();
        let out = collapse_resx_family_findings(findings);
        assert_eq!(out.len(), 1, "4 language variants collapse to 1");
        assert!(out[0].file_path.contains("label.*.resx") || out[0].title.contains("label.*.resx"));
        assert!(out[0].title.contains('4'));
    }

    #[test]
    fn collapse_resx_family_findings_collapses_embedded_reference_variants() {
        let langs = ["label.resx", "label.en.resx", "label.de.resx"];
        let findings: Vec<ReviewFinding> = langs
            .iter()
            .map(|f| {
                ReviewFinding::new(
                    Severity::Info,
                    "temporal",
                    "Site/Default.aspx",
                    format!("Coupled file `Site/App_GlobalResources/{f}` not in diff"),
                    "d",
                    "s",
                )
            })
            .collect();
        let out = collapse_resx_family_findings(findings);
        assert_eq!(
            out.len(),
            1,
            "3 co-change partners in the same resx family collapse to 1"
        );
        assert_eq!(
            out[0].file_path, "Site/Default.aspx",
            "host file is preserved"
        );
        assert!(out[0].title.contains('3'));
    }

    #[test]
    fn collapse_resx_family_findings_leaves_single_variant_untouched() {
        let findings = vec![ReviewFinding::new(
            Severity::Style,
            "new_file",
            "Site/App_GlobalResources/label.resx",
            "File naming doesn't match `sys_*` convention",
            "d",
            "s",
        )];
        let out = collapse_resx_family_findings(findings.clone());
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].title, findings[0].title,
            "lone resx finding is untouched"
        );
    }

    #[test]
    fn collapse_resx_family_findings_does_not_merge_different_stems() {
        let findings = vec![
            ReviewFinding::new(
                Severity::Style,
                "new_file",
                "Site/App_GlobalResources/label.resx",
                "File naming doesn't match `sys_*` convention",
                "d",
                "s",
            ),
            ReviewFinding::new(
                Severity::Style,
                "new_file",
                "Site/App_GlobalResources/text.resx",
                "File naming doesn't match `sys_*` convention",
                "d",
                "s",
            ),
        ];
        let out = collapse_resx_family_findings(findings);
        assert_eq!(out.len(), 2, "different basenames are different families");
    }

    // ── Finding-budget hygiene: severity-ordered per-gate budget ────────

    #[test]
    fn apply_per_gate_budget_caps_noisy_gate_without_starving_others() {
        let mut findings = Vec::new();
        for i in 0..100 {
            findings.push(ReviewFinding::new(
                Severity::Style,
                "style",
                format!("f{i}.vb"),
                format!("style finding {i}"),
                "d",
                "s",
            ));
        }
        findings.push(ReviewFinding::new(
            Severity::Warning,
            "immune",
            "a.vb",
            "w1",
            "d",
            "s",
        ));
        findings.push(ReviewFinding::new(
            Severity::Warning,
            "immune",
            "b.vb",
            "w2",
            "d",
            "s",
        ));

        let out = apply_per_gate_budget(findings, 30);
        let immune_count = out.iter().filter(|f| f.gate == "immune").count();
        let style_count = out.iter().filter(|f| f.gate == "style").count();
        assert_eq!(immune_count, 2, "the small gate keeps all its findings");
        assert!(
            style_count < 100 && style_count >= 10,
            "the noisy gate is capped, not starved: {style_count}"
        );
    }

    #[test]
    fn apply_per_gate_budget_noop_when_total_fits_budget() {
        // Regression: a gate whose count exceeds max_findings/num_gates
        // must NOT be truncated while the total is under budget — the
        // per-gate cap exists to arbitrate a scarce budget, not to delete
        // findings there was room for. (Live repro: temporal gate lost 6
        // of 39 real coupling findings to a 200-budget cap while the whole
        // review emitted only 75.)
        let mut findings = Vec::new();
        for i in 0..39 {
            findings.push(ReviewFinding::new(
                Severity::Info,
                "temporal",
                format!("f{i}.vb"),
                format!("coupled finding {i}"),
                "d",
                "s",
            ));
        }
        for i in 0..5 {
            findings.push(ReviewFinding::new(
                Severity::Warning,
                "immune",
                format!("w{i}.vb"),
                format!("warning finding {i}"),
                "d",
                "s",
            ));
        }
        // 44 findings, 2 gates → per-gate share would be 100, but even
        // with a share below the noisy gate's count (e.g. max=60 → 30
        // each) nothing may be dropped while 44 <= 60.
        let out = apply_per_gate_budget(findings, 60);
        assert_eq!(out.len(), 44, "under-budget totals pass through untouched");
        let temporal = out.iter().filter(|f| f.gate == "temporal").count();
        assert_eq!(temporal, 39, "no per-gate truncation under budget");
    }

    #[test]
    fn aggregate_findings_warnings_survive_truncation_over_style() {
        let mut findings = Vec::new();
        for i in 0..40 {
            findings.push(ReviewFinding::new(
                Severity::Style,
                "style",
                format!("f{i}.vb"),
                format!("style finding {i}"),
                "d",
                "s",
            ));
        }
        for i in 0..5 {
            findings.push(ReviewFinding::new(
                Severity::Warning,
                "immune",
                format!("w{i}.vb"),
                format!("warning finding {i}"),
                "d",
                "s",
            ));
        }
        let out = aggregate_findings(findings, &[], Severity::Style, 10);
        assert_eq!(out.len(), 10);
        let warnings = out
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count();
        let styles = out.iter().filter(|f| f.severity == Severity::Style).count();
        assert_eq!(warnings, 5, "all warnings must survive a tight truncation");
        assert_eq!(
            styles, 5,
            "style only fills whatever budget warnings leave behind"
        );
    }

    #[test]
    fn corroboration_meta_sorted_after_primaries_within_severity() {
        // 3 gates flagging the same file trigger a corroboration meta
        // finding (see `aggregate_emits_corroboration_when_3_gates_hit_same_file`).
        // Use Warning primaries so the escalation rule (Warning -> Warning)
        // lands the meta finding in the SAME severity tier as the
        // primaries it references — that's the case where the tie-break
        // actually matters.
        let f1 = ReviewFinding::new(Severity::Warning, "a", "foo.rs", "x1", "d", "s");
        let f2 = ReviewFinding::new(Severity::Warning, "b", "foo.rs", "x2", "d", "s");
        let f3 = ReviewFinding::new(Severity::Warning, "c", "foo.rs", "x3", "d", "s");
        let out = aggregate_findings(vec![f1, f2, f3], &[], Severity::Style, 100);
        let meta_pos = out
            .iter()
            .position(|f| f.gate == "corroboration")
            .expect("3 gates on one file must produce a corroboration meta-finding");
        assert_eq!(
            out[meta_pos].severity,
            Severity::Warning,
            "sanity check: meta landed in the same tier as the Warning primaries"
        );
        let primary_positions: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, f)| f.gate != "corroboration")
            .map(|(i, _)| i)
            .collect();
        assert!(
            primary_positions.iter().all(|&p| p < meta_pos),
            "corroboration meta must sort after all same-severity primaries"
        );
    }
}

#[cfg(test)]
mod header_gate_total_tests {
    use super::*;

    #[test]
    fn header_reports_the_registered_gate_total_not_a_literal() {
        let n = gates::all_gates().len();
        let out = render_markdown(&[], 3, n, 12, &[]);
        let header = out.lines().find(|l| l.contains("Gates run")).unwrap_or("");
        assert!(
            header.contains(&format!("**Gates run**: {n}/{n}")),
            "header must show gates_run over the REGISTERED total ({n}); got: {header}"
        );
        // A skipped gate lowers the numerator only.
        let out = render_markdown(&[], 3, n - 2, 12, &[]);
        let header = out.lines().find(|l| l.contains("Gates run")).unwrap_or("");
        assert!(
            header.contains(&format!("**Gates run**: {}/{n}", n - 2)),
            "got: {header}"
        );
    }
}
