use crate::tools::Engram;
use engram_core::{ContentHash, DocIdStr};
use engram_git::history::GitWalker;
use engram_index::IndexDoc;
use rmcp::ErrorData as McpError;
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio_util::sync::CancellationToken;

const CURSOR: &str = "git_document_refresh_v1_after";

impl Engram {
    /// Caller owns the project update lock. This path deliberately has no graph
    /// writes and does not call the history walker or alter its checkpoints.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn refresh_history_documents(
        &self,
        project_id: &str,
        directory: &str,
        generation: u64,
        max_commits: usize,
        force: bool,
        cancel: &CancellationToken,
        mut progress: Box<dyn FnMut(usize, usize) + Send>,
    ) -> Result<String, McpError> {
        let result: anyhow::Result<String> = async {
            let ps = self.ensure_project_runtime(project_id).await
                .map_err(|e| anyhow::anyhow!(e.message.to_string()))?;
            let search = ps.search.clone();
            let mut commits: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
            let snapshot = tokio::task::spawn_blocking({
                let search = search.clone(); let pid = project_id.to_string();
                move || search.list_docs_in_namespace(&pid, "history")
            }).await??;
            for doc in snapshot {
                if let Some((oid, _)) = doc.path.strip_prefix("diff:").and_then(|s| s.split_once(':'))
                    && git2::Oid::from_str(oid).is_ok() {
                    commits.entry(oid.to_string()).or_default().push((doc.path, doc.doc_id));
                }
            }
            let cursor = if force { None } else { self.state.registry.get_meta(project_id, CURSOR)? };
            let remaining: Vec<_> = commits.into_iter()
                .filter(|(oid,_)| cursor.as_ref().is_none_or(|c| oid > c)).collect();
            let more = remaining.len() > max_commits;
            let remaining_after_batch = remaining.len().saturating_sub(max_commits);
            let total = remaining.len().min(max_commits);
            let mut checked = 0;
            let mut refreshed = 0;
            let mut removed = 0;
            let mut pending_docs = Vec::new();
            let mut pending_obsolete = Vec::new();
            let mut pending_bytes = 0usize;
            for (oid, old) in remaining.into_iter().take(max_commits) {
                anyhow::ensure!(!cancel.is_cancelled(), "history document refresh cancelled");
                let docs = tokio::task::spawn_blocking({
                    let directory = directory.to_string(); let oid = oid.clone();
                    let wanted: HashSet<_> = old.iter().map(|(p,_)| p.clone()).collect();
                    move || -> anyhow::Result<Vec<IndexDoc>> {
                        let repo = GitWalker::open_repo(std::path::Path::new(&directory))?;
                        let id = git2::Oid::from_str(&oid)?;
                        let commit = repo.find_commit(id)?;
                        let mut docs = Vec::new();
                        for (path, content) in GitWalker::diff_text_for_commit(&repo, id, 50_000)? {
                            let path = format!("diff:{oid}:{path}");
                            if !wanted.contains(&path) { continue; }
                            let hash = ContentHash::compute(content.as_bytes());
                            docs.push(IndexDoc {
                                generation, chunk_id: engram_index::chunk_id_from_content_hash(&hash),
                                doc_id: DocIdStr::compute(&path, 0, 0, &hash).0,
                                content_hash: hash.0, path: path.into(), language: "diff".into(),
                                content, namespace: "history".into(),
                                author: Some(commit.author().name().unwrap_or("unknown").to_string()),
                                timestamp: Some(commit.time().seconds() as u64), start_line: 0, end_line: 0,
                            });
                        }
                        anyhow::ensure!(docs.len() == wanted.len(),
                            "Cannot reproduce all stored diff paths for {oid}; old evidence preserved");
                        Ok(docs)
                    }
                }).await??;
                let new_ids: HashMap<_,_> = docs.iter().map(|d| (d.path.as_str(), d.doc_id.as_str())).collect();
                let obsolete: Vec<_> = old.iter().filter(|(path,id)|
                    new_ids.get(path.as_str()).is_some_and(|new| *new != id.as_str()))
                    .map(|(_,id)| id.clone()).collect();
                if !obsolete.is_empty() {
                    pending_bytes += docs.iter().map(|d| d.content.len()).sum::<usize>();
                    pending_docs.extend(docs);
                    pending_obsolete.extend(obsolete);
                }
                checked += 1;
                // Amortize Tantivy commits and Lance fragment rewrites across
                // small commits. The cursor advances only after the entire
                // replacement batch and cleanup succeed, so retries converge.
                if pending_docs.len() >= 200 || pending_bytes >= 8_000_000 || checked == total {
                    for batch in pending_docs.chunks(200) {
                        search.index_docs(project_id, batch, cancel).await?;
                        anyhow::ensure!(!cancel.is_cancelled(), "history document refresh cancelled; old evidence retained");
                    }
                    // Only superseded IDs are deleted, after successful writes.
                    search.delete_documents(project_id, "history", &pending_obsolete).await?;
                    refreshed += pending_docs.len(); removed += pending_obsolete.len();
                    pending_docs.clear(); pending_obsolete.clear(); pending_bytes = 0;
                    self.state.registry.set_meta(project_id, CURSOR, &oid)?;
                }
                progress(checked, total);
            }
            if !more { self.state.registry.set_meta(project_id, CURSOR, "")?; }
            Ok(format!("history_document_refresh:\ncommits_checked: {checked}\ndocuments_refreshed: {refreshed}\nobsolete_documents_removed: {removed}\nmore: {more}\nremaining_commits: {remaining_after_batch}\ngraph_edges: unchanged\nhistory_watermarks: unchanged\n{}",
                if more { "Repeat mode='refresh' to continue." } else { "Stored diff refresh complete." }))
        }.await;
        result.map_err(|e| {
            McpError::internal_error(format!("history document refresh failed: {e:#}"), None)
        })
    }
}
