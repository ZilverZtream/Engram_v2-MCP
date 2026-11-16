//! Complete source change inspection from retained Git objects, not current disk.
use crate::{models::SearchHistoryRequest, tools::Engram};
use anyhow::{Context, Result, ensure};
use git2::{DiffOptions, Patch};
use rmcp::{ErrorData as McpError, model::CallToolResult};
use serde_json::{Value, json};
use std::path::Path;

const MAX_BYTES: usize = 2 * 1024 * 1024;

fn read(root: &Path, project: &str, oid: &str, cutoff: Option<&str>, path: &str) -> Result<Value> {
    let (repo, id, scope) = super::history_commit::open_scoped(root, oid, cutoff)?;
    let commit = repo.find_commit(id)?;
    let tree = commit.tree()?;
    let parent = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };
    let mut options = DiffOptions::new();
    options
        .pathspec(path)
        .disable_pathspec_match(true)
        .id_abbrev(40)
        .max_size(MAX_BYTES as i64);
    let diff = repo.diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut options))?;
    ensure!(
        diff.deltas().len() == 1,
        "Exact path has no first-parent change in the selected commit"
    );
    let delta = diff
        .get_delta(0)
        .context("Selected Git delta unavailable")?;
    let mut binary = false;
    for file in [delta.old_file(), delta.new_file()] {
        if !file.id().is_zero() {
            let blob = repo
                .find_blob(file.id())
                .context("Changed entry is not an available Git blob")?;
            ensure!(
                blob.size() <= MAX_BYTES,
                "Git blob exceeds2MiB; no partial patch returned"
            );
            binary |= blob.is_binary();
        }
    }
    let patch = if binary {
        None
    } else {
        let mut patch = Patch::from_diff(&diff, 0)?.context("Git text patch unavailable")?;
        let buffer = patch.to_buf()?;
        ensure!(
            buffer.len() <= MAX_BYTES,
            "Git patch exceeds2MiB; no partial patch returned"
        );
        Some(
            std::str::from_utf8(&buffer)
                .context("Git patch is not UTF-8; no lossy patch returned")?
                .to_owned(),
        )
    };
    let old = delta.old_file();
    let new = delta.new_file();
    Ok(
        json!({"rank":1,"project_id":project,"commit":id.to_string(),"as_of_commit":scope.to_string(),
        "first_parent":commit.parent_ids().next().map(|id|id.to_string()),"path":path,
        "status":if binary {"BINARY_NO_TEXT_PATCH"} else {"COMPLETE_TEXT_PATCH"},
        "change_status":format!("{:?}",delta.status()),
        "old_blob":if old.id().is_zero(){None}else{Some(old.id().to_string())},
        "new_blob":if new.id().is_zero(){None}else{Some(new.id().to_string())},
        "old_mode":format!("{:?}",old.mode()),"new_mode":format!("{:?}",new.mode()),
        "patch_blake3":patch.as_ref().map(|text|blake3::hash(text.as_bytes()).to_hex().to_string()),
        "patch_bytes":patch.as_ref().map(String::len),"patch":patch,"patch_truncated":false,
        "source":"local_git_objects","coverage":"Exact-path first-parent unified Git delta with three context lines and full object IDs. No external diff/textconv command or rename inference. Binary entries retain identities but no text patch. This is historical source evidence, not current policy, approval, deployment or runtime behavior."}),
    )
}

impl Engram {
    pub(super) async fn handle_history_patch(
        &self,
        req: SearchHistoryRequest,
    ) -> Result<CallToolResult, McpError> {
        super::validate_project_id(&req.project_id)?;
        if req.query.len() != 40 || !req.query.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(McpError::invalid_params(
                "patch mode query must be an exact40hex Git commit ID",
                None,
            ));
        }
        let path=req.file_filter.as_deref().filter(|path|!path.is_empty() && path.len()<=1024
            && !path.contains(['\\',':','\0']) && path.split('/').all(|part|!matches!(part,""|"."|"..")))
            .ok_or_else(||McpError::invalid_params("patch mode requires file_filter as one exact repository-relative Git path (forward slashes, at most1024bytes)",None))?.to_owned();
        if req
            .exclude_paths
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
            || req.author_filter.is_some()
            || req.date_after.is_some()
            || req.date_before.is_some()
            || req.offset != 0
        {
            return Err(McpError::invalid_params(
                "patch mode reads one exact file; unrelated search filters and nonzero offset are not supported",
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
                &path,
            )
        })
        .await
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
        .map_err(|error| {
            McpError::invalid_params(format!("Git patch unavailable: {error:#}"), None)
        })?;
        let block = format!(
            "commit: {}\npath: {}\nstatus: {}\n{}\ncoverage: {}\n",
            row["commit"],
            row["path"],
            row["status"],
            row["patch"].as_str().unwrap_or("No text patch supplied"),
            row["coverage"]
        );
        let metadata = json!({"mode":"patch","source":"local_git_objects","search_executed":false,
            "ranked_matches_before_limit":1,"semantic_retrieval":"not_run","reranking":"not_run",
            "scope_commit":row["as_of_commit"],"next_tools":[],
            "complete_capture":{"tool":"search_history","arguments":{"project_id":req.project_id,
                "query":req.query,"mode":"patch","file_filter":req.file_filter,"as_of_rev":row["as_of_commit"],
                "output_json":true,"retain_response":true,"max_tokens":1024}}});
        super::history_response::finish(
            &req,
            "Selected historical file patch\n".into(),
            &[block],
            &[row],
            metadata,
        )
    }
}
