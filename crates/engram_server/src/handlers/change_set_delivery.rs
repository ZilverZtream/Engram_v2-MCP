//! Bounded delivery of immutable planning evidence. Recovery never reruns planning.

use rmcp::ErrorData as McpError;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(super) const RESPONSE_BYTES: usize = 40 * 1024;
const SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const SNAPSHOTS_PER_PROJECT: usize = 32;
const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

fn failure(message: impl Into<String>) -> McpError {
    McpError::internal_error(message.into(), None)
}

fn directory(data_dir: &Path, project_id: &str) -> Result<PathBuf, McpError> {
    super::super::validate_project_id(project_id)?;
    engram_core::safe_join(data_dir, &format!("responses/change-set/{project_id}"))
        .map_err(|_| failure("Cannot safely access the project-scoped change-set response cache"))
}

fn checked_path(directory: &Path, id: &str) -> Result<PathBuf, McpError> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(McpError::invalid_params(
            "response_id must be a 64-character lowercase BLAKE3 digest",
            None,
        ));
    }
    engram_core::safe_join(directory, &format!("{id}.txt"))
        .map_err(|_| failure("Cannot safely access the requested change-set response snapshot"))
}

fn continuation(project_id: &str, id: &str, offset: usize) -> Value {
    json!({"tool": "get_change_set", "arguments": {
        "project_id": project_id, "story": "", "response_id": id,
        "response_offset": offset, "output_json": true
    }})
}

fn prune(directory: &Path, keep: &Path) {
    // Only remove checked regular files owned by this cache; never recurse.
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut snapshots = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let id = name.strip_suffix(".txt")?;
            let path = checked_path(directory, id).ok()?;
            let metadata = std::fs::symlink_metadata(&path).ok()?;
            (metadata.is_file() && !metadata.file_type().is_symlink() && path != keep)
                .then(|| (path, metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
        })
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    for (index, (path, modified)) in snapshots.into_iter().enumerate() {
        if index >= SNAPSHOTS_PER_PROJECT - 1 || modified.elapsed().is_ok_and(|age| age > RETENTION)
        {
            // Recheck the final target immediately before deleting it.
            if let Some(id) = path.file_stem().and_then(|name| name.to_str())
                && let Ok(checked) = checked_path(directory, id)
                && checked == path
            {
                let _ = std::fs::remove_file(checked);
            }
        }
    }
}

fn save(data_dir: &Path, project_id: &str, body: &str) -> Result<String, McpError> {
    if body.len() > SNAPSHOT_BYTES {
        return Err(failure(
            "Change-set evidence exceeds the 8 MiB snapshot limit; narrow the story or concepts. No evidence was silently truncated.",
        ));
    }
    let directory = directory(data_dir, project_id)?;
    std::fs::create_dir_all(&directory)
        .map_err(|_| failure("Cannot create the change-set response cache"))?;
    // Validate again after creation so existing symlink components cannot redirect writes.
    let directory = self::directory(data_dir, project_id)?;
    // tempfile's Windows persistence calls native APIs with the supplied paths.
    // Canonical paths carry the extended-length prefix needed beyond MAX_PATH.
    // Both the temporary file and final filename must use this same directory.
    let root = std::fs::canonicalize(data_dir)
        .map_err(|error| failure(format!("Cannot resolve the response cache data root: {error}")))?;
    let directory = std::fs::canonicalize(directory)
        .map_err(|error| failure(format!("Cannot resolve the response cache directory: {error}")))?;
    if !directory.starts_with(root) {
        return Err(failure("Response cache directory resolves outside its data root"));
    }
    let id = blake3::hash(body.as_bytes()).to_hex().to_string();
    let target = checked_path(&directory, &id)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)
        .map_err(|_| failure("Cannot create an atomic change-set response snapshot"))?;
    temporary
        .write_all(body.as_bytes())
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|_| failure("Cannot persist the change-set response snapshot"))?;
    match temporary.persist_noclobber(&target) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = Vec::new();
            engram_core::safe_open_read(&directory, &format!("{id}.txt"))
                .map_err(|_| failure("Cannot safely verify the existing response snapshot"))?
                .take(SNAPSHOT_BYTES as u64 + 1)
                .read_to_end(&mut existing)
                .map_err(|_| failure("Cannot verify the existing response snapshot"))?;
            if existing.len() > SNAPSHOT_BYTES || blake3::hash(&existing).to_hex().as_str() != id {
                return Err(failure(
                    "Existing response snapshot failed its BLAKE3 identity check",
                ));
            }
            // Refresh retention atomically with identical bytes. An unchanged
            // dossier must not return a recovery ID that has already expired.
            error
                .file
                .persist(&target)
                .map_err(|_| failure("Cannot refresh the identical response snapshot"))?;
        }
        Err(error) => {
            return Err(failure(format!(
                "Cannot atomically persist the change-set response snapshot: {}", error.error,
            )));
        }
    }
    prune(&directory, &target);
    Ok(id)
}

/// Leave small responses byte-for-byte unchanged. Oversized responses retain
/// every mandatory identity; their full original evidence has immutable recovery.
pub(super) fn bounded(
    data_dir: &Path,
    project_id: &str,
    body: String,
    output_json: bool,
    mut manifest: Value,
) -> Result<String, McpError> {
    if body.len() <= RESPONSE_BYTES {
        return Ok(body);
    }
    let id = save(data_dir, project_id, &body)?;
    let delivery = json!({
        "status": "DEFERRED_CONTEXT", "complete": false,
        "response_id": id, "original_bytes": body.len(),
        "original_format": if output_json {"json"} else {"markdown"},
        "response_budget_bytes": RESPONSE_BYTES,
        "retention": "At most 24 hours or 32 newer project snapshots; missing snapshots require a fresh planning call and fresh receipt reconciliation.",
        "instruction": "The complete original response is deferred, not absent. Recover it before completing intake or validating the feature contract. Concatenate recovery content in byte-offset order; the BLAKE3 digest binds the exact original UTF-8 response. Recovery does not certify that the current checkout is unchanged.",
        "next_call": continuation(project_id, &id, 0),
    });
    manifest["response_delivery"] = delivery;
    let mut rendered = serde_json::to_string(&manifest)
        .map_err(|_| failure("Cannot render change-set delivery manifest"))?;
    const MARKDOWN_PREFIX: &str = "# Change set: deferred evidence\n\nEvery mandatory identity is retained below. Recover the full dossier before completing intake.\n\n```json\n";
    const MARKDOWN_SUFFIX: &str = "\n```\n";
    let wrapper_bytes = if output_json {
        0
    } else {
        MARKDOWN_PREFIX.len() + MARKDOWN_SUFFIX.len()
    };
    if rendered.len() + wrapper_bytes > RESPONSE_BYTES {
        // Paths and checkpoint bodies may be arbitrarily long; their identities
        // remain inline, while their exact content stays in the snapshot.
        if let Some(rows) = manifest["reconciliation_rows"].as_array_mut() {
            for row in rows {
                if let Some(object) = row.as_object_mut() {
                    object.remove("path");
                }
            }
        }
        manifest["contract_checkpoint"] = Value::Null;
        manifest["response_delivery"]["checkpoint_content_deferred"] = json!(true);
        rendered = serde_json::to_string(&manifest)
            .map_err(|_| failure("Cannot render change-set delivery manifest"))?;
    }
    let rendered = if output_json {
        rendered
    } else {
        format!("{MARKDOWN_PREFIX}{rendered}{MARKDOWN_SUFFIX}")
    };
    if rendered.len() > RESPONSE_BYTES {
        return Err(failure(
            "Mandatory change-set identities exceed the response budget; narrow the story or configured hard rules. No receipt or mandatory identity was silently removed.",
        ));
    }
    Ok(rendered)
}

pub(super) fn recover(
    data_dir: &Path,
    project_id: &str,
    id: &str,
    offset: usize,
) -> Result<String, McpError> {
    let directory = directory(data_dir, project_id)?;
    let path = checked_path(&directory, id)?;
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| McpError::invalid_params("Response snapshot unavailable; rerun the original planning request and reconcile the new receipt", None))?;
    if !metadata.is_file() || metadata.len() > SNAPSHOT_BYTES as u64 {
        return Err(failure(
            "Response snapshot is not a supported bounded regular file",
        ));
    }
    if metadata
        .modified()
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age > RETENTION)
    {
        return Err(McpError::invalid_params(
            "Response snapshot expired; rerun planning and reconcile the new receipt",
            None,
        ));
    }
    let mut bytes = Vec::new();
    engram_core::safe_open_read(&directory, &format!("{id}.txt"))
        .map_err(|_| failure("Cannot safely open the change-set response snapshot"))?
        .take(SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure("Cannot read the change-set response snapshot"))?;
    if bytes.len() > SNAPSHOT_BYTES {
        return Err(failure("Response snapshot exceeds its byte limit"));
    }
    if blake3::hash(&bytes).to_hex().as_str() != id {
        return Err(failure(
            "Response snapshot failed its BLAKE3 identity check; no evidence returned",
        ));
    }
    let body = String::from_utf8(bytes).map_err(|_| failure("Response snapshot is not UTF-8"))?;
    if offset > body.len() || !body.is_char_boundary(offset) {
        return Err(McpError::invalid_params(
            "response_offset must be a UTF-8 byte boundary within the original response",
            None,
        ));
    }
    let mut end = offset.saturating_add(RESPONSE_BYTES / 2).min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    loop {
        let complete = end == body.len();
        let page = json!({
            "response_id": id, "original_bytes": body.len(),
            "start_byte": offset, "end_byte": end,
            "complete": complete, "content": &body[offset..end],
            "evidence_scope": "Immutable original response; current checkout freshness is not re-evaluated",
            "next_call": if complete {Value::Null} else {continuation(project_id, id, end)},
        });
        let rendered = serde_json::to_string(&page)
            .map_err(|_| failure("Cannot render response recovery page"))?;
        if rendered.len() <= RESPONSE_BYTES {
            return Ok(rendered);
        }
        end = offset + (end - offset) / 2;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn deeply_nested_data_directory_preserves_exact_response_recovery() {
        let temporary = tempfile::tempdir().unwrap();
        let mut data = temporary.path().to_path_buf();
        for _ in 0..6 { data.push("nested-response-data-location-000000000000"); }
        std::fs::create_dir_all(&data).unwrap();
        let body = "Preserve the complete source policy and its exceptions.\n".repeat(1_000);
        let response = super::bounded(&data, "project", body.clone(), true, super::json!({})).unwrap();
        let value: super::Value = serde_json::from_str(&response).unwrap();
        let id = value["response_delivery"]["response_id"].as_str().unwrap();
        assert_eq!(recover_all(&data, "project", id), body);
    }
    use super::*;

    fn manifest() -> Value {
        json!({
            "reconciliation": {"receipt_id": "sha256:unaltered", "required_rows": 120},
            "reconciliation_rows": (0..120).map(|i| json!({"row_id":format!("P{i:03}"), "path":format!("src/{i}.rs")})).collect::<Vec<_>>(),
            "contract_checkpoint": {"receipt": {"receipt_id":"sha256:checkpoint"}, "hard_items":[{"id":"RULE-001","requirement":"Preserve this exact requirement"}]},
            "contract_checkpoint_identity": {"receipt_id":"sha256:checkpoint", "hard_item_ids":["RULE-001"]}
        })
    }

    #[test]
    fn bounded_delivery_preserves_identities_and_recovers_exact_unicode_json() {
        let tmp = tempfile::tempdir().unwrap();
        let original =
            serde_json::to_string(&json!({"rules": "å🦀\\\"\n".repeat(18_000)})).unwrap();
        let first = bounded(tmp.path(), "project", original.clone(), true, manifest()).unwrap();
        assert!(first.len() <= RESPONSE_BYTES);
        let first: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(first["reconciliation"], manifest()["reconciliation"]);
        assert_eq!(
            first["reconciliation_rows"],
            manifest()["reconciliation_rows"]
        );
        assert_eq!(
            first["contract_checkpoint"],
            manifest()["contract_checkpoint"]
        );
        let id = first["response_delivery"]["response_id"].as_str().unwrap();
        let mut recovered = String::new();
        let mut offset = 0;
        loop {
            let text = recover(tmp.path(), "project", id, offset).unwrap();
            assert!(text.len() <= RESPONSE_BYTES);
            let page: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(page["start_byte"], offset);
            recovered.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            offset = page["end_byte"].as_u64().unwrap() as usize;
        }
        assert_eq!(recovered, original);
        assert!(recover(tmp.path(), "other", id, 0).is_err());
        assert!(recover(tmp.path(), "project", "../escape", 0).is_err());
        let path = checked_path(&directory(tmp.path(), "project").unwrap(), id).unwrap();
        std::fs::write(path, "tampered").unwrap();
        assert!(recover(tmp.path(), "project", id, 0).is_err());
    }

    #[test]
    fn markdown_and_small_outputs_keep_their_delivery_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let small = "# Existing output\n".to_string();
        assert_eq!(
            bounded(tmp.path(), "project", small.clone(), false, manifest()).unwrap(),
            small
        );
        let original = format!("# Original dossier\n{}", "evidence\n".repeat(10_000));
        let text = bounded(tmp.path(), "project", original, false, manifest()).unwrap();
        assert!(text.len() <= RESPONSE_BYTES);
        assert!(text.contains("sha256:unaltered"));
        assert!(text.contains("P119"));
        assert!(text.contains("RULE-001"));
    }

    #[test]
    fn invalid_utf8_offset_is_rejected_without_recomputing_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let body = "🦀".repeat(15_000);
        let id = save(tmp.path(), "project", &body).unwrap();
        assert!(recover(tmp.path(), "project", &id, 1).is_err());
        assert!(recover(tmp.path(), "project", &id, body.len() + 1).is_err());
    }

    fn recover_all(root: &Path, project: &str, id: &str) -> String {
        let mut body = String::new();
        loop {
            let offset = body.len();
            let text = recover(root, project, id, offset).unwrap();
            assert!(text.len() <= RESPONSE_BYTES);
            let page: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(page["start_byte"], offset);
            body.push_str(page["content"].as_str().unwrap());
            assert_eq!(page["end_byte"], body.len());
            if page["complete"] == true {
                break;
            }
            assert!(body.len() > offset, "recovery must make forward progress");
        }
        assert_eq!(blake3::hash(body.as_bytes()).to_hex().as_str(), id);
        body
    }

    #[test]
    fn large_checkpoint_is_recoverable_without_losing_mandatory_identities() {
        let temporary = tempfile::tempdir().unwrap();
        let mut expected = manifest();
        expected["contract_checkpoint"]["hard_items"][0]["requirement"] =
            json!("Keep the requirement and its exception. ".repeat(4_000));
        let original = serde_json::to_string(&expected).unwrap();
        let text = bounded(
            temporary.path(),
            "project",
            original.clone(),
            true,
            expected.clone(),
        )
        .unwrap();
        assert!(text.len() <= RESPONSE_BYTES);
        let first: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(first["reconciliation"], expected["reconciliation"]);
        assert_eq!(
            first["contract_checkpoint_identity"],
            expected["contract_checkpoint_identity"]
        );
        assert!(first["contract_checkpoint"].is_null());
        assert_eq!(
            first["response_delivery"]["checkpoint_content_deferred"],
            true
        );
        let ids = |value: &Value| {
            value["reconciliation_rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["row_id"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&first), ids(&expected));
        assert_eq!(
            recover_all(
                temporary.path(),
                "project",
                first["response_delivery"]["response_id"].as_str().unwrap()
            ),
            original
        );
    }

    #[test]
    fn raw_markdown_with_control_characters_and_unicode_recovers_exactly() {
        let temporary = tempfile::tempdir().unwrap();
        let original = format!(
            "# Dossier\n{}",
            "quote=\" backslash=\\ newline=\n tab=\t control=\u{0001} \u{00e5}\u{1f980}\n"
                .repeat(3_000)
        );
        let first = bounded(
            temporary.path(),
            "project",
            original.clone(),
            false,
            manifest(),
        )
        .unwrap();
        assert!(first.len() <= RESPONSE_BYTES);
        let id = blake3::hash(original.as_bytes()).to_hex().to_string();
        assert_eq!(recover_all(temporary.path(), "project", &id), original);
    }

    #[test]
    fn cache_retention_is_project_scoped_and_preserves_the_latest_capture() {
        let temporary = tempfile::tempdir().unwrap();
        let sibling = save(temporary.path(), "sibling", "sibling evidence").unwrap();
        let cache = directory(temporary.path(), "project").unwrap();
        let mut ids = Vec::new();
        for number in 0..=SNAPSHOTS_PER_PROJECT {
            ids.push(save(temporary.path(), "project", &format!("evidence {number}")).unwrap());
        }
        assert_eq!(
            ids.iter()
                .filter(|id| recover(temporary.path(), "project", id, 0).is_ok())
                .count(),
            SNAPSHOTS_PER_PROJECT
        );
        assert_eq!(
            recover_all(temporary.path(), "project", ids.last().unwrap()),
            format!("evidence {SNAPSHOTS_PER_PROJECT}")
        );
        assert_eq!(
            recover_all(temporary.path(), "sibling", &sibling),
            "sibling evidence"
        );
        let sentinel = cache.join("not-a-response.txt");
        std::fs::write(&sentinel, "unrelated file").unwrap();
        save(temporary.path(), "project", "latest evidence").unwrap();
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unrelated file");
    }

    #[test]
    fn repeated_capture_refreshes_expired_evidence_without_changing_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let body = "exact original evidence";
        let id = save(temporary.path(), "project", body).unwrap();
        let path = checked_path(&directory(temporary.path(), "project").unwrap(), &id).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
        )
        .unwrap();
        drop(file);
        assert!(recover(temporary.path(), "project", &id, 0).is_err());
        assert_eq!(save(temporary.path(), "project", body).unwrap(), id);
        assert_eq!(recover_all(temporary.path(), "project", &id), body);
    }
}
