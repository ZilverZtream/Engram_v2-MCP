use engram_graph::GraphStore;
use git2::{ErrorCode, ObjectType, Oid, Repository, Tree};
use rmcp::ErrorData as McpError;
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::Path};

const MAX_FILES: usize = 16;
const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;

pub(super) fn validate(paths: &[String]) -> Result<Vec<String>, McpError> {
    let invalid = || {
        McpError::invalid_params(
            "source_files requires 1..16 literal project-relative paths of at most 1024 UTF-8 bytes",
            None,
        )
    };
    if paths.is_empty() || paths.len() > MAX_FILES {
        return Err(invalid());
    }
    paths
        .iter()
        .map(|path| {
            if path.len() > 1024 || path.chars().any(char::is_control) {
                return Err(invalid());
            }
            crate::services::project_policy::normalized_relative(path).ok_or_else(invalid)
        })
        .collect()
}

pub(super) fn observe(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    generation: u64,
    revisions: Option<&BTreeMap<String, u64>>,
    paths: &[String],
) -> Value {
    let files: Vec<_> = paths
        .iter()
        .map(|path| {
            let Some(revisions) = revisions else {
                return json!({"path":path,"status":"managed_source_selection_unavailable"});
            };
            let Some(&revision) = revisions.get(path) else {
                return json!({"path":path,"status":"file_not_selected"});
            };
            let mut report = json!({"path":path,"physical_revision":revision});
            let node = graph.get_node_bounded(project, &format!("file:{path}"), MAX_METADATA_BYTES);
            let Some(hash) = node.ok().flatten().and_then(|(node, _)| {
                if node.node_type != "file"
                    || node.file_path.as_str() != path
                    || node.generation != revision
                {
                    return None;
                }
                let metadata = node.metadata?;
                let hash = metadata.get("file_hash")?.as_str()?;
                (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
                    .then(|| hash.to_ascii_lowercase())
            }) else {
                report["status"] = json!("published_fingerprint_unavailable");
                return report;
            };
            report["indexed_blake3"] = json!(hash);
            let current = (|| -> anyhow::Result<_> {
                let file = engram_core::safe_open_read(root, path)?;
                let metadata = file.metadata()?;
                anyhow::ensure!(metadata.is_file(), "not a regular source file");
                let mut hasher = blake3::Hasher::new();
                let bytes = std::io::copy(&mut file.take(MAX_SOURCE_BYTES + 1), &mut hasher)?;
                Ok((bytes, hasher.finalize().to_hex().to_string()))
            })();
            report["status"] = match current {
                Ok((bytes, _)) if bytes > MAX_SOURCE_BYTES => json!("disk_file_over_limit"),
                Ok((bytes, current_hash)) => {
                    let matches = current_hash == hash;
                    report["disk_blake3"] = json!(current_hash);
                    report["disk_bytes"] = json!(bytes);
                    json!(if matches {
                        "matching_fingerprint"
                    } else {
                        "disk_differs"
                    })
                }
                Err(_) => json!("disk_unavailable"),
            };
            report
        })
        .collect();
    json!({
        "project_id":project,
        "scope":"selected_source_files",
        "active_generation":generation,
        "files":files,
        "disk_bytes_per_file_limit":MAX_SOURCE_BYTES,
        "coverage":"Read-only byte observations for requested files against one held source selection. Comparisons use the selected physical revisions and admit inherited files. Each disk read is independent. Project-wide freshness, parser and extracted graph completeness, and vector consistency are separate observations."
    })
}

/// Attach commit-blob evidence to the same managed source and disk observations.
pub(super) fn attach_git_binding(root: &Path, revision: &str, report: &mut Value) {
    let mut observed = || -> anyhow::Result<()> {
        let repo = engram_git::history::GitWalker::open_repo(root)?;
        let workdir = repo.workdir()
            .ok_or_else(|| anyhow::anyhow!("project has no Git worktree"))?
            .canonicalize()?;
        let root = root.canonicalize()?;
        let prefix = root.strip_prefix(workdir)?;
        let tree = repo.find_commit(Oid::from_str(revision)?)?.tree()?;
        if let Some(files) = report["files"].as_array_mut() {
            for file in files {
                if let Some(path) = file["path"].as_str() {
                    file["git"] = git_file(&repo, &tree, &prefix.join(path), file);
                }
            }
        }
        Ok(())
    };
    let status = if observed().is_ok() { "observed" } else { "revision_unavailable" };
    report["git_binding"] = json!({
        "expected_commit": revision,
        "status": status,
        "blob_bytes_per_file_limit": MAX_SOURCE_BYTES,
        "coverage": "Raw Git blob, held indexed fingerprint and observed disk bytes for the requested paths. Checkout line-ending and filter transformations can change disk bytes. HEAD, other paths and runtime behavior are outside this comparison.",
    });
}

fn git_file(repo: &Repository, tree: &Tree<'_>, path: &Path, file: &Value) -> Value {
    let entry = match tree.get_path(path) {
        Ok(entry) => entry,
        Err(error) if error.code() == ErrorCode::NotFound => {
            return json!({"status": "not_in_commit"});
        }
        Err(_) => return json!({"status": "blob_unavailable"}),
    };
    if !matches!(entry.filemode(), 0o100644 | 0o100755) {
        return json!({"status": "not_regular_file"});
    }
    let id = entry.id();
    let Ok((size, ObjectType::Blob)) = repo.odb().and_then(|odb| odb.read_header(id)) else {
        return json!({"status": "blob_unavailable", "blob_oid": id.to_string()});
    };
    if size as u64 > MAX_SOURCE_BYTES {
        return json!({"status": "blob_over_limit", "blob_oid": id.to_string(), "bytes": size});
    }
    let Ok(blob) = repo.find_blob(id) else {
        return json!({"status": "blob_unavailable", "blob_oid": id.to_string()});
    };
    let hash = blake3::hash(blob.content()).to_hex().to_string();
    json!({
        "status": "observed",
        "blob_oid": id.to_string(),
        "blake3": hash,
        "bytes": size,
        "matches_indexed": file["indexed_blake3"].as_str().map(|value| value == hash),
        "matches_disk": file["disk_blake3"].as_str().map(|value| value == hash),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::RelPath;
    use engram_graph::Node;

    #[test]
    fn compares_raw_disk_bytes_at_the_held_physical_revision() {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path();
        let graph = GraphStore::open(&root.join("graph.redb")).unwrap();
        let original = b"Public Const Minimum = 1\r\n";
        let edited = b"Public Const Minimum = 2\r\n";
        let node = |path: &str, generation| Node {
            node_id: format!("file:{path}"),
            node_type: "file".into(),
            name: path.into(),
            namespace: "memory".into(),
            file_path: RelPath::new(path),
            language: "vbnet".into(),
            start_line: 1,
            end_line: 1,
            generation,
            metadata: Some(json!({"file_hash":blake3::hash(original).to_hex().to_string()})),
        };
        std::fs::create_dir(root.join("Site")).unwrap();
        std::fs::write(root.join("Site/Keep.vb"), original).unwrap();
        std::fs::write(root.join("Site/Edit.vb"), edited).unwrap();
        std::fs::write(root.join("Site/Newer.vb"), original).unwrap();
        graph
            .upsert_nodes(
                "owned",
                &[
                    node("Site/Keep.vb", 1),
                    node("Site/Edit.vb", 2),
                    node("Site/Newer.vb", 4),
                    node("Site/Missing.vb", 2),
                ],
            )
            .unwrap();
        let revisions = BTreeMap::from([
            ("Site/Keep.vb".into(), 1),
            ("Site/Edit.vb".into(), 2),
            ("Site/Newer.vb".into(), 2),
            ("Site/Missing.vb".into(), 2),
        ]);
        let paths = validate(&[
            "Site\\Keep.vb".into(),
            "Site/Edit.vb".into(),
            "Site/Newer.vb".into(),
            "Site/Unselected.sql".into(),
            "Site/Missing.vb".into(),
        ])
        .unwrap();
        let report = observe(&graph, "owned", root, 3, Some(&revisions), &paths);
        assert_eq!(report["active_generation"], 3);
        let files = report["files"].as_array().unwrap();
        assert_eq!(files[0]["status"], "matching_fingerprint");
        assert_eq!(files[0]["physical_revision"], 1);
        assert_eq!(files[0]["indexed_blake3"], files[0]["disk_blake3"]);
        assert_eq!(files[1]["status"], "disk_differs");
        assert_eq!(
            files[1]["disk_blake3"],
            blake3::hash(edited).to_hex().to_string()
        );
        assert_eq!(files[2]["status"], "published_fingerprint_unavailable");
        assert_eq!(files[3]["status"], "file_not_selected");
        assert_eq!(files[4]["status"], "disk_unavailable");
    }
}
