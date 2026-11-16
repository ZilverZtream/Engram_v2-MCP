//! Inspect a selected Git change without treating a clipped search card as its cohort.
use crate::{models::SearchHistoryRequest, tools::Engram};
use anyhow::{Context, Result, ensure};
use engram_git::history::GitWalker;
use git2::Oid;
use rmcp::{ErrorData as McpError, model::CallToolResult};
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn open_scoped(
    root: &Path,
    oid: &str,
    cutoff: Option<&str>,
) -> Result<(git2::Repository, Oid, Oid)> {
    let repo = GitWalker::open_repo(root)?;
    let id = Oid::from_str(oid)?;
    let scope = match cutoff {
        Some(rev) => repo.revparse_single(rev)?.peel_to_commit()?.id(),
        None => GitWalker::approved_history_root(&repo)
            .context("No published history scope is available; supply as_of_rev explicitly")?,
    };
    ensure!(
        id == scope || repo.graph_descendant_of(scope, id)?,
        "Commit is outside the selected history scope"
    );
    Ok((repo, id, scope))
}

fn read(root: &Path, project: &str, oid: &str, cutoff: Option<&str>) -> Result<Value> {
    let (repo, id, scope) = open_scoped(root, oid, cutoff)?;
    let commit = repo.find_commit(id)?;
    ensure!(
        commit.message_bytes().len() <= 256 * 1024,
        "Complete Git message exceeds256KiB; no partial commit detail returned"
    );
    let message =
        std::str::from_utf8(commit.message_bytes()).context("Git commit message is not UTF-8")?;
    let tree = commit.tree()?;
    let parent = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };
    let diff = repo.diff_tree_to_tree(parent.as_ref(), Some(&tree), None)?;
    ensure!(
        diff.deltas().len() <= 10000,
        "Complete Git cohort exceeds10000paths; no partial commit detail returned"
    );
    let mut files = Vec::new();
    let mut changes = Vec::new();
    let mut bytes = message.len();
    for delta in diff.deltas() {
        let old = delta.old_file();
        let new = delta.new_file();
        let path = new
            .path()
            .or_else(|| old.path())
            .context("Changed Git entry has no path")?;
        let path = path.to_str().context("Changed Git path is not UTF-8")?;
        bytes = bytes.saturating_add(path.len());
        ensure!(
            bytes <= 2 * 1024 * 1024,
            "Complete Git message/cohort exceeds2MiB; no partial commit detail returned"
        );
        files.push(path.to_owned());
        changes.push(json!({"status":format!("{:?}",delta.status()),"path":path,
            "old_blob":if old.id().is_zero(){None}else{Some(old.id().to_string())},
            "new_blob":if new.id().is_zero(){None}else{Some(new.id().to_string())},
            "old_mode":format!("{:?}",old.mode()),"new_mode":format!("{:?}",new.mode()),
            "patch":{"tool":"search_history","arguments":{"project_id":project,"query":id.to_string(),
                "mode":"patch","file_filter":path,"as_of_rev":scope.to_string(),
                "output_json":true,"retain_response":true,"max_tokens":1024}}}));
    }
    Ok(
        json!({"rank":1,"project_id":project,"commit":id.to_string(),
        "parents":commit.parent_ids().map(|id|id.to_string()).collect::<Vec<_>>(),
        "author":commit.author().name(),"author_email":commit.author().email(),"timestamp":commit.time().seconds(),
        "message":commit.summary(),"content":message,"message_blake3":blake3::hash(message.as_bytes()).to_hex().to_string(),
        "content_truncated":false,"content_coverage":"Complete original UTF-8 Git commit message",
        "files":files,"file_count":changes.len(),"files_truncated":false,"file_changes":changes,
        "source":"local_git_objects","as_of_commit":scope.to_string(),
        "coverage":"Complete first-parent tree delta, including generated paths. No rename inference: moves can appear as delete/add. Blob IDs and modes describe retained Git objects, not deployed behavior, reviewer approval or current policy applicability. Patch bodies are not included."}),
    )
}

impl Engram {
    pub(super) async fn handle_history_commit(
        &self,
        req: SearchHistoryRequest,
    ) -> Result<CallToolResult, McpError> {
        super::validate_project_id(&req.project_id)?;
        if req.query.len() != 40 || !req.query.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(McpError::invalid_params(
                "commit mode query must be an exact40hex Git commit ID",
                None,
            ));
        }
        if req.file_filter.is_some()
            || req.exclude_paths.as_ref().is_some_and(|v| !v.is_empty())
            || req.author_filter.is_some()
            || req.date_after.is_some()
            || req.date_before.is_some()
            || req.offset != 0
        {
            return Err(McpError::invalid_params(
                "commit mode reads one complete cohort; search filters and nonzero offset are not supported. Use as_of_rev for history scope.",
                None,
            ));
        }
        if req.as_of_rev.as_ref().is_some_and(|rev| {
            rev.trim().is_empty() || rev.len() > 256 || rev.chars().any(char::is_control)
        }) {
            return Err(McpError::invalid_params(
                "as_of_rev must be a nonempty Git revision of at most256bytes",
                None,
            ));
        }
        let record = self.ensure_project_record(&req.project_id).await?;
        let project = req.project_id.clone();
        let oid = req.query.clone();
        let cutoff = req.as_of_rev.clone();
        let row = tokio::task::spawn_blocking(move || {
            read(
                Path::new(&record.directory),
                &project,
                &oid,
                cutoff.as_deref(),
            )
        })
        .await
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
        .map_err(|error| {
            McpError::invalid_params(format!("Commit detail unavailable: {error:#}"), None)
        })?;
        let block = format!(
            "commit: {}\n{}\nfiles (complete first-parent cohort): {}\nfile_changes: {}\n",
            row["commit"],
            row["content"].as_str().unwrap_or_default(),
            row["files"],
            row["file_changes"]
        );
        let metadata = json!({"mode":"commit","source":"local_git_objects","search_executed":false,
            "ranked_matches_before_limit":1,"semantic_retrieval":"not_run","reranking":"not_run",
            "scope_commit":row["as_of_commit"],"next_tools":[],
            "complete_capture":{"tool":"search_history","arguments":{"project_id":req.project_id,
                "query":req.query,"mode":"commit","as_of_rev":row["as_of_commit"],
                "output_json":true,"retain_response":true,"max_tokens":1024}}});
        super::history_response::finish(
            &req,
            "Complete selected Git change\n".into(),
            &[block],
            &[row],
            metadata,
        )
    }
}
