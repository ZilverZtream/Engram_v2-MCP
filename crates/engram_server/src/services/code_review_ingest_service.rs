//! Code-review ingestion — turns CodeRabbit (and future: GitHub review,
//! CodeClimate, etc.) review history into first-class Engram rules.
//!
//! The pipeline is five deterministic stages plus an optional LLM pass:
//!
//! 1. **Fetch** — pre-scraped JSONL file or live Azure DevOps API.
//!    Both converge on the same `RawReviewComment` representation.
//! 2. **Parse** — reject walkthroughs/meta, strip HTML, extract rule
//!    text from bold titles, extract pattern tokens (backticks +
//!    PascalCase + method calls), capture fix commit hashes from
//!    `✅ Addressed in commits <sha>` markers, map thread-status to
//!    weighted fix signal.
//! 3. **Cluster** — Jaccard token overlap (default 0.4) + same
//!    language. Canonical rule = highest composite score
//!    (`fix_weight + severity_weight`). Compute fix_rate per cluster.
//! 4. **Store** — three sinks:
//!    - Fixed / closed / active clusters → Tantivy `antipattern`
//!      namespace with `source=coderabbit` metadata
//!    - WontFix clusters → Tantivy `wontfix_patterns` namespace,
//!      scoped by file-path pattern for precision-targeted suppression
//!    - Graph: one `review_pattern` node per cluster with
//!      `AntiPattern` edges to every file mentioned
//!    - Registry: high-confidence rules (fix_rate ≥ 0.7, ≥ 3 PRs)
//!      auto-promoted to `cr_*` repo rules injected into chunk reads
//! 5. **Incremental** — `cr_ingest:<source_sig>:last_pr_id` in
//!    registry meta. Live fetches stop walking PRs the moment they
//!    hit last_pr_id; JSONL imports dedupe by content hash.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use engram_core::registry::RepoRule;
use engram_core::{ContentHash, DocIdStr, RelPath};
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_index::{IndexDoc, chunk_id_from_content_hash};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

// ─── Public types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadStatus {
    Fixed,
    Active,
    Closed,
    WontFix,
    Unknown,
}

// Custom deserializer that tolerates whatever Azure DevOps or CodeRabbit
// decide to emit — unknown status strings fall back to `Unknown`
// instead of failing the whole JSONL line. Keeping the record with a
// known-unknown status is better than dropping it on the floor, since
// `Unknown` is already a weight-zero no-op in the pipeline.
impl<'de> Deserialize<'de> for ThreadStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
    }
}

impl ThreadStatus {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "fixed" => Self::Fixed,
            "active" | "open" | "pending" => Self::Active,
            "closed" | "resolved" => Self::Closed,
            "wontfix" | "won't fix" | "won't-fix" | "wont_fix" | "wont fix" => Self::WontFix,
            _ => Self::Unknown,
        }
    }

    /// Weight contribution toward canonical-rule selection.
    ///
    /// `Closed` is 0.6 rather than 0.5 — when a thread has been
    /// resolved without a `✅ Addressed in commits` marker, a human
    /// manually pressed Resolved, which on balance leans "fixed"
    /// rather than "wontFix". Explicit `✅ Addressed in commits <sha>`
    /// in the body always overrides to `Fixed` regardless of this
    /// value (see `effective_fix_status`).
    pub fn weight(self) -> f32 {
        match self {
            Self::Fixed => 1.0,
            Self::Closed => 0.6,
            Self::Active => 0.3,
            // WontFix participates in suppression, not positive rules.
            Self::WontFix => 0.0,
            Self::Unknown => 0.0,
        }
    }
}

/// Compute the effective fix status given the raw thread_status and
/// whether CodeRabbit posted a `✅ Addressed in commits <sha>` marker
/// anywhere in the thread. The marker is the definitive fix signal
/// and always promotes the status to `Fixed` — even if Azure DevOps
/// ended up with the thread still showing `Active` or `Closed`.
fn effective_fix_status(raw: ThreadStatus, fix_commit: Option<&str>) -> ThreadStatus {
    if fix_commit.is_some() && !matches!(raw, ThreadStatus::WontFix) {
        // WontFix is sacred — even if CR later acknowledged the fix in
        // some downstream commit, the team explicitly rejected the
        // pattern here, so it belongs in suppression.
        return ThreadStatus::Fixed;
    }
    raw
}

/// Normalised severity — mapped from CodeRabbit's own labels plus our
/// heuristic upgrade path (high-severity language in the body can
/// promote a minor → warning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
    Style,
}

impl Severity {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" | "major" | "critical" | "error" | "blocker" => Self::Critical,
            "medium" | "warn" | "warning" => Self::Warning,
            "minor" | "low" | "info" => Self::Info,
            _ => Self::Info,
        }
    }

    fn weight(self) -> f32 {
        match self {
            Self::Critical => 1.0,
            Self::Warning => 0.6,
            Self::Info => 0.3,
            Self::Style => 0.1,
        }
    }
}

/// Raw comment record — shape of a single JSONL line and also the
/// target shape that the live Azure DevOps fetcher builds into. One
/// representation lets every downstream stage be source-agnostic.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RawReviewComment {
    pub pr_id: u64,
    #[serde(default)]
    pub pr_title: String,
    #[serde(default)]
    pub pr_author: String,
    #[serde(default)]
    pub pr_date: String,
    #[serde(default)]
    pub pr_branch: String,
    #[serde(default)]
    pub pr_url: String,
    #[serde(default)]
    pub thread_id: u64,
    #[serde(default = "ThreadStatus::unknown_default")]
    pub thread_status: ThreadStatus,
    #[serde(default)]
    pub file_path: String,
    #[serde(default)]
    pub line_start: u32,
    #[serde(default)]
    pub line_end: u32,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub coderabbit_comment: String,
    /// Token-anchored fix exemplar: the unified-diff hunk from the merged
    /// PR's later iterations that changed the code this finding named.
    /// Populated (fail-soft) only for resolved findings during the
    /// azure_devops fetch; None for JSONL and unresolved findings.
    #[serde(default)]
    pub fix_hunk: Option<String>,
}

impl ThreadStatus {
    fn unknown_default() -> Self {
        Self::Unknown
    }
}

/// Inferred resolution for ambiguous (closed, no explicit ✅)
/// threads. Populated only when the caller opts into LLM-assisted
/// classification; always `None` in the deterministic-only path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmResolution {
    /// LLM inferred the developer resolved by fixing the code.
    Fixed,
    /// LLM inferred the developer resolved by dismissing the finding.
    Dismissed,
    /// LLM refused to classify.
    Unknown,
}

/// Parsed rule after extraction. One per raw comment that isn't meta.
#[derive(Debug, Clone)]
pub struct ParsedRule {
    pub rule_text: String,
    pub pattern_tokens: Vec<String>,
    pub file_path: String,
    pub file_pattern: String,
    pub language: String,
    pub severity: Severity,
    pub fix_status: ThreadStatus,
    pub fix_commit: Option<String>,
    /// When the caller opted into `use_llm_for_ambiguous`, this is the
    /// classifier's verdict for a `closed` thread that had no
    /// `✅ Addressed in commits` marker. Never populated via a live
    /// LLM call when the field is `Some` — the value is either freshly
    /// returned from the backend or loaded from the per-hash cache in
    /// the registry, so the classifier spends tokens at most once per
    /// unique finding.
    pub llm_resolution: Option<LlmResolution>,
    pub pr_id: u64,
    pub pr_url: String,
    pub thread_id: u64,
    pub pr_date: String,
    /// Unique identity of this specific (pr_id, thread_id) review —
    /// drives record-level dedup.
    pub content_hash: String,
    /// Semantic identity of the *finding* itself — rule_text + tokens +
    /// file_pattern, WITHOUT the pr_id/thread_id. Used as the LLM
    /// classifier cache key so we don't spend tokens re-classifying
    /// the same finding from a different PR.
    pub semantic_hash: String,
    pub raw_body: String,
    /// Token-anchored fix exemplar (unified-diff hunk) carried from the
    /// raw comment — the concrete before→after the team applied.
    pub fix_hunk: Option<String>,
}

/// A cluster of ParsedRules that represent the same underlying pattern.
#[derive(Debug, Clone)]
pub struct ReviewCluster {
    pub cluster_id: String,
    pub canonical: ParsedRule,
    pub members: Vec<ParsedRule>,
    pub fix_rate: f32,
    pub confidence: f32,
    pub pr_ids: Vec<u64>,
    pub file_paths: Vec<String>,
    pub file_patterns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IngestStats {
    pub total_raw: usize,
    pub parsed_success: usize,
    pub parsed_skipped: usize,
    pub clusters_produced: usize,
    pub suppression_clusters: usize,
    pub antipattern_docs_indexed: usize,
    pub suppression_docs_indexed: usize,
    pub graph_nodes_created: usize,
    pub graph_edges_created: usize,
    pub repo_rules_promoted: usize,
    pub incremental_skipped_prs: usize,
    pub newest_pr_id: Option<u64>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub struct IngestConfig {
    pub source: IngestSource,
    pub min_fix_rate: f32,
    pub token_overlap_threshold: f32,
    pub max_findings: usize,
    pub promote_repo_rule_fix_rate: f32,
    pub promote_repo_rule_min_prs: usize,
    pub force_full_rescan: bool,
    /// When `true`, closed threads without a `✅ Addressed in
    /// commits` marker are classified by the configured LLM backend
    /// (OpenRouter / Ollama / OpenAI). Results are cached by content
    /// hash so the classifier never spends tokens twice on the same
    /// finding. Off by default — deterministic path works fine.
    pub use_llm_for_ambiguous: bool,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            source: IngestSource::JsonlFile {
                path: PathBuf::new(),
            },
            // Matches the 43% fixed-rate floor in the pilot corpus while
            // still rejecting noise: ≥ 50% of decisive threads must say
            // `fixed` before we treat the pattern as a positive rule.
            min_fix_rate: 0.5,
            token_overlap_threshold: 0.4,
            max_findings: 10_000,
            promote_repo_rule_fix_rate: 0.7,
            promote_repo_rule_min_prs: 3,
            force_full_rescan: false,
            use_llm_for_ambiguous: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum IngestSource {
    /// Pre-scraped JSONL produced by the reference Python scraper.
    JsonlFile { path: PathBuf },
    /// Live Azure DevOps fetch.
    AzureDevops {
        org: String,
        project: String,
        repo: String,
        pat_token: String,
        max_prs: Option<usize>,
    },
}

impl IngestSource {
    /// Stable signature used to key the incremental-fetch state. Never
    /// includes the PAT.
    fn signature(&self) -> String {
        match self {
            Self::JsonlFile { path } => format!("jsonl:{}", path.display()),
            Self::AzureDevops {
                org, project, repo, ..
            } => format!("azdo:{org}:{project}:{repo}"),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            Self::JsonlFile { .. } => "jsonl",
            Self::AzureDevops { .. } => "azure_devops",
        }
    }
}

// ─── Public entrypoint ──────────────────────────────────────────────────────

pub async fn ingest_code_review_history(
    state: &AppState,
    project_id: &str,
    config: IngestConfig,
) -> anyhow::Result<IngestStats> {
    let start = std::time::Instant::now();
    let mut stats = IngestStats::default();
    let source_sig = config.source.signature();

    // Read incremental state unless the caller forced a full rescan.
    let last_pr_id: Option<u64> = if config.force_full_rescan {
        None
    } else {
        state
            .registry
            .get_meta(project_id, &meta_key(&source_sig, "last_pr_id"))
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
    };

    // Stage 1: fetch
    let (raw, skipped) = fetch_raw_comments(&config.source, last_pr_id).await?;
    stats.total_raw = raw.len();
    stats.incremental_skipped_prs = skipped;

    // Stage 2: parse
    let mut parsed: Vec<ParsedRule> = Vec::with_capacity(raw.len());
    let mut seen_hashes: HashSet<String> = HashSet::with_capacity(raw.len());
    let mut newest_pr: Option<u64> = None;
    for r in &raw {
        newest_pr = Some(newest_pr.map(|p| p.max(r.pr_id)).unwrap_or(r.pr_id));
        match parse_comment(r) {
            Some(p) => {
                if seen_hashes.insert(p.content_hash.clone()) {
                    parsed.push(p);
                } else {
                    stats.parsed_skipped += 1;
                }
            }
            None => stats.parsed_skipped += 1,
        }
    }
    stats.parsed_success = parsed.len();
    stats.newest_pr_id = newest_pr;

    // Stage 2b: optional LLM classification of ambiguous `closed`
    // threads. Runs once per unique finding (cached by content hash)
    // so reruns cost zero tokens.
    if config.use_llm_for_ambiguous {
        for r in &mut parsed {
            if let Some(verdict) = classify_ambiguous(state, project_id, r).await {
                r.llm_resolution = Some(verdict);
            }
        }
    }

    // Stage 3: cluster on the FULL set (positive + suppression) so
    // fix_rate reflects the actual team behaviour across all
    // similar findings, not a partition-biased subset. After
    // clustering we split into sinks.
    let all_clusters = cluster_rules(parsed, config.token_overlap_threshold);

    // A cluster contributes to the positive antipattern index when its
    // fix_rate meets the threshold — computed over (fixed ∪
    // llm-fixed) / (fixed ∪ llm-fixed ∪ wontFix ∪ llm-dismissed).
    // A cluster contributes to the suppression namespace whenever it
    // has any wontFix or llm-dismissed members; the suppression doc
    // is narrowed to just those members' file patterns so the signal
    // is precise (matches the user's "scope by file family" rule).
    let mut positive_clusters: Vec<ReviewCluster> = Vec::new();
    let mut suppression_clusters: Vec<ReviewCluster> = Vec::new();
    for cluster in all_clusters {
        let has_suppression_members = cluster.members.iter().any(is_suppression);
        let has_positive_members = cluster.members.iter().any(|m| !is_suppression(m));

        if has_positive_members && cluster.fix_rate >= config.min_fix_rate {
            positive_clusters.push(cluster.clone());
        }
        if has_suppression_members {
            // Narrow the suppression cluster to just the wontFix /
            // llm-dismissed members so the file patterns stored with
            // it match ONLY the files the team actually said "leave
            // alone" about. This is what keeps the gQtyManager
            // suppression precise to qtyManager files instead of
            // dampening every TypeScript file globally.
            let supp_members: Vec<ParsedRule> = cluster
                .members
                .iter()
                .filter(|m| is_suppression(m))
                .cloned()
                .collect();
            if !supp_members.is_empty() {
                suppression_clusters.push(build_cluster(supp_members));
            }
        }
    }
    stats.clusters_produced = positive_clusters.len();
    stats.suppression_clusters = suppression_clusters.len();

    // Stage 4: store
    let gen_ = crate::services::project_service::get_active_generation(state, project_id).await?;
    let ps = crate::services::project_service::ensure_project_runtime(state, project_id).await?;

    // 4a. Index positive rules into antipattern namespace.
    let anti_docs = build_index_docs(&positive_clusters, "antipattern", gen_);
    if !anti_docs.is_empty() {
        ps.search
            .index_docs(project_id, &anti_docs, &CancellationToken::new())
            .await?;
        stats.antipattern_docs_indexed = anti_docs.len();
    }

    // 4b. Index suppression rules into wontfix_patterns namespace. We
    // do NOT drop them — they're the signal that stops future
    // immune_check from false-positiving on the same "intentional" code.
    let supp_docs = build_index_docs(&suppression_clusters, "wontfix_patterns", gen_);
    if !supp_docs.is_empty() {
        ps.search
            .index_docs(project_id, &supp_docs, &CancellationToken::new())
            .await?;
        stats.suppression_docs_indexed = supp_docs.len();
    }

    // 4c. Graph: review_pattern nodes + AntiPattern edges.
    let (nodes, edges) = build_graph_entities(&positive_clusters, &suppression_clusters, gen_);
    stats.graph_nodes_created = nodes.len();
    stats.graph_edges_created = edges.len();
    if !nodes.is_empty() || !edges.is_empty() {
        let graph = state.graph.clone();
        let pid = project_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            if !nodes.is_empty() {
                graph.upsert_nodes(&pid, &nodes)?;
            }
            if !edges.is_empty() {
                graph.upsert_edges(&pid, &edges)?;
            }
            Ok(())
        })
        .await??;
    }

    // 4d. Registry: auto-promote high-confidence clusters to repo rules.
    for c in &positive_clusters {
        if c.fix_rate >= config.promote_repo_rule_fix_rate
            && c.pr_ids.len() >= config.promote_repo_rule_min_prs
        {
            let rule = RepoRule {
                rule_id: format!("cr_{}", &c.cluster_id[..12]),
                file_pattern: c
                    .file_patterns
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "**/*".into()),
                rule_text: format!(
                    "{} — CodeRabbit pattern, {} PRs, {:.0}% fix rate",
                    c.canonical.rule_text,
                    c.pr_ids.len(),
                    c.fix_rate * 100.0
                ),
                priority: (c.confidence * 100.0) as i32,
                updated_at_ms: now_ms(),
            };
            state.registry.put_repo_rule(project_id, &rule)?;
            stats.repo_rules_promoted += 1;
        }
    }

    // Stage 5: update incremental state.
    if let Some(newest) = newest_pr {
        state.registry.set_meta(
            project_id,
            &meta_key(&source_sig, "last_pr_id"),
            &newest.to_string(),
        )?;
        state.registry.set_meta(
            project_id,
            &meta_key(&source_sig, "last_fetch_ms"),
            &now_ms().to_string(),
        )?;
        state.registry.set_meta(
            project_id,
            &meta_key(&source_sig, "source_kind"),
            config.source.kind_str(),
        )?;
    }

    stats.elapsed_ms = start.elapsed().as_millis();
    Ok(stats)
}

fn meta_key(source_sig: &str, field: &str) -> String {
    // Registry meta keys are flat strings; we namespace with `cr_ingest:`
    // + a source signature + the field. The signature is stable and
    // deterministic per source (see `IngestSource::signature`).
    format!("cr_ingest:{source_sig}:{field}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── Stage 1: fetch ─────────────────────────────────────────────────────────

async fn fetch_raw_comments(
    source: &IngestSource,
    last_pr_id: Option<u64>,
) -> anyhow::Result<(Vec<RawReviewComment>, usize)> {
    match source {
        IngestSource::JsonlFile { path } => read_jsonl(path, last_pr_id),
        IngestSource::AzureDevops {
            org,
            project,
            repo,
            pat_token,
            max_prs,
        } => fetch_azure_devops(org, project, repo, pat_token, *max_prs, last_pr_id).await,
    }
}

fn read_jsonl(
    path: &Path,
    last_pr_id: Option<u64>,
) -> anyhow::Result<(Vec<RawReviewComment>, usize)> {
    let bytes = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read jsonl at {}: {e}", path.display()))?;
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for (lineno, line) in bytes.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: RawReviewComment = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(line = lineno + 1, "skipping malformed JSONL: {e}");
                continue;
            }
        };
        if let Some(last) = last_pr_id {
            if rec.pr_id <= last {
                skipped += 1;
                continue;
            }
        }
        out.push(rec);
    }
    Ok((out, skipped))
}

async fn fetch_azure_devops(
    org: &str,
    project: &str,
    repo: &str,
    pat_token: &str,
    max_prs: Option<usize>,
    last_pr_id: Option<u64>,
) -> anyhow::Result<(Vec<RawReviewComment>, usize)> {
    use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};

    // Basic auth: PAT goes in the password half of `user:pass`, user is blank.
    // Inline base64 encode — it's the only crypto-adjacent op we need and adding
    // a `base64` dep just for one header isn't worth the supply-chain footprint.
    let auth = format!(
        "Basic {}",
        base64_encode(format!(":{pat_token}").as_bytes())
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let base = format!(
        "https://dev.azure.com/{org}/{project}/_apis/git/repositories/{repo}",
        org = url_escape(org),
        project = url_escape(project),
        repo = url_escape(repo),
    );

    // List completed PRs — paginate via $skip. We build the URL with
    // query params inline rather than using `RequestBuilder::query` —
    // some reqwest feature combinations don't expose it.
    let mut prs_raw: Vec<serde_json::Value> = Vec::new();
    let page_size = 100usize;
    let mut skip = 0usize;
    loop {
        let url = format!(
            "{base}/pullrequests?searchCriteria.status=completed&api-version=7.1&$top={page_size}&$skip={skip}"
        );
        let resp = client
            .get(&url)
            .header(AUTHORIZATION, &auth)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .send()
            .await?
            .error_for_status()?;
        let body: serde_json::Value = resp.json().await?;
        let batch = body
            .get("value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let n = batch.len();
        prs_raw.extend(batch);
        if n < page_size {
            break;
        }
        skip += n;
        if let Some(cap) = max_prs
            && prs_raw.len() >= cap
        {
            prs_raw.truncate(cap);
            break;
        }
    }

    // Sort newest-first by pullRequestId.
    prs_raw.sort_by(|a, b| {
        let a_id = a.get("pullRequestId").and_then(|v| v.as_u64()).unwrap_or(0);
        let b_id = b.get("pullRequestId").and_then(|v| v.as_u64()).unwrap_or(0);
        b_id.cmp(&a_id)
    });

    let mut out: Vec<RawReviewComment> = Vec::new();
    let mut skipped_incremental = 0usize;
    for pr in prs_raw {
        let pr_id = pr
            .get("pullRequestId")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if pr_id == 0 {
            continue;
        }
        if let Some(last) = last_pr_id {
            if pr_id <= last {
                // Newer PRs are exhausted (sorted desc) — stop walking.
                skipped_incremental += 1;
                break;
            }
        }
        let pr_title = pr
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let pr_author = pr
            .get("createdBy")
            .and_then(|v| v.get("displayName"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let pr_date = pr
            .get("closedDate")
            .or_else(|| pr.get("creationDate"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let pr_branch = pr
            .get("sourceRefName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_start_matches("refs/heads/")
            .to_string();
        let pr_url =
            format!("https://dev.azure.com/{org}/{project}/_git/{repo}/pullrequest/{pr_id}");

        // Fetch threads for this PR.
        let threads_url = format!("{base}/pullrequests/{pr_id}/threads?api-version=7.1");
        let threads_resp = match client
            .get(&threads_url)
            .header(AUTHORIZATION, &auth)
            .header(ACCEPT, "application/json")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(pr = pr_id, "azdo threads fetch failed: {e}");
                continue;
            }
        };
        let threads_resp = match threads_resp.error_for_status() {
            Ok(r) => r,
            Err(_) => continue,
        };
        let threads_body: serde_json::Value = match threads_resp.json().await {
            Ok(j) => j,
            Err(_) => continue,
        };
        let threads = threads_body
            .get("value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for thread in threads {
            let comments = thread
                .get("comments")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            // Keep only CodeRabbit-authored comments.
            let cr_text: Vec<String> = comments
                .iter()
                .filter(|c| is_coderabbit_author(c.get("author")))
                .filter_map(|c| c.get("content").and_then(|v| v.as_str()).map(String::from))
                .collect();
            if cr_text.is_empty() {
                continue;
            }
            let thread_status = ThreadStatus::parse(
                thread
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
            );
            let thread_id = thread.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let ctx = thread.get("threadContext");
            let file_path = ctx
                .and_then(|c| c.get("filePath"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let line_start = ctx
                .and_then(|c| c.get("rightFileStart"))
                .and_then(|p| p.get("line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let line_end = ctx
                .and_then(|c| c.get("rightFileEnd"))
                .and_then(|p| p.get("line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let severity = extract_severity_string(&cr_text.join("\n\n"));

            out.push(RawReviewComment {
                pr_id,
                pr_title: pr_title.clone(),
                pr_author: pr_author.clone(),
                pr_date: pr_date.clone(),
                pr_branch: pr_branch.clone(),
                pr_url: pr_url.clone(),
                thread_id,
                thread_status,
                file_path,
                line_start,
                line_end,
                severity,
                coderabbit_comment: cr_text.join("\n\n"),
                fix_hunk: None, // filled by the bounded fix-exemplar pass below
            });
        }

        if let Some(cap) = max_prs
            && out.len() >= cap * 10
        {
            // Rough heuristic: average ~10 threads per PR with CR
            // comments in the pilot corpus; cap output so unbounded
            // fetches don't blow memory when callers forget max_prs.
            break;
        }
    }

    // Bounded fix-exemplar pass (iteration-delta mining): for RESOLVED
    // findings that quote code, recover the concrete house fix hunk from
    // the merged PR's iteration diffs. Ingest is a batch op, so the extra
    // ADO calls are acceptable; kept cheap by (a) resolved + tokenized
    // findings only, (b) per-PR iteration fetch cached, (c) blobs cached
    // by (commit, file), (d) fully fail-soft — a missing hunk just means
    // the rule ships without an exemplar.
    attach_fix_hunks(&client, &base, &auth, &mut out).await;

    Ok((out, skipped_incremental))
}

/// Per-PR map of `changes-API path → [blob objectId in iteration order]`
/// — every version of every file the PR touched. Built from each
/// iteration's `changes` (which carries item.path + item.objectId).
/// Empty on any failure. The ADO items-by-path API 404s here, and thread
/// paths carry a `/Site` prefix the changes paths lack, so objectId is
/// the reliable handle for fetching a specific version.
async fn fetch_pr_file_versions(
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    pr_id: u64,
) -> std::collections::HashMap<String, Vec<String>> {
    use reqwest::header::{ACCEPT, AUTHORIZATION};
    use std::collections::HashMap;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    // How many iterations?
    let iters_url = format!("{base}/pullrequests/{pr_id}/iterations?api-version=7.1");
    let Some(iter_ids): Option<Vec<u64>> = async {
        let body: serde_json::Value = client
            .get(&iters_url)
            .header(AUTHORIZATION, auth)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        Some(
            body.get("value")?
                .as_array()?
                .iter()
                .filter_map(|it| it.get("id").and_then(|v| v.as_u64()))
                .collect(),
        )
    }
    .await
    else {
        return out;
    };
    const MAX_ITERS: usize = 10;
    for it in iter_ids.into_iter().take(MAX_ITERS) {
        let ch_url =
            format!("{base}/pullrequests/{pr_id}/iterations/{it}/changes?api-version=7.1");
        let Ok(resp) = client
            .get(&ch_url)
            .header(AUTHORIZATION, auth)
            .header(ACCEPT, "application/json")
            .send()
            .await
        else {
            continue;
        };
        let Ok(resp) = resp.error_for_status() else {
            continue;
        };
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            continue;
        };
        let entries = body
            .get("changeEntries")
            .or_else(|| body.get("value"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for e in entries {
            let Some(item) = e.get("item") else { continue };
            let (Some(path), Some(oid)) = (
                item.get("path").and_then(|v| v.as_str()),
                item.get("objectId").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if oid.is_empty() {
                continue;
            }
            let versions = out.entry(path.to_string()).or_default();
            if versions.last().map(String::as_str) != Some(oid) {
                versions.push(oid.to_string());
            }
        }
    }
    out
}

/// Raw content of a blob by its objectId (the reliable ADO handle — no
/// path/version resolution). None on any failure or oversized blob.
async fn fetch_blob_by_object_id(
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    object_id: &str,
) -> Option<String> {
    use reqwest::header::{ACCEPT, AUTHORIZATION};
    let url = format!("{base}/blobs/{object_id}?api-version=7.1&$format=text");
    let resp = client
        .get(&url)
        .header(AUTHORIZATION, auth)
        .header(ACCEPT, "text/plain")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let text = resp.text().await.ok()?;
    if text.len() > 2_000_000 {
        return None;
    }
    Some(text)
}

/// A thread's file matches a changes-API path when one is a path-suffix
/// of the other (thread paths carry a `/Site` prefix the changes paths
/// drop). Compare on the last few segments to avoid basename collisions.
fn path_suffix_matches(thread_path: &str, changes_path: &str) -> bool {
    let a = thread_path.trim_start_matches('/').to_ascii_lowercase();
    let b = changes_path.trim_start_matches('/').to_ascii_lowercase();
    a.ends_with(&b) || b.ends_with(&a)
}

/// Fill `fix_hunk` on resolved, tokenized findings: diff the finding's
/// file between its EARLIEST and LATEST version in the PR (widest span;
/// token-anchoring finds the fix hunk regardless of intervening churn)
/// and take the hunk whose removed side carries a quoted token. Bounded
/// (resolved + tokenized only; per-PR version map cached; blobs cached by
/// objectId) and fully fail-soft.
async fn attach_fix_hunks(
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    findings: &mut [RawReviewComment],
) {
    use std::collections::HashMap;
    let mut versions_by_pr: HashMap<u64, HashMap<String, Vec<String>>> = HashMap::new();
    let mut blob_cache: HashMap<String, Option<String>> = HashMap::new();

    for f in findings.iter_mut() {
        let resolved = matches!(f.thread_status, ThreadStatus::Fixed | ThreadStatus::Closed);
        if !resolved || f.file_path.is_empty() {
            continue;
        }
        let tokens = quoted_code_tokens(&f.coderabbit_comment);
        if tokens.is_empty() {
            continue;
        }
        if !versions_by_pr.contains_key(&f.pr_id) {
            let v = fetch_pr_file_versions(client, base, auth, f.pr_id).await;
            versions_by_pr.insert(f.pr_id, v);
        }
        let file_versions = &versions_by_pr[&f.pr_id];
        // Find the changes-path whose file matches this finding's, take
        // its earliest and latest distinct objectIds.
        let Some(oids) = file_versions
            .iter()
            .find(|(p, _)| path_suffix_matches(&f.file_path, p))
            .map(|(_, v)| v)
        else {
            continue;
        };
        if oids.len() < 2 {
            continue; // file has one version in the PR → no before→after
        }
        let (first, last) = (&oids[0], &oids[oids.len() - 1]);
        for oid in [first, last] {
            if !blob_cache.contains_key(oid) {
                let v = fetch_blob_by_object_id(client, base, auth, oid).await;
                blob_cache.insert(oid.clone(), v);
            }
        }
        let (Some(old), Some(new)) = (
            blob_cache.get(first).cloned().flatten(),
            blob_cache.get(last).cloned().flatten(),
        ) else {
            continue;
        };
        if old == new {
            continue;
        }
        let diff = similar::TextDiff::from_lines(&old, &new)
            .unified_diff()
            .context_radius(2)
            .to_string();
        if let Some(hunk) = fix_hunk_by_token(&diff, &tokens) {
            f.fix_hunk = Some(hunk);
        }
    }
}

fn is_coderabbit_author(author: Option<&serde_json::Value>) -> bool {
    let Some(a) = author else { return false };
    let name = a
        .get("displayName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let email = a
        .get("uniqueName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.contains("coderabbit") || email.contains("coderabbit")
}

fn extract_severity_string(body: &str) -> String {
    // CodeRabbit writes `_⚠️ Potential issue_ | _🟠 Major_` or
    // `_🟡 Minor_` in the first few lines. Map those markers to the
    // severity strings the JSONL export already uses.
    for line in body.lines().take(8) {
        let l = line.to_ascii_lowercase();
        if l.contains("major") || l.contains("🟠") {
            return "major".into();
        }
        if l.contains("minor") || l.contains("🟡") {
            return "minor".into();
        }
        if l.contains("critical") || l.contains("🔴") {
            return "high".into();
        }
    }
    "low".into()
}

/// Minimal standard-alphabet base64 encoder. Only used for the Azure
/// DevOps `Basic` auth header — avoids a full `base64` crate dep.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut i = 0usize;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push_str("==");
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push('=');
    }
    out
}

fn url_escape(s: &str) -> String {
    // Extremely small URL-component escaper — azdo org/project/repo
    // names are alphanumeric + `-` / `_` / space in practice. Anything
    // weird just gets percent-encoded.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ─── Stage 2: parse ─────────────────────────────────────────────────────────

pub fn parse_comment(raw: &RawReviewComment) -> Option<ParsedRule> {
    // Reject walkthroughs / summaries / pre-merge checks — these have
    // no actionable finding. The scraper already filters most of these
    // via the file_path/line_start requirement, but be defensive.
    if raw.file_path.is_empty() || raw.line_start == 0 {
        return None;
    }
    let body_html = &raw.coderabbit_comment;
    if body_html.is_empty() {
        return None;
    }
    if body_html.contains("## Walkthrough")
        || body_html.contains("Estimated code review effort")
        || body_html.contains("## Changes")
        || body_html.contains("Pre-merge checks")
    {
        return None;
    }

    let body = strip_html(body_html);
    let rule_text = extract_bold_title(&body)?;
    // Require a rule text of reasonable size — otherwise we're
    // hallucinating from a noise comment.
    if rule_text.len() < 10 || rule_text.len() > 400 {
        return None;
    }
    let tokens = extract_pattern_tokens(&body);
    if tokens.len() < 2 {
        return None;
    }
    let fix_commit = extract_fix_commit(&body);
    // The ✅ Addressed in commits marker is the definitive fix
    // signal — promote to Fixed when present, regardless of Azure
    // DevOps' thread_status (which can be stale when CR's acknowledgment
    // was late or when a human pressed Resolved without the marker).
    let fix_status = effective_fix_status(raw.thread_status, fix_commit.as_deref());
    let severity = Severity::parse(&raw.severity);
    let language = infer_language(&raw.file_path);
    let file_pattern = derive_file_pattern(&raw.file_path);

    // Two hashes with distinct purposes:
    //
    // - `semantic_hash` = rule_text + tokens + file_pattern only.
    //   Identifies the *finding class*, not a specific PR. Used as
    //   the LLM classifier cache key so the classifier never spends
    //   tokens twice on the same finding pattern, even across PRs.
    //
    // - `content_hash` = semantic_hash + pr_id + thread_id. Unique
    //   per-thread identity; drives record-level dedup so the same
    //   thread re-pulled by a rerun collapses cleanly, but the same
    //   finding in different PRs stays distinct and contributes
    //   separate cluster members (which is what drives PR-count
    //   thresholds for repo-rule auto-promotion).
    let mut sem_bytes: Vec<u8> = Vec::with_capacity(rule_text.len() + tokens.len() * 8 + 16);
    sem_bytes.extend_from_slice(rule_text.as_bytes());
    sem_bytes.push(0);
    for t in &tokens {
        sem_bytes.extend_from_slice(t.as_bytes());
        sem_bytes.push(0);
    }
    sem_bytes.extend_from_slice(file_pattern.as_bytes());
    let semantic_hash = blake3::hash(&sem_bytes).to_hex()[..32].to_string();

    let mut content_bytes = sem_bytes;
    content_bytes.push(0);
    content_bytes.extend_from_slice(&raw.pr_id.to_le_bytes());
    content_bytes.extend_from_slice(&raw.thread_id.to_le_bytes());
    let content_hash = blake3::hash(&content_bytes).to_hex()[..32].to_string();

    Some(ParsedRule {
        rule_text,
        pattern_tokens: tokens,
        file_path: raw.file_path.clone(),
        file_pattern,
        language,
        severity,
        fix_status,
        fix_commit,
        llm_resolution: None,
        pr_id: raw.pr_id,
        pr_url: raw.pr_url.clone(),
        thread_id: raw.thread_id,
        pr_date: raw.pr_date.clone(),
        content_hash,
        semantic_hash,
        raw_body: body,
        fix_hunk: raw.fix_hunk.clone(),
    })
}

/// Classify ambiguous `closed` threads using the configured LLM
/// backend. Only called when `use_llm_for_ambiguous` is set. Caches
/// results under `cr_llm:<content_hash>` in the registry so repeat
/// runs never spend tokens twice on the same finding.
async fn classify_ambiguous(
    state: &AppState,
    project_id: &str,
    rule: &ParsedRule,
) -> Option<LlmResolution> {
    // Only ambiguous `closed` threads with no fix_commit qualify for
    // LLM inference. Explicit ✅ already promotes to Fixed; wontFix is
    // untouchable; active/unknown are left at their deterministic
    // weights.
    if rule.fix_commit.is_some() || !matches!(rule.fix_status, ThreadStatus::Closed) {
        return None;
    }

    // Cache key uses the SEMANTIC hash, not content_hash — the LLM's
    // answer depends on the finding content + file, not which PR
    // raised it. Same finding across 3 PRs = 1 classification call.
    let cache_key = format!("cr_llm:{}", rule.semantic_hash);
    if let Ok(Some(cached)) = state.registry.get_meta(project_id, &cache_key) {
        return parse_llm_resolution(&cached);
    }

    let prompt = format!(
        "You are analysing a code review thread.\n\
         \n\
         Thread status: closed (manually resolved by a developer)\n\
         File: {}\n\
         CodeRabbit finding: \"{}\"\n\
         No \"Addressed in commits\" message found in the thread.\n\
         \n\
         Did the developer likely:\n\
         A) Fix this issue (resolved after fixing)\n\
         B) Dismiss this issue (resolved without fixing, disagreed)\n\
         C) Cannot determine\n\
         \n\
         Reply with just A, B, or C.",
        rule.file_path, rule.rule_text
    );
    let text = match state
        .dreaming
        .generate_text(&prompt, 4, std::time::Duration::from_secs(10))
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                project_id,
                "cr_ingest LLM classify failed (deterministic fallback): {e}"
            );
            return None;
        }
    };
    let verdict = parse_llm_single_letter(&text);
    if let Some(v) = verdict {
        // Cache even Unknown — no point re-asking.
        let _ = state
            .registry
            .set_meta(project_id, &cache_key, llm_resolution_str(v));
        Some(v)
    } else {
        None
    }
}

fn parse_llm_single_letter(text: &str) -> Option<LlmResolution> {
    // Scan for the FIRST isolated A / B / C — an alphabetic character
    // that is immediately preceded by a non-alphabetic character (or
    // string start) and followed by a non-alphanumeric character (or
    // string end). This handles real LLM responses like `"A"`,
    // `"Option A"`, `"The answer is A."`, `" A) fix"`, while rejecting
    // stray alphabetics embedded inside longer words (`"Apple"`
    // mustn't return Fixed).
    let bytes = text.as_bytes();
    for (i, ch) in text.char_indices() {
        let up = ch.to_ascii_uppercase();
        if !matches!(up, 'A' | 'B' | 'C') {
            continue;
        }
        // Preceding char must NOT be alphabetic.
        let before_ok = i == 0
            || text[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphabetic());
        // Following char must NOT be alphanumeric.
        let next_idx = i + ch.len_utf8();
        let after_ok = next_idx >= bytes.len()
            || text[next_idx..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return Some(match up {
                'A' => LlmResolution::Fixed,
                'B' => LlmResolution::Dismissed,
                _ => LlmResolution::Unknown,
            });
        }
    }
    None
}

fn parse_llm_resolution(s: &str) -> Option<LlmResolution> {
    match s.trim() {
        "fixed" => Some(LlmResolution::Fixed),
        "dismissed" => Some(LlmResolution::Dismissed),
        "unknown" => Some(LlmResolution::Unknown),
        _ => None,
    }
}

fn llm_resolution_str(r: LlmResolution) -> &'static str {
    match r {
        LlmResolution::Fixed => "fixed",
        LlmResolution::Dismissed => "dismissed",
        LlmResolution::Unknown => "unknown",
    }
}

/// Combined weight — accounts for the definitive `✅ Addressed in
/// commits` signal (cap 1.0) vs LLM-inferred fix (cap 0.85) vs the raw
/// thread status (0.6 for closed, etc). The cap differential is
/// deliberate: `✅` is commit evidence, LLM is inference without
/// seeing the diff.
fn parsed_rule_weight(r: &ParsedRule) -> f32 {
    if r.fix_commit.is_some() {
        // Explicit fix evidence, highest possible weight.
        return 1.0;
    }
    match r.llm_resolution {
        Some(LlmResolution::Fixed) => 0.85,
        Some(LlmResolution::Dismissed) => 0.0, // Treated as suppression.
        Some(LlmResolution::Unknown) => r.fix_status.weight(),
        None => r.fix_status.weight(),
    }
}

/// Which sink a parsed rule lands in — positive antipattern index vs
/// wontfix suppression namespace. LLM-inferred `Dismissed` joins the
/// explicit wontFix pile; everything else is positive (modulo the
/// positive cluster's fix_rate threshold later in the pipeline).
fn is_suppression(r: &ParsedRule) -> bool {
    matches!(r.fix_status, ThreadStatus::WontFix)
        || matches!(r.llm_resolution, Some(LlmResolution::Dismissed))
}

pub fn strip_html(s: &str) -> String {
    // Remove <script> / <style> blocks in full (so their contents don't
    // bleed through), then strip remaining tags, then decode common
    // entities. Deliberately minimal — CodeRabbit bodies are 99%
    // markdown, so the HTML path exists only to deal with the occasional
    // inline HTML in pre-rendered comments.
    use std::sync::LazyLock;
    static BLOCK_RE: LazyLock<Option<regex::Regex>> = LazyLock::new(|| {
        regex::Regex::new(r"(?is)<(?:script|style|head)[^>]*>.*?</(?:script|style|head)>").ok()
    });
    // CodeRabbit folds its INTERNAL tooling transcript into collapsible
    // blocks — `<details><summary>🧩 Analysis chain</summary>🏁 Script
    // executed: #!/bin/bash …`. That text is not part of the finding;
    // ingested, it polluted rule texts and token clouds (live: PR1874
    // residual entries were analysis-chain shell scripts). Drop the whole
    // block; the finding's bold title always sits OUTSIDE it.
    static DETAILS_RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"(?is)<details>.*?(?:</details>|\z)").ok());
    static TAG_RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"<[^>]+>").ok());

    let stripped = BLOCK_RE
        .as_ref()
        .map(|re| re.replace_all(s, "").to_string())
        .unwrap_or_else(|| s.to_string());
    let stripped = DETAILS_RE
        .as_ref()
        .map(|re| re.replace_all(&stripped, "").to_string())
        .unwrap_or(stripped);
    let tagless = TAG_RE
        .as_ref()
        .map(|re| re.replace_all(&stripped, "").to_string())
        .unwrap_or(stripped);

    // Entity decode — only the common ones.
    tagless
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn extract_bold_title(body: &str) -> Option<String> {
    // CodeRabbit's real findings always open with a one-line bold
    // title on its own line — `**…**`. Walkthrough summaries instead
    // use `## Walkthrough` (rejected earlier). We scan the first ~15
    // lines to tolerate the severity-marker prefix line.
    use std::sync::LazyLock;
    static BOLD_RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"\*\*([^*\n][^*]*[^*\n]|[^*\n])\*\*").ok());
    let re = BOLD_RE.as_ref()?;
    for (i, line) in body.lines().enumerate() {
        if i > 15 {
            break;
        }
        if let Some(caps) = re.captures(line) {
            let t = caps.get(1)?.as_str().trim();
            if !t.is_empty() && !t.contains("Potential issue") && !t.contains("Nitpick") {
                return Some(strip_markdown_inline(t));
            }
        }
    }
    None
}

fn strip_markdown_inline(s: &str) -> String {
    s.replace('`', "").replace("\\_", "_").trim().to_string()
}

/// Small stop-word list for pattern token filtering. These are tokens
/// that look like identifiers but appear in almost every review and so
/// carry no signal (they'd over-cluster unrelated rules). Not a full
/// dictionary — just the tokens our test corpus shows as persistent
/// noise.
fn is_stop_token(t: &str) -> bool {
    matches!(
        t,
        "This"
            | "That"
            | "These"
            | "Those"
            | "The"
            | "When"
            | "Then"
            | "Else"
            | "If"
            | "For"
            | "While"
            | "Return"
            | "Call"
            | "Code"
            | "File"
            | "Files"
            | "Line"
            | "Method"
            | "Class"
            | "Function"
            | "Parameter"
            | "Value"
            | "Object"
            | "Array"
            | "String"
            | "Number"
            | "Boolean"
            | "Error"
            | "Note"
            | "Ensure"
            | "Consider"
            | "Prompt"
            | "Agents"
    )
}

fn extract_pattern_tokens(body: &str) -> Vec<String> {
    use std::sync::LazyLock;
    // Backtick-quoted identifiers, PascalCase words, and method calls
    // (word followed by `(`) are the three token sources. PascalCase
    // requires at least one inner lowercase letter so we don't flag
    // SCREAMING_CASE constants; we cover those via the backtick path
    // when the author marked them.
    static BACKTICK_RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"`([A-Za-z_][A-Za-z0-9_\.]{2,})(?:\(\))?`").ok());
    static PASCAL_RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"\b([A-Z][a-z]+[A-Za-z0-9]{2,})\b").ok());
    // camelCase with an interior hump ("gQtyManager", "setWarningsText") —
    // English prose never has interior caps, so this shape needs no
    // prose filter.
    static CAMEL_RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"\b([a-z_][a-z0-9_]*[A-Z][A-Za-z0-9_]+)\b").ok());
    static METHOD_RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]{2,})\s*\(").ok());

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (re, is_pascal) in [
        (BACKTICK_RE.as_ref(), false),
        (PASCAL_RE.as_ref(), true),
        (CAMEL_RE.as_ref(), false),
        (METHOD_RE.as_ref(), false),
    ] {
        let Some(re) = re else { continue };
        for cap in re.captures_iter(body).take(200) {
            if let Some(m) = cap.get(1) {
                let tok = m.as_str().trim_matches('.').to_string();
                if tok.len() < 4 || is_stop_token(&tok) {
                    continue;
                }
                // The PascalCase path alone also matches plain Titlecase
                // ENGLISH words ("Thanks", "Carefully", "Understood") —
                // review-comment courtesy prose that polluted the rule
                // docs' token clouds and degraded rule search (live: the
                // writing-time rules section is matched lexically against
                // story text). Require a real identifier signal: an
                // interior uppercase hump, digit, or underscore.
                // Backtick tokens are author-marked code and call-shaped
                // tokens are code by construction — those paths stay open.
                if is_pascal
                    && !tok[1..]
                        .chars()
                        .any(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                {
                    continue;
                }
                if seen.insert(tok.clone()) {
                    out.push(tok);
                }
            }
        }
    }
    out
}

fn extract_fix_commit(body: &str) -> Option<String> {
    use std::sync::LazyLock;
    // `✅ Addressed in commits <sha>[ to <sha>]`
    static RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"Addressed in commits?\s+([a-f0-9]{7,40})").ok());
    let re = RE.as_ref()?;
    re.captures(body)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

// ─── Fix-exemplar localization (iteration-delta mining) ──────────────────────
//
// A review finding quotes the offending code in backticks; the merged PR's
// later-iteration diff of the same file contains the concrete fix. Matching
// the fix hunk by that quoted token (not by line number, which drifts across
// iterations) recovers the canonical before→after — the highest-quality
// signal for the rule corpus ("here's how the team fixed this last time").
//
// Proven in eval/_iter_delta_probe.py: token-anchoring ~3x'd the hit rate on
// live PRs (PR1874 4/17 → 11/17). These are the in-tree, unit-tested core;
// the live ADO iteration-diff fetch that feeds them is wired separately.

/// Backtick-quoted identifier-ish tokens the reviewer named, longest first —
/// a line-number-independent anchor for the offending code.
pub(crate) fn quoted_code_tokens(finding_text: &str) -> Vec<String> {
    use std::sync::LazyLock;
    static RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"`([^`]{3,60})`").ok());
    static IDENT: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"[A-Za-z_]\w*").ok());
    let (Some(re), Some(ident)) = (RE.as_ref(), IDENT.as_ref()) else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for cap in re.captures_iter(finding_text) {
        if let Some(m) = cap.get(1) {
            let t = m.as_str().to_string();
            if ident.is_match(&t) && seen.insert(t.clone()) {
                out.push(t);
            }
        }
    }
    out.sort_by(|a, b| b.len().cmp(&a.len()));
    out
}

/// From a unified diff of ONE file, return the first `@@` hunk whose
/// REMOVED (`-`) side contains one of `tokens` — the hunk that changed the
/// offending code, regardless of how line numbers drifted. None if no
/// removed line carries a named token (a fix that only ADDS lines, or an
/// unrelated hunk).
pub(crate) fn fix_hunk_by_token(diff_text: &str, tokens: &[String]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let has_token = |removed: &[&str]| -> bool {
        removed.iter().any(|r| tokens.iter().any(|t| r.contains(t.as_str())))
    };
    let mut cur: Vec<&str> = Vec::new();
    let mut removed: Vec<&str> = Vec::new();
    let mut started = false;
    for line in diff_text.lines() {
        if line.starts_with("@@ ") {
            if started && has_token(&removed) {
                return Some(cur.join("\n"));
            }
            cur.clear();
            removed.clear();
            cur.push(line);
            started = true;
        } else if started {
            cur.push(line);
            if line.starts_with('-') && !line.starts_with("---") {
                removed.push(line);
            }
        }
    }
    if started && has_token(&removed) {
        return Some(cur.join("\n"));
    }
    None
}

pub fn infer_language(file_path: &str) -> String {
    let lower = file_path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "rust".into(),
        "py" => "python".into(),
        "ts" | "tsx" => "typescript".into(),
        "js" | "jsx" | "mjs" => "javascript".into(),
        "vb" => "vbnet".into(),
        "cs" => "csharp".into(),
        "go" => "go".into(),
        "java" => "java".into(),
        "sql" => "sql".into(),
        "aspx" | "ascx" | "master" => "webforms".into(),
        "asp" => "classic-asp".into(),
        "c" | "h" => "c".into(),
        "cpp" | "cc" | "hpp" | "hh" => "cpp".into(),
        _ => "unknown".into(),
    }
}

fn derive_file_pattern(file_path: &str) -> String {
    let lower = file_path.to_ascii_lowercase();
    // Keep the first directory segment + the extension — tight enough to
    // scope suppression (e.g. `Site/ts/qty/qtyManager.ts` → `Site/ts/qty/**/*.ts`),
    // loose enough to cover the generated JS mirrors and sibling files
    // in the same feature folder.
    let dir = match lower.rfind('/') {
        Some(i) => &lower[..i],
        None => "",
    };
    let filename = match lower.rfind('/') {
        Some(i) => &lower[i + 1..],
        None => &lower[..],
    };
    // Only derive an `*.ext` pattern when the filename actually has
    // an extension. Extensionless names (`Makefile`, `Dockerfile`,
    // `Rakefile`) get a path-only pattern — matching them on sibling
    // extensionless files only, which is the right scoping.
    let ext = filename
        .rfind('.')
        .filter(|i| *i > 0 && *i < filename.len() - 1)
        .map(|i| &filename[i + 1..]);
    match (dir.is_empty(), ext) {
        (true, Some(e)) => format!("**/*.{e}"),
        (false, Some(e)) => format!("{dir}/**/*.{e}"),
        (true, None) => format!("**/{filename}"),
        (false, None) => format!("{dir}/**/{filename}"),
    }
}

// ─── Stage 3: cluster ───────────────────────────────────────────────────────

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    inter as f32 / union as f32
}

fn composite_score(r: &ParsedRule) -> f32 {
    parsed_rule_weight(r) + r.severity.weight() * 0.5
}

fn cluster_rules(rules: Vec<ParsedRule>, overlap_threshold: f32) -> Vec<ReviewCluster> {
    if rules.is_empty() {
        return Vec::new();
    }

    // Jaccard requires same-language. Partition rules by language up
    // front so the O(n²) inner loop only compares within a language —
    // on a multi-language corpus that cuts the work by the square of
    // the per-language fraction (e.g. 5 evenly-sized languages → 5×
    // fewer total pair comparisons).
    let mut by_language: std::collections::HashMap<String, Vec<ParsedRule>> =
        std::collections::HashMap::new();
    for r in rules {
        by_language.entry(r.language.clone()).or_default().push(r);
    }

    let mut out: Vec<ReviewCluster> = Vec::new();
    for (_lang, bucket) in by_language {
        let n = bucket.len();
        // Precompute HashSets once per rule, reused across the n²
        // Jaccard comparisons inside this language bucket.
        let token_sets: Vec<HashSet<String>> = bucket
            .iter()
            .map(|r| r.pattern_tokens.iter().cloned().collect::<HashSet<_>>())
            .collect();
        let mut cluster_of: Vec<Option<usize>> = vec![None; n];
        let mut cluster_indices: Vec<Vec<usize>> = Vec::new();
        for i in 0..n {
            if cluster_of[i].is_some() {
                continue;
            }
            let mut new_cluster: Vec<usize> = vec![i];
            cluster_of[i] = Some(cluster_indices.len());
            for j in (i + 1)..n {
                if cluster_of[j].is_some() {
                    continue;
                }
                let sim = jaccard(&token_sets[i], &token_sets[j]);
                if sim >= overlap_threshold {
                    cluster_of[j] = Some(cluster_indices.len());
                    new_cluster.push(j);
                }
            }
            cluster_indices.push(new_cluster);
        }
        for idxs in cluster_indices {
            let members: Vec<ParsedRule> = idxs.iter().map(|i| bucket[*i].clone()).collect();
            out.push(build_cluster(members));
        }
    }
    out
}

fn build_cluster(members: Vec<ParsedRule>) -> ReviewCluster {
    let canonical = members
        .iter()
        .max_by(|a, b| {
            composite_score(a)
                .partial_cmp(&composite_score(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
        .expect("cluster must have at least one member");

    // Fix-rate math counts LLM-inferred verdicts alongside the
    // deterministic thread statuses:
    //
    //   fixed  = ThreadStatus::Fixed  ∪  llm_resolution::Fixed
    //          ∪  (fix_commit present AND not WontFix)
    //   wont   = ThreadStatus::WontFix  ∪  llm_resolution::Dismissed
    //
    // This matches the pipeline's partitioning rule (is_suppression)
    // so the fix_rate you see here is the fix_rate that actually
    // gates the sink-filter decision downstream.
    let fixed = members
        .iter()
        .filter(|m| {
            matches!(m.fix_status, ThreadStatus::Fixed)
                || matches!(m.llm_resolution, Some(LlmResolution::Fixed))
        })
        .count();
    let wont = members.iter().filter(|m| is_suppression(m)).count();
    let decisive = fixed + wont;
    let fix_rate = if decisive == 0 {
        // No fixed / wontFix signal — cluster is all Active or
        // ambiguous Closed with no LLM verdict. Neither positive nor
        // suppression: give it a nominal 0.5 so the default
        // `min_fix_rate=0.5` drops it into the positive-index path
        // only if the caller explicitly lowered the threshold.
        0.5
    } else {
        fixed as f32 / decisive as f32
    };

    let mut pr_ids: Vec<u64> = members.iter().map(|m| m.pr_id).collect();
    pr_ids.sort_unstable();
    pr_ids.dedup();

    let mut file_paths: Vec<String> = members.iter().map(|m| m.file_path.clone()).collect();
    file_paths.sort();
    file_paths.dedup();

    let mut file_patterns: Vec<String> = members.iter().map(|m| m.file_pattern.clone()).collect();
    file_patterns.sort();
    file_patterns.dedup();

    // Cluster-wide confidence: blend fix rate, PR-count saturation, and
    // average severity. Capped at 1.0.
    let pr_sat = (pr_ids.len() as f32 / 5.0).min(1.0);
    let avg_sev: f32 =
        members.iter().map(|m| m.severity.weight()).sum::<f32>() / members.len() as f32;
    let confidence = (0.55 * fix_rate + 0.30 * pr_sat + 0.15 * avg_sev).clamp(0.0, 1.0);

    // Deterministic cluster id from canonical token set — stable across runs.
    let mut tokens_sorted: Vec<String> = canonical.pattern_tokens.clone();
    tokens_sorted.sort();
    let token_blob = tokens_sorted.join("|");
    let cluster_id = blake3::hash(token_blob.as_bytes()).to_hex().to_string()[..16].to_string();

    ReviewCluster {
        cluster_id,
        canonical,
        members,
        fix_rate,
        confidence,
        pr_ids,
        file_paths,
        file_patterns,
    }
}

// ─── Stage 4: storage ───────────────────────────────────────────────────────

fn cluster_index_body(c: &ReviewCluster) -> String {
    // Keep body bounded — a 1000-member cluster with 20 tokens each
    // and 3-digit PR IDs could otherwise produce tens of KB per doc,
    // which inflates the Tantivy index and blows the search snippet
    // budget. Cap tokens at 128 and PR references at 64, both sorted
    // so the cap is deterministic.
    const MAX_TOKENS: usize = 128;
    const MAX_PR_REFS: usize = 64;

    let mut body = String::with_capacity(c.canonical.rule_text.len() + 512);
    body.push_str(&c.canonical.rule_text);
    body.push_str("\n\n");
    let mut all_tokens: HashSet<String> = HashSet::new();
    for m in &c.members {
        for t in &m.pattern_tokens {
            all_tokens.insert(t.clone());
        }
    }
    let mut toks: Vec<String> = all_tokens.into_iter().collect();
    toks.sort();
    toks.truncate(MAX_TOKENS);
    body.push_str("Tokens: ");
    body.push_str(&toks.join(", "));

    body.push_str("\n\nPR references: ");
    let pr_refs: Vec<String> = c
        .pr_ids
        .iter()
        .take(MAX_PR_REFS)
        .map(|i| format!("#{i}"))
        .collect();
    body.push_str(&pr_refs.join(", "));
    if c.pr_ids.len() > MAX_PR_REFS {
        body.push_str(&format!(" (+{} more)", c.pr_ids.len() - MAX_PR_REFS));
    }

    if let Some(fc) = &c.canonical.fix_commit {
        body.push_str(&format!("\nFix commit: {fc}"));
    }
    body.push_str(&format!(
        "\nFix rate: {:.0}%  |  Severity: {:?}  |  Language: {}",
        c.fix_rate * 100.0,
        c.canonical.severity,
        c.canonical.language
    ));

    // Fix exemplar: the concrete before→after the team applied last time,
    // token-anchored from a merged-PR later iteration. Prefer the
    // canonical member's; fall back to any member that has one. Bounded
    // so a huge hunk can't blow the index/snippet budget. This is what
    // upgrades a rule from "avoid X" to "avoid X — here's the house fix".
    let exemplar = c
        .canonical
        .fix_hunk
        .as_deref()
        .or_else(|| c.members.iter().find_map(|m| m.fix_hunk.as_deref()));
    if let Some(hunk) = exemplar {
        const MAX_HUNK: usize = 1200;
        let trimmed: String = hunk.chars().take(MAX_HUNK).collect();
        body.push_str("\n\nHouse fix (applied in a merged PR):\n");
        body.push_str(&trimmed);
    }
    body
}

fn build_index_docs(clusters: &[ReviewCluster], namespace: &str, generation: u64) -> Vec<IndexDoc> {
    let mut out: Vec<IndexDoc> = Vec::with_capacity(clusters.len());
    for c in clusters {
        let content = cluster_index_body(c);
        let ch = ContentHash::compute(content.as_bytes());
        // Use a stable synthetic path — the file pattern — so DocStore
        // dedup keys off cluster identity rather than physical file.
        let path = RelPath::new(
            c.file_patterns
                .first()
                .map(String::as_str)
                .unwrap_or("coderabbit://cluster"),
        );
        let doc_id = DocIdStr::compute(path.as_str(), 0, 0, &ch).0;
        out.push(IndexDoc {
            generation,
            chunk_id: chunk_id_from_content_hash(&ch),
            doc_id,
            content_hash: ch.0,
            path,
            language: c.canonical.language.clone(),
            content,
            namespace: namespace.into(),
            author: Some("coderabbit".into()),
            timestamp: None,
            start_line: 0,
            end_line: 0,
        });
    }
    out
}

fn build_graph_entities(
    positive: &[ReviewCluster],
    suppression: &[ReviewCluster],
    generation: u64,
) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let ts = now_ms();

    for (label, clusters) in [("pattern", positive), ("suppression", suppression)] {
        for c in clusters {
            let node_id = format!("review_{label}:{}", c.cluster_id);
            nodes.push(Node {
                node_id: node_id.clone(),
                node_type: "review_pattern".into(),
                name: c.canonical.rule_text.clone(),
                namespace: "antipattern".into(),
                language: c.canonical.language.clone(),
                file_path: RelPath::new(
                    c.file_patterns
                        .first()
                        .map(String::as_str)
                        .unwrap_or("coderabbit"),
                ),
                start_line: 0,
                end_line: 0,
                generation,
                metadata: Some(cluster_metadata_value(c, label)),
            });

            for fp in &c.file_paths {
                if fp.is_empty() {
                    continue;
                }
                let tgt = format!("file:{}", fp.trim_start_matches('/'));
                edges.push(Edge {
                    source_id: node_id.clone(),
                    target_id: tgt,
                    namespace: "antipattern".into(),
                    language: c.canonical.language.clone(),
                    edge_kind: EdgeKind::AntiPattern,
                    weight: (c.confidence * 100.0) as u32,
                    generation,
                    metadata: None,
                    updated_at_ms: ts,
                });
            }
        }
    }

    (nodes, edges)
}

fn cluster_metadata_value(c: &ReviewCluster, kind: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "source".into(),
        serde_json::Value::String("coderabbit".into()),
    );
    map.insert("kind".into(), serde_json::Value::String(kind.into()));
    map.insert(
        "cluster_id".into(),
        serde_json::Value::String(c.cluster_id.clone()),
    );
    map.insert(
        "fix_rate".into(),
        serde_json::Value::from(round_half(c.fix_rate as f64, 3)),
    );
    map.insert(
        "confidence".into(),
        serde_json::Value::from(round_half(c.confidence as f64, 3)),
    );
    map.insert(
        "pr_ids".into(),
        serde_json::Value::Array(
            c.pr_ids
                .iter()
                .map(|i| serde_json::Value::from(*i))
                .collect(),
        ),
    );
    map.insert(
        "file_patterns".into(),
        serde_json::Value::Array(
            c.file_patterns
                .iter()
                .map(|p| serde_json::Value::String(p.clone()))
                .collect(),
        ),
    );
    if let Some(fc) = &c.canonical.fix_commit {
        map.insert("fix_commit".into(), serde_json::Value::String(fc.clone()));
    }
    serde_json::Value::Object(map)
}

fn round_half(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

// ─── Graph store helper (re-export for convenience) ─────────────────────────

#[allow(dead_code)]
fn _hint_graph_api(graph: &GraphStore) {
    let _ = graph;
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_status_parses_common_strings() {
        assert_eq!(ThreadStatus::parse("fixed"), ThreadStatus::Fixed);
        assert_eq!(ThreadStatus::parse("wontFix"), ThreadStatus::WontFix);
        assert_eq!(ThreadStatus::parse("won't fix"), ThreadStatus::WontFix);
        assert_eq!(ThreadStatus::parse("active"), ThreadStatus::Active);
        assert_eq!(ThreadStatus::parse("closed"), ThreadStatus::Closed);
        assert_eq!(ThreadStatus::parse("garbage"), ThreadStatus::Unknown);
    }

    #[test]
    fn severity_maps_coderabbit_labels() {
        assert_eq!(Severity::parse("major"), Severity::Critical);
        assert_eq!(Severity::parse("medium"), Severity::Warning);
        assert_eq!(Severity::parse("minor"), Severity::Info);
    }

    #[test]
    fn path_suffix_matches_handles_site_prefix() {
        // Thread paths carry /Site; changes-API paths drop it.
        assert!(path_suffix_matches(
            "/Site/App_Code/api-v2/WebApiConfig.vb",
            "/App_Code/api-v2/WebApiConfig.vb"
        ));
        // Identical.
        assert!(path_suffix_matches("/a/b/c.vb", "/a/b/c.vb"));
        // Different files must not match.
        assert!(!path_suffix_matches(
            "/Site/App_Code/Foo.vb",
            "/App_Code/Bar.vb"
        ));
        // Bare-basename collision across dirs still matches on the tail —
        // acceptable: the diff+token-anchor is the real filter.
        assert!(path_suffix_matches("/x/Config.vb", "/Config.vb"));
    }

    #[test]
    fn cluster_index_body_renders_house_fix_exemplar() {
        let raw = RawReviewComment {
            pr_id: 1,
            pr_title: String::new(),
            pr_author: String::new(),
            pr_date: String::new(),
            pr_branch: String::new(),
            pr_url: String::new(),
            thread_id: 1,
            thread_status: ThreadStatus::Fixed,
            file_path: "/f.vb".into(),
            line_start: 1,
            line_end: 1,
            severity: "major".into(),
            coderabbit_comment: "_🟠 Major_\n\n**Guard `Query.projectId` before calling \
                `Check_pr_id`.** Use `HasValue` safely.".into(),
            fix_hunk: Some(
                "@@ -1,3 +1,3 @@\n-If Query.projectId.HasValue Then\n+If Query?.projectId IsNot Nothing Then"
                    .into(),
            ),
        };
        let rule = parse_comment(&raw).expect("parses");
        assert_eq!(rule.fix_hunk.as_deref().map(|h| h.contains("IsNot Nothing")), Some(true));
        let cluster = build_cluster(vec![rule]);
        let body = cluster_index_body(&cluster);
        assert!(body.contains("House fix (applied in a merged PR):"), "body:\n{body}");
        assert!(body.contains("If Query?.projectId IsNot Nothing Then"));
    }

    #[test]
    fn strip_html_removes_tags_and_scripts() {
        let input = "<p>hello <script>alert(1)</script>world</p>";
        assert_eq!(strip_html(input), "hello world");
    }

    #[test]
    fn strip_html_decodes_entities() {
        assert_eq!(strip_html("a &amp; b &lt; c"), "a & b < c");
    }

    #[test]
    fn extract_bold_title_pulls_first_non_meta_bold() {
        let body = "_⚠️ Potential issue_ | _🟠 Major_\n\n**Move the PdfExtGState cache to instance scope**\n\nbody body";
        assert_eq!(
            extract_bold_title(body),
            Some("Move the PdfExtGState cache to instance scope".into())
        );
    }

    #[test]
    fn extract_pattern_tokens_picks_backticks_and_pascal() {
        let body = "The static `_fillOpacityStates` cache collides across `PdfDocument` instances when WriteToDisk() runs.";
        let tokens = extract_pattern_tokens(body);
        assert!(
            tokens.iter().any(|t| t == "_fillOpacityStates"),
            "tokens={tokens:?}"
        );
        assert!(tokens.iter().any(|t| t == "PdfDocument"));
        assert!(tokens.iter().any(|t| t == "WriteToDisk"));
    }

    #[test]
    fn extract_pattern_tokens_drops_stopwords() {
        let body = "The method Method() returns a String for This class.";
        let tokens = extract_pattern_tokens(body);
        for t in &tokens {
            assert!(!is_stop_token(t), "leaked stopword: {t}");
        }
    }

    #[test]
    fn extract_pattern_tokens_drops_titlecase_prose() {
        // Courtesy/reply prose from review threads must not become
        // pattern tokens (live: "Thanks, Understood, Learnt, Carefully"
        // in rule docs' token clouds). Identifier-shaped PascalCase and
        // explicit code markers still pass.
        let body = "Thanks! Understood. Therefore we should Carefully check \
                    NullReferenceException in `spGetChildRecords` when GetByID() runs.";
        let tokens = extract_pattern_tokens(body);
        for prose in ["Thanks", "Understood", "Therefore", "Carefully"] {
            assert!(
                !tokens.iter().any(|t| t == prose),
                "prose token leaked: {prose} in {tokens:?}"
            );
        }
        assert!(tokens.iter().any(|t| t == "NullReferenceException"));
        assert!(tokens.iter().any(|t| t == "spGetChildRecords"));
        assert!(tokens.iter().any(|t| t == "GetByID"));
    }

    #[test]
    fn strip_html_drops_details_analysis_chains() {
        let body = "**Missing null check on selectedRow.**\n\nThe row lookup can return Nothing.\n\
                    <details><summary>🧩 Analysis chain</summary>\n🏁 Script executed: \
                    #!/bin/bash\nfind . -name '*.vb' | xargs grep -l selectedRow</details>\n\
                    Fix by guarding the lookup.";
        let out = strip_html(body);
        assert!(out.contains("Missing null check"));
        assert!(out.contains("Fix by guarding"));
        assert!(!out.contains("Script executed"), "analysis chain leaked: {out}");
        assert!(!out.contains("bin/bash"));
        // Unterminated details (truncated comment) must also be dropped.
        let out2 = strip_html("**Title.**\n<details><summary>chain</summary>partial…");
        assert!(!out2.contains("partial"));
        assert!(out2.contains("Title."));
    }

    #[test]
    fn extract_fix_commit_grabs_sha() {
        let body = "✅ Addressed in commits 8133c13 to dde4b4e";
        assert_eq!(extract_fix_commit(body), Some("8133c13".to_string()));
    }

    #[test]
    fn quoted_code_tokens_ranks_identifiers_longest_first() {
        let finding = "**Guard `Query` before dereferencing** — call `Check_pr_id` and \
                       avoid `x`.";
        let toks = quoted_code_tokens(finding);
        // 'x' is <3 chars → dropped; identifiers kept, longest first.
        assert_eq!(toks, vec!["Check_pr_id".to_string(), "Query".to_string()]);
    }

    #[test]
    fn fix_hunk_by_token_locates_the_removed_offending_code() {
        // The literal PR1874 fix the probe recovered: nullable-guard change.
        let diff = "@@ -18,7 +18,7 @@ Namespace _api2.svc\n\
                     \n\
                                     Using db As New iFaltDataContext\n\
                     \n\
                     -                If Query.projectId.HasValue Then\n\
                     +                If Query?.projectId IsNot Nothing Then\n\
                                          Return Nothing\n\
                     @@ -39,7 +39,7 @@ another region\n\
                     -                Dim x = 1\n\
                     +                Dim x = 2\n";
        let toks = quoted_code_tokens("Guard `Query.projectId.HasValue` before use");
        let hunk = fix_hunk_by_token(diff, &toks).expect("should locate the guard hunk");
        assert!(hunk.contains("If Query.projectId.HasValue Then"));
        assert!(hunk.contains("If Query?.projectId IsNot Nothing Then"));
        // Must pick the FIRST token-bearing hunk, not the unrelated x=1 one.
        assert!(!hunk.contains("Dim x = 1"));
        // A token nobody removed → no match.
        assert!(fix_hunk_by_token(diff, &["nonexistent_symbol".to_string()]).is_none());
    }

    #[test]
    fn derive_file_pattern_scopes_to_dir_and_ext() {
        assert_eq!(
            derive_file_pattern("/Site/ts/qty/qtyManager.ts"),
            "/site/ts/qty/**/*.ts"
        );
        assert_eq!(derive_file_pattern("foo.vb"), "**/*.vb");
    }

    #[test]
    fn parse_comment_rejects_walkthrough() {
        let raw = RawReviewComment {
            pr_id: 1,
            pr_title: "".into(),
            pr_author: "".into(),
            pr_date: "".into(),
            pr_branch: "".into(),
            pr_url: "".into(),
            thread_id: 1,
            thread_status: ThreadStatus::Closed,
            file_path: "".into(),
            line_start: 0,
            line_end: 0,
            severity: "".into(),
            coderabbit_comment: "## Walkthrough\n\nSome summary".into(),
            fix_hunk: None,
        };
        assert!(parse_comment(&raw).is_none());
    }

    #[test]
    fn parse_comment_builds_parsed_rule_from_real_format() {
        let raw = RawReviewComment {
            pr_id: 1905,
            pr_title: "".into(),
            pr_author: "".into(),
            pr_date: "2026-04-01".into(),
            pr_branch: "".into(),
            pr_url: "https://dev.azure.com/x".into(),
            thread_id: 26984,
            thread_status: ThreadStatus::Fixed,
            file_path: "/Site/ts/qty/qtyManager.ts".into(),
            line_start: 733,
            line_end: 735,
            severity: "minor".into(),
            coderabbit_comment: "_⚠️ Potential issue_ | _🟡 Minor_\n\n**Avoid clearing \
                other warnings from the shared banner.**\n\nBecause `setWarningsText()` is \
                also used for other popup warnings in this dialog, the `else` branch here \
                can wipe an existing IO-marker warning after a company switch.\n\n\
                ✅ Addressed in commits eb16a30 to dde4b4e"
                .into(),
            fix_hunk: None,
        };
        let parsed = parse_comment(&raw).expect("parse must succeed");
        assert!(parsed.rule_text.contains("Avoid clearing"));
        assert!(parsed.pattern_tokens.iter().any(|t| t == "setWarningsText"));
        assert_eq!(parsed.fix_commit.as_deref(), Some("eb16a30"));
        assert_eq!(parsed.fix_status, ThreadStatus::Fixed);
        assert_eq!(parsed.language, "typescript");
        assert_eq!(parsed.severity, Severity::Info);
    }

    #[test]
    fn cluster_rules_groups_same_pattern_across_prs() {
        let make = |pr: u64, tokens: &[&str]| ParsedRule {
            rule_text: "Move cache".into(),
            pattern_tokens: tokens.iter().map(|s| s.to_string()).collect(),
            file_path: "/Site/Export.vb".into(),
            file_pattern: "/site/**/*.vb".into(),
            language: "vbnet".into(),
            severity: Severity::Critical,
            fix_status: ThreadStatus::Fixed,
            fix_commit: None,
            llm_resolution: None,
            pr_id: pr,
            pr_url: "".into(),
            thread_id: 0,
            pr_date: "".into(),
            content_hash: format!("{pr}"),
            semantic_hash: "s".into(),
            raw_body: "".into(),
            fix_hunk: None,
        };
        let rules = vec![
            make(1, &["PdfExtGState", "PdfDocument", "WriteToDisk"]),
            make(2, &["PdfExtGState", "PdfDocument", "WriteToDisk"]),
            make(3, &["PdfExtGState", "WriteToDisk", "ExtraTok"]),
        ];
        let clusters = cluster_rules(rules, 0.4);
        assert_eq!(
            clusters.len(),
            1,
            "expected single cluster, got {clusters:#?}"
        );
        assert_eq!(clusters[0].pr_ids.len(), 3);
        assert_eq!(clusters[0].fix_rate, 1.0);
        assert!(clusters[0].confidence > 0.5);
    }

    #[test]
    fn cluster_rules_separates_different_patterns() {
        let make = |tokens: &[&str]| ParsedRule {
            rule_text: "r".into(),
            pattern_tokens: tokens.iter().map(|s| s.to_string()).collect(),
            file_path: "/f.vb".into(),
            file_pattern: "/**/*.vb".into(),
            language: "vbnet".into(),
            severity: Severity::Warning,
            fix_status: ThreadStatus::Fixed,
            fix_commit: None,
            llm_resolution: None,
            pr_id: 1,
            pr_url: "".into(),
            thread_id: 0,
            pr_date: "".into(),
            content_hash: blake3::hash(tokens.join(",").as_bytes()).to_hex()[..8].to_string(),
            semantic_hash: "s".into(),
            raw_body: "".into(),
            fix_hunk: None,
        };
        let rules = vec![
            make(&["DeleteAllOnSubmit", "SubmitChanges"]),
            make(&["SomethingTotally", "UnrelatedToken", "Mismatch"]),
        ];
        let clusters = cluster_rules(rules, 0.4);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn cluster_rules_respects_language_boundary() {
        let a = ParsedRule {
            rule_text: "".into(),
            pattern_tokens: vec!["DeleteAllOnSubmit".into(), "SubmitChanges".into()],
            file_path: "f.vb".into(),
            file_pattern: "**/*.vb".into(),
            language: "vbnet".into(),
            severity: Severity::Critical,
            fix_status: ThreadStatus::Fixed,
            fix_commit: None,
            llm_resolution: None,
            pr_id: 1,
            pr_url: "".into(),
            thread_id: 0,
            pr_date: "".into(),
            content_hash: "a".into(),
            semantic_hash: "sa".into(),
            raw_body: "".into(),
            fix_hunk: None,
        };
        let mut b = a.clone();
        b.language = "typescript".into();
        b.file_path = "f.ts".into();
        b.pr_id = 2;
        b.content_hash = "b".into();
        let clusters = cluster_rules(vec![a, b], 0.4);
        // Identical tokens but different languages → two clusters.
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn fix_rate_excludes_active_threads_from_denominator() {
        let tpl = |status| ParsedRule {
            rule_text: "r".into(),
            pattern_tokens: vec!["TokA".into(), "TokB".into(), "TokC".into()],
            file_path: "f.vb".into(),
            file_pattern: "**/*.vb".into(),
            language: "vbnet".into(),
            severity: Severity::Warning,
            fix_status: status,
            fix_commit: None,
            llm_resolution: None,
            pr_id: 1,
            pr_url: "".into(),
            thread_id: 0,
            pr_date: "".into(),
            content_hash: format!("{status:?}"),
            semantic_hash: "s-status".into(),
            raw_body: "".into(),
            fix_hunk: None,
        };
        let clusters = cluster_rules(
            vec![
                tpl(ThreadStatus::Fixed),
                tpl(ThreadStatus::Fixed),
                tpl(ThreadStatus::Active),
                tpl(ThreadStatus::WontFix),
            ],
            0.4,
        );
        // Active + WontFix: we only mix Fixed/Closed/Active on the
        // positive path (wontFix is a separate cluster), so the
        // cluster here should still come out with 2/2 = 100% fix rate
        // if the WontFix member was filtered before clustering. In
        // this unit test we call cluster_rules directly, so we just
        // assert the math matches the rule "fixed / (fixed + wontFix)".
        // Denominator is decisive = 2 fixed + 1 wontFix = 3.
        assert!(!clusters.is_empty());
        let c = &clusters[0];
        let expected = 2.0 / 3.0;
        assert!((c.fix_rate - expected).abs() < 0.01, "got {}", c.fix_rate);
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b":pat-token"), "OnBhdC10b2tlbg==");
    }

    #[test]
    fn fix_commit_marker_overrides_closed_status() {
        let raw = RawReviewComment {
            pr_id: 1,
            pr_title: "".into(),
            pr_author: "".into(),
            pr_date: "".into(),
            pr_branch: "".into(),
            pr_url: "".into(),
            thread_id: 1,
            thread_status: ThreadStatus::Closed,
            file_path: "/src/foo.ts".into(),
            line_start: 10,
            line_end: 12,
            severity: "major".into(),
            coderabbit_comment: "_⚠️ Potential issue_ | _🟠 Major_\n\n\
                **Missing null check on gQtyManager**\n\n\
                The call to `gQtyManager.validate()` can throw when window is not ready.\n\n\
                ✅ Addressed in commits 1331879 to 8133c13"
                .into(),
            fix_hunk: None,
        };
        let parsed = parse_comment(&raw).expect("parse");
        // Raw status was Closed but the ✅ marker promotes it to Fixed.
        assert_eq!(parsed.fix_status, ThreadStatus::Fixed);
        assert_eq!(parsed.fix_commit.as_deref(), Some("1331879"));
        // Weight function must return 1.0 — explicit commit evidence.
        assert!((parsed_rule_weight(&parsed) - 1.0).abs() < 0.001);
    }

    #[test]
    fn closed_without_fix_commit_stays_closed() {
        let raw = RawReviewComment {
            pr_id: 1,
            pr_title: "".into(),
            pr_author: "".into(),
            pr_date: "".into(),
            pr_branch: "".into(),
            pr_url: "".into(),
            thread_id: 1,
            thread_status: ThreadStatus::Closed,
            file_path: "/src/foo.ts".into(),
            line_start: 10,
            line_end: 12,
            severity: "minor".into(),
            coderabbit_comment: "_⚠️ Potential issue_ | _🟡 Minor_\n\n\
                **Consider renaming parameter for clarity**\n\n\
                The parameter `flag` inside `handleInput()` shadows an outer `flag` from the \
                surrounding `controller` scope."
                .into(),
            fix_hunk: None,
        };
        let parsed = parse_comment(&raw).expect("parse");
        assert_eq!(parsed.fix_status, ThreadStatus::Closed);
        assert!(parsed.fix_commit.is_none());
        // Closed weight is 0.6 per the updated table.
        assert!((parsed_rule_weight(&parsed) - 0.6).abs() < 0.001);
    }

    #[test]
    fn llm_resolution_fixed_caps_at_085() {
        let rule = ParsedRule {
            rule_text: "rule".into(),
            pattern_tokens: vec!["TokA".into(), "TokB".into(), "TokC".into()],
            file_path: "/f.ts".into(),
            file_pattern: "**/*.ts".into(),
            language: "typescript".into(),
            severity: Severity::Info,
            fix_status: ThreadStatus::Closed,
            fix_commit: None,
            llm_resolution: Some(LlmResolution::Fixed),
            pr_id: 1,
            pr_url: "".into(),
            thread_id: 0,
            pr_date: "".into(),
            content_hash: "x".into(),
            semantic_hash: "sx".into(),
            raw_body: "".into(),
            fix_hunk: None,
        };
        assert!((parsed_rule_weight(&rule) - 0.85).abs() < 0.001);
        assert!(!is_suppression(&rule));
    }

    #[test]
    fn llm_resolution_dismissed_goes_to_suppression() {
        let rule = ParsedRule {
            rule_text: "rule".into(),
            pattern_tokens: vec!["TokA".into(), "TokB".into(), "TokC".into()],
            file_path: "/f.ts".into(),
            file_pattern: "**/*.ts".into(),
            language: "typescript".into(),
            severity: Severity::Info,
            fix_status: ThreadStatus::Closed,
            fix_commit: None,
            llm_resolution: Some(LlmResolution::Dismissed),
            pr_id: 1,
            pr_url: "".into(),
            thread_id: 0,
            pr_date: "".into(),
            content_hash: "x".into(),
            semantic_hash: "sx".into(),
            raw_body: "".into(),
            fix_hunk: None,
        };
        assert!(is_suppression(&rule));
        assert_eq!(parsed_rule_weight(&rule), 0.0);
    }

    #[test]
    fn llm_single_letter_parser_handles_whitespace() {
        assert_eq!(parse_llm_single_letter("A"), Some(LlmResolution::Fixed));
        assert_eq!(
            parse_llm_single_letter("  B "),
            Some(LlmResolution::Dismissed)
        );
        assert_eq!(parse_llm_single_letter("C\n"), Some(LlmResolution::Unknown));
        assert_eq!(parse_llm_single_letter("a"), Some(LlmResolution::Fixed));
        assert_eq!(parse_llm_single_letter(""), None);
        assert_eq!(parse_llm_single_letter("D"), None);
    }

    #[test]
    fn parse_llm_handles_option_a_and_prefixes() {
        assert_eq!(
            parse_llm_single_letter("Option A"),
            Some(LlmResolution::Fixed)
        );
        assert_eq!(
            parse_llm_single_letter("The answer is A."),
            Some(LlmResolution::Fixed)
        );
        assert_eq!(
            parse_llm_single_letter("B) dismiss"),
            Some(LlmResolution::Dismissed)
        );
        assert_eq!(
            parse_llm_single_letter("I believe C is correct"),
            Some(LlmResolution::Unknown)
        );
        // Must NOT match an 'A' embedded in a longer word.
        assert_eq!(parse_llm_single_letter("Apple"), None);
        assert_eq!(parse_llm_single_letter("Banana"), None);
        // Empty / unrelated should return None.
        assert_eq!(parse_llm_single_letter(""), None);
        assert_eq!(parse_llm_single_letter("hmm no idea"), None);
    }

    #[test]
    fn semantic_hash_is_pr_independent() {
        // Same rule text + tokens + file_pattern from two different
        // PRs must produce identical semantic_hash (for LLM cache
        // reuse) but different content_hash (for dedup).
        let base = RawReviewComment {
            pr_id: 1,
            pr_title: "".into(),
            pr_author: "".into(),
            pr_date: "".into(),
            pr_branch: "".into(),
            pr_url: "".into(),
            thread_id: 100,
            thread_status: ThreadStatus::Fixed,
            file_path: "/Site/foo.ts".into(),
            line_start: 10,
            line_end: 12,
            severity: "major".into(),
            coderabbit_comment: "_⚠️ Potential issue_ | _🟠 Major_\n\n\
                **Avoid calling setWarningsText unconditionally.**\n\n\
                The `setWarningsText()` call in the `else` branch clears other warnings."
                .into(),
            fix_hunk: None,
        };
        let p1 = parse_comment(&base).unwrap();
        let base2 = RawReviewComment {
            pr_id: 2,
            thread_id: 200,
            ..base
        };
        let p2 = parse_comment(&base2).unwrap();
        assert_eq!(
            p1.semantic_hash, p2.semantic_hash,
            "semantic hash must be PR-independent so the LLM classifier \
             reuses one cached verdict across PRs"
        );
        assert_ne!(
            p1.content_hash, p2.content_hash,
            "content hash must include pr_id so cluster PR count is accurate"
        );
    }

    #[test]
    fn fix_rate_counts_llm_verdicts_into_math() {
        // 1 Fixed + 1 LLM-Fixed (closed) + 1 WontFix → fix_rate = 2/3.
        let mk = |status: ThreadStatus, llm: Option<LlmResolution>, pr: u64| -> ParsedRule {
            ParsedRule {
                rule_text: "r".into(),
                pattern_tokens: vec!["TokA".into(), "TokB".into(), "TokC".into()],
                file_path: "/x.ts".into(),
                file_pattern: "/x/**/*.ts".into(),
                language: "typescript".into(),
                severity: Severity::Warning,
                fix_status: status,
                fix_commit: None,
                llm_resolution: llm,
                pr_id: pr,
                pr_url: "".into(),
                thread_id: pr,
                pr_date: "".into(),
                content_hash: format!("{pr}"),
                semantic_hash: "s".into(),
                raw_body: "".into(),
                fix_hunk: None,
            }
        };
        let rules = vec![
            mk(ThreadStatus::Fixed, None, 1),
            mk(ThreadStatus::Closed, Some(LlmResolution::Fixed), 2),
            mk(ThreadStatus::WontFix, None, 3),
        ];
        let clusters = cluster_rules(rules, 0.4);
        assert_eq!(clusters.len(), 1);
        let expected = 2.0 / 3.0;
        assert!(
            (clusters[0].fix_rate - expected).abs() < 0.001,
            "expected {:.3}, got {:.3}",
            expected,
            clusters[0].fix_rate
        );
    }

    #[test]
    fn unknown_thread_status_falls_back_not_fatal() {
        // JSONL with a status string serde doesn't recognise should
        // deserialize cleanly to Unknown — NOT fail the whole record.
        let line = r#"{"pr_id":1,"thread_status":"pending_review","file_path":"/x.ts","line_start":1,"line_end":2,"severity":"minor","coderabbit_comment":"body"}"#;
        let parsed: Result<RawReviewComment, _> = serde_json::from_str(line);
        assert!(
            parsed.is_ok(),
            "unknown thread_status must fall back, not fail: {:?}",
            parsed.err()
        );
        assert_eq!(parsed.unwrap().thread_status, ThreadStatus::Unknown);
    }

    #[test]
    fn derive_file_pattern_handles_extensionless() {
        assert_eq!(
            derive_file_pattern("Makefile"),
            "**/Makefile".to_ascii_lowercase()
        );
        assert_eq!(
            derive_file_pattern("/repo/Dockerfile"),
            "/repo/**/dockerfile"
        );
        assert_eq!(derive_file_pattern("/x/Rakefile"), "/x/**/rakefile");
    }

    #[test]
    fn wontfix_never_promoted_by_fix_commit() {
        // Even if CR later said "addressed" elsewhere, a wontFix thread
        // is an explicit rejection and must stay in suppression.
        assert_eq!(
            effective_fix_status(ThreadStatus::WontFix, Some("abc1234")),
            ThreadStatus::WontFix
        );
    }

    #[test]
    fn suppression_cluster_gets_zero_fix_rate() {
        let rule = ParsedRule {
            rule_text: "suppress me".into(),
            pattern_tokens: vec!["gQtyManager".into(), "validate".into(), "window".into()],
            file_path: "/Site/ts/qty/qtyManager.ts".into(),
            file_pattern: "/site/ts/qty/**/*.ts".into(),
            language: "typescript".into(),
            severity: Severity::Info,
            fix_status: ThreadStatus::WontFix,
            fix_commit: None,
            llm_resolution: None,
            pr_id: 1,
            pr_url: "".into(),
            thread_id: 0,
            pr_date: "".into(),
            content_hash: "x".into(),
            semantic_hash: "sx".into(),
            raw_body: "".into(),
            fix_hunk: None,
        };
        let clusters = cluster_rules(vec![rule], 0.4);
        assert_eq!(clusters[0].fix_rate, 0.0);
    }
}
