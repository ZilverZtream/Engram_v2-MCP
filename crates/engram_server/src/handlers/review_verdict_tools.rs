//! `ingest_review_verdicts` / `get_review_precedents`: review findings with
//! the verdict of the people who decide them. See
//! `services::review_verdict_service` for the verdict rules.

use crate::handlers::validate_project_id;
use crate::models::{GetReviewPrecedentsRequest, IngestReviewVerdictsRequest};
use crate::services::review_verdict_service::{self as rv, AuthorRecord, ReviewThread};
use crate::tools::Engram;
use engram_core::namespaces::NAMESPACE_REVIEW_VERDICT;
use engram_core::{ContentHash, DocIdStr};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use std::collections::BTreeMap;

/// Path prefix of verdict documents, and the path of the authors record.
const VERDICT_PREFIX: &str = "review:PR-";
const AUTHORS_PATH: &str = "review:authors";

/// Stored beside the verdicts: who decides, the owner's stated trust in
/// authors, and every author's record of argued declines.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AuthorsDoc {
    decision_makers: Vec<String>,
    #[serde(default)]
    author_trust: BTreeMap<String, String>,
    records: BTreeMap<String, AuthorRecord>,
}

fn verdict_path(thread: &ReviewThread) -> String {
    format!("{VERDICT_PREFIX}{}:{}", thread.pr_id, thread.thread_id)
}

fn pr_of_path(path: &str) -> Option<u64> {
    path.strip_prefix(VERDICT_PREFIX)?
        .split(':')
        .next()?
        .parse()
        .ok()
}

/// Upsertable document: identity from the path alone, so a re-ingest
/// replaces a thread's verdict instead of adding a second one.
fn stable_doc(
    path: &str,
    content: String,
    author: Option<String>,
    timestamp: Option<u64>,
) -> engram_index::IndexDoc {
    let path_hash = ContentHash::compute(path.as_bytes());
    engram_index::IndexDoc {
        generation: 0,
        chunk_id: engram_index::chunk_id_from_content_hash(&path_hash),
        doc_id: DocIdStr::compute(path, 0, 0, &path_hash).0,
        content_hash: ContentHash::compute(content.as_bytes()).0,
        path: path.into(),
        language: "text".into(),
        content,
        namespace: NAMESPACE_REVIEW_VERDICT.into(),
        author,
        timestamp,
        start_line: 0,
        end_line: 0,
    }
}

impl Engram {
    pub async fn handle_ingest_review_verdicts(
        &self,
        req: IngestReviewVerdictsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let decision_makers: Vec<String> = req
            .decision_makers
            .iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
        if decision_makers.is_empty() {
            return Err(McpError::invalid_params(
                "decision_makers must name at least one reviewer whose replies decide findings",
                None,
            ));
        }
        if let Some(cutoff) = req.completed_before.as_deref()
            && crate::handlers::pr_history_tools::ymd_to_epoch_secs(cutoff).is_none()
        {
            return Err(McpError::invalid_params(
                format!("completed_before must be YYYY-MM-DD, got '{cutoff}'"),
                None,
            ));
        }
        let record = self.ensure_project_record(&req.project_id).await?;
        let threads: Vec<ReviewThread> = match req.source.as_deref().unwrap_or("azure_devops") {
            "json_file" => {
                let raw = req.file_path.as_deref().ok_or_else(|| {
                    McpError::invalid_params("source json_file requires file_path", None)
                })?;
                let path = std::path::Path::new(raw);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    std::path::Path::new(&record.directory).join(path)
                };
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    McpError::invalid_params(format!("cannot read {}: {e}", path.display()), None)
                })?;
                text.lines()
                    .enumerate()
                    .filter(|(_, line)| !line.trim().is_empty())
                    .map(|(n, line)| {
                        serde_json::from_str::<ReviewThread>(line).map_err(|e| {
                            McpError::invalid_params(format!("line {}: {e}", n + 1), None)
                        })
                    })
                    .collect::<Result<_, _>>()?
            }
            "azure_devops" => {
                let pat = req
                    .pat_token
                    .clone()
                    .into_iter()
                    .chain(std::env::var("ADO_PAT").ok())
                    .map(|p| p.trim().to_string())
                    .find(|p| !p.is_empty())
                    .ok_or_else(|| {
                        McpError::invalid_params(
                            "no Azure DevOps PAT: pass pat_token or set ADO_PAT for the server",
                            None,
                        )
                    })?;
                let (org, project, repo) = self.review_repo_coordinates(&req, &record.directory)?;
                let window = rv::FetchWindow {
                    min_pr_id: req.min_pr_id,
                    max_pr_id: req.max_pr_id,
                    completed_before: req.completed_before.clone(),
                    max_prs: req.max_prs,
                };
                rv::fetch_azure_devops_threads(&org, &project, &repo, &pat, &window)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("Azure DevOps fetch failed: {e}"), None)
                    })?
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("source must be azure_devops or json_file, got '{other}'"),
                    None,
                ));
            }
        };
        let threads: Vec<ReviewThread> = threads
            .into_iter()
            .filter(|t| {
                req.min_pr_id.is_none_or(|lo| t.pr_id >= lo)
                    && req.max_pr_id.is_none_or(|hi| t.pr_id <= hi)
            })
            .collect();

        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut docs = Vec::with_capacity(threads.len() + 1);
        for thread in &threads {
            let verdict = rv::thread_verdict(thread, &decision_makers);
            *counts.entry(verdict.kind.as_str()).or_default() += 1;
            let timestamp = thread
                .pr_date
                .get(..10)
                .and_then(crate::handlers::pr_history_tools::ymd_to_epoch_secs);
            docs.push(stable_doc(
                &verdict_path(thread),
                rv::verdict_doc_content(thread, &verdict),
                thread.comments.first().map(|c| c.author.clone()),
                timestamp,
            ));
        }
        let authors = AuthorsDoc {
            decision_makers: decision_makers.clone(),
            author_trust: req.author_trust.clone().unwrap_or_default(),
            records: rv::author_records(&threads, &decision_makers),
        };
        let authors_json = serde_json::to_string(&authors)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        docs.push(stable_doc(AUTHORS_PATH, authors_json, None, None));

        let ps = self.ensure_project_runtime(&req.project_id).await?;
        ps.search
            .index_docs(
                &req.project_id,
                &docs,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let prs: std::collections::BTreeSet<u64> = threads.iter().map(|t| t.pr_id).collect();
        let mut out = format!(
            "Ingested {} review threads from {} PRs (PR {}–{}) as verdicts; decision-makers: {}.\n\nVerdicts:\n",
            threads.len(),
            prs.len(),
            prs.first().copied().unwrap_or(0),
            prs.last().copied().unwrap_or(0),
            decision_makers.join(", "),
        );
        for (kind, n) in &counts {
            out.push_str(&format!("- {kind}: {n}\n"));
        }
        out.push_str("\nPR authors' explicit declines (wontFix/byDesign) of others' findings:\n");
        for (author, r) in &authors.records {
            out.push_str(&format!(
                "- {author}: argued {} ({} stood, {} overturned by a decision-maker), bare {}{}\n",
                r.argued_stood + r.argued_overturned,
                r.argued_stood,
                r.argued_overturned,
                r.bare,
                authors
                    .author_trust
                    .get(author)
                    .map(|t| format!("; owner trust: {t}"))
                    .unwrap_or_default(),
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// org / project / repo: request, then saved coordinates, then the git
    /// remote (`https://dev.azure.com/{org}/{project}/_git/{repo}`).
    fn review_repo_coordinates(
        &self,
        req: &IngestReviewVerdictsRequest,
        directory: &str,
    ) -> Result<(String, String, String), McpError> {
        let meta = |key| {
            self.state
                .registry
                .get_meta(&req.project_id, key)
                .ok()
                .flatten()
        };
        let remote =
            crate::handlers::planning_tools::git_remote_origin_url(std::path::Path::new(directory));
        let from_remote = remote.as_deref().and_then(|url| {
            let (org, project) = crate::handlers::planning_tools::ado_coords_from_remote_url(url)?;
            let repo = url
                .trim_end_matches('/')
                .trim_end_matches(".git")
                .rsplit('/')
                .next()
                .map(str::to_string)?;
            Some((org, project, repo))
        });
        let org = req
            .org
            .clone()
            .or_else(|| meta("ado_org"))
            .or_else(|| from_remote.as_ref().map(|c| c.0.clone()));
        let project = req
            .project
            .clone()
            .or_else(|| meta("ado_project"))
            .or_else(|| from_remote.as_ref().map(|c| c.1.clone()));
        let repo = req
            .repo
            .clone()
            .or_else(|| from_remote.as_ref().map(|c| c.2.clone()));
        match (org, project, repo) {
            (Some(org), Some(project), Some(repo)) => Ok((org, project, repo)),
            _ => Err(McpError::invalid_params(
                "Azure DevOps coordinates unknown: pass org, project and repo (the git remote is not an Azure DevOps URL)",
                None,
            )),
        }
    }

    pub async fn handle_get_review_precedents(
        &self,
        req: GetReviewPrecedentsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.query.trim().is_empty() {
            return Err(McpError::invalid_params("query must not be empty", None));
        }
        let wanted: Option<Vec<String>> = req
            .verdicts
            .as_ref()
            .map(|v| v.iter().map(|s| s.trim().to_lowercase()).collect());
        let limit = req.limit.clamp(1, 30);
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let mut text = req.query.clone();
        if let Some(file) = req.file_path.as_deref() {
            text.push(' ');
            text.push_str(file.replace('\\', "/").rsplit('/').next().unwrap_or(file));
        }
        let query = engram_index::HybridQuery {
            project_id: req.project_id.clone(),
            namespace: NAMESPACE_REVIEW_VERDICT.into(),
            generation: 0,
            text,
            top_k: (limit * 5).min(200),
            fts_mode: "loose".into(),
            include_path_prefixes: Some(vec![VERDICT_PREFIX.into()]),
            exclude_path_prefixes: None,
            include_path_suffixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        let hits = ps
            .search
            .search(&query, None, &tokio_util::sync::CancellationToken::new())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let authors: AuthorsDoc = ps
            .search
            .stored_doc_at_path(&req.project_id, NAMESPACE_REVIEW_VERDICT, AUTHORS_PATH)
            .ok()
            .flatten()
            .and_then(|d| serde_json::from_str(&d.content).ok())
            .unwrap_or_default();
        if authors.decision_makers.is_empty() && hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No review verdicts are ingested for this project; run ingest_review_verdicts first.",
            )]));
        }

        let file = req.file_path.as_deref().map(|f| f.replace('\\', "/"));
        let mut rows: Vec<(bool, String, String)> = Vec::new(); // (same file, kind, content)
        for hit in &hits {
            if req
                .before_pr_id
                .is_some_and(|cut| pr_of_path(hit.path.as_str()).is_none_or(|pr| pr >= cut))
            {
                continue;
            }
            let Some(doc) = ps.search.stored_doc_by_pk(&hit.pk).ok().flatten() else {
                continue;
            };
            let kind = doc
                .content
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("Review verdict: "))
                .and_then(|l| l.split_whitespace().next())
                .unwrap_or("unknown")
                .to_string();
            if wanted.as_ref().is_some_and(|w| !w.contains(&kind)) {
                continue;
            }
            let same_file = file.as_ref().is_some_and(|f| {
                doc.content.lines().any(|l| {
                    l.starts_with("File: ")
                        && l.trim_end_matches(char::is_numeric)
                            .trim_end_matches(':')
                            .ends_with(f.trim_start_matches('/'))
                })
            });
            rows.push((same_file, kind, doc.content));
        }
        // Same-file precedents first; relevance order otherwise.
        rows.sort_by_key(|(same_file, _, _)| !*same_file);
        rows.truncate(limit);
        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No past review findings match '{}'.",
                req.query
            ))]));
        }

        let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, kind, _) in &rows {
            *tally.entry(kind.as_str()).or_default() += 1;
        }
        let mut out = format!(
            "{} past review findings like this ({}). Decision-makers: {}. A rejection's reason is \
             the team's ruling on that trade-off; do not re-raise it without new evidence.\n",
            rows.len(),
            tally
                .iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect::<Vec<_>>()
                .join(", "),
            if authors.decision_makers.is_empty() {
                "unknown".to_string()
            } else {
                authors.decision_makers.join(", ")
            },
        );
        for (i, (same_file, kind, content)) in rows.iter().enumerate() {
            out.push_str(&format!(
                "\n--- #{}{} ---\n{}",
                i + 1,
                if *same_file { " (same file)" } else { "" },
                content.trim_end()
            ));
            if kind == "contested_by_author"
                && let Some(author) = content
                    .lines()
                    .find_map(|l| l.strip_prefix("Reasoning ("))
                    .and_then(|l| l.split_once(')'))
                    .map(|(who, _)| who)
            {
                let record = authors.records.get(author).cloned().unwrap_or_default();
                out.push_str(&format!(
                    "\nAuthor record: {} argued declines ({} stood, {} overturned), {} bare; argued in {:.0}% of declines{}.",
                    record.argued_stood + record.argued_overturned,
                    record.argued_stood,
                    record.argued_overturned,
                    record.bare,
                    record.argued_rate() * 100.0,
                    authors
                        .author_trust
                        .get(author)
                        .map(|t| format!("; owner trust: {t}"))
                        .unwrap_or_default(),
                ));
            }
            out.push('\n');
        }
        Ok(CallToolResult::success(vec![Content::text(
            out.trim_end().to_string(),
        )]))
    }
}
