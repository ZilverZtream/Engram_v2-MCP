//! Derived file observations. They never publish or certify a source snapshot.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedFileStat {
    pub version: u32,
    pub identity_epoch: u64,
    pub generation: u64,
    pub file_hash: String,
    pub mtime_ns: u64,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileStatPatch {
    pub path: RelPath,
    pub generation: u64,
    pub metadata_digest: String,
    pub stat: VerifiedFileStat,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FileStatPatchReport {
    pub applied: usize,
    pub changed: usize,
    pub missing: usize,
    pub deferred: usize,
}

pub fn file_metadata_digest(metadata: &JsonValue) -> anyhow::Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(metadata)?)
        .to_hex()
        .to_string())
}

impl GraphStore {
    /// A bounded transaction updates only this cache's owned metadata field.
    /// The observation retains its original epoch; the reader must compare it
    /// with current Registry identity. This is not a cross-database commit.
    pub fn patch_verified_file_stats(
        &self,
        project: &str,
        patches: &[FileStatPatch],
    ) -> anyhow::Result<FileStatPatchReport> {
        use bincode::Options;
        validate_key_component("project_id", project)?;
        anyhow::ensure!(
            patches.len() <= 256,
            "File-stat batch exceeds 256 observations"
        );
        anyhow::ensure!(
            serde_json::to_vec(patches)?.len() <= 512 * 1024,
            "File-stat batch exceeds its metadata budget"
        );
        let mut paths = std::collections::HashSet::new();
        for patch in patches {
            let hash_valid =
                |text: &str| text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit());
            validate_key_component("file_path", patch.path.as_str())?;
            anyhow::ensure!(
                paths.insert(patch.path.as_str())
                    && patch.stat.version == 1
                    && patch.stat.identity_epoch != 0
                    && patch.generation != 0
                    && patch.stat.generation == patch.generation
                    && patch.stat.mtime_ns != 0
                    && hash_valid(&patch.stat.file_hash)
                    && hash_valid(&patch.metadata_digest),
                "Invalid or duplicate file-stat observation"
            );
        }
        let mut report = FileStatPatchReport::default();
        if patches.is_empty() {
            return Ok(report);
        }
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(NODES)?;
            for patch in patches {
                let node_id = engram_core::ids::NodeId::file(patch.path.as_str()).0;
                let key = format!("{project}\0{node_id}");
                let existing = {
                    let Some(value) = table.get(key.as_str())? else {
                        report.missing += 1;
                        continue;
                    };
                    if value.value().len() > 4 * 1024 * 1024 {
                        report.deferred += 1;
                        continue;
                    }
                    match bincode::DefaultOptions::new()
                        .with_fixint_encoding()
                        .with_limit(4 * 1024 * 1024)
                        .reject_trailing_bytes()
                        .deserialize::<Node>(value.value())
                    {
                        Ok(node) => node,
                        Err(_) => {
                            // A cache hint cannot authorize dropping an unknown
                            // binary suffix or repairing an unreadable source row.
                            // Preserve it and still warm independent healthy rows.
                            report.deferred += 1;
                            continue;
                        }
                    }
                };
                let mut node = existing;
                let Some(metadata) = node.metadata.as_mut() else {
                    report.changed += 1;
                    continue;
                };
                if node.node_type != "file"
                    || node.namespace != "memory"
                    || node.file_path != patch.path
                    || node.generation != patch.generation
                    || metadata.get("file_hash").and_then(|v| v.as_str())
                        != Some(patch.stat.file_hash.as_str())
                    || file_metadata_digest(metadata)? != patch.metadata_digest
                {
                    report.changed += 1;
                    continue;
                }
                let Some(object) = metadata.as_object_mut() else {
                    report.changed += 1;
                    continue;
                };
                object.insert(
                    "verified_stat_v1".into(),
                    serde_json::to_value(&patch.stat)?,
                );
                table.insert(key.as_str(), bincode::serialize(&node)?.as_slice())?;
                report.applied += 1;
            }
        }
        tx.commit()?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> Node {
        Node {
            node_id: engram_core::ids::NodeId::file("source.rs").0,
            node_type: "file".into(),
            name: "source.rs".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: RelPath::new("source.rs"),
            start_line: 0,
            end_line: 0,
            generation: 2,
            metadata: Some(serde_json::json!({"mtime":1,"mtime_ns":100,
                "size":40,"file_hash":"a".repeat(64),"source_index_version":1,
                "human_note":"preserve this"})),
        }
    }

    fn patch(node: &Node, mtime_ns: u64) -> FileStatPatch {
        FileStatPatch {
            path: node.file_path.clone(),
            generation: node.generation,
            metadata_digest: file_metadata_digest(node.metadata.as_ref().unwrap()).unwrap(),
            stat: VerifiedFileStat {
                version: 1,
                identity_epoch: 7,
                generation: node.generation,
                file_hash: "a".repeat(64),
                mtime_ns,
                size: 40,
            },
        }
    }

    #[test]
    fn stale_stat_observations_cannot_overwrite_newer_hints_or_source_rows() {
        let temporary = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temporary.path().join("graph.redb")).unwrap();
        let original = file();
        graph.upsert_nodes("project", &[original.clone()]).unwrap();
        let old = patch(&original, 200);
        assert_eq!(
            graph
                .patch_verified_file_stats("project", &[old.clone()])
                .unwrap()
                .applied,
            1
        );
        let first = graph
            .get_node("project", &original.node_id)
            .unwrap()
            .unwrap();
        let newer = patch(&first, 300);
        assert_eq!(
            graph
                .patch_verified_file_stats("project", &[newer])
                .unwrap()
                .applied,
            1
        );
        assert_eq!(
            graph
                .patch_verified_file_stats("project", &[old.clone()])
                .unwrap()
                .changed,
            1
        );
        let warmed = graph
            .get_node("project", &original.node_id)
            .unwrap()
            .unwrap();
        assert_eq!(warmed.generation, original.generation);
        let metadata = warmed.metadata.as_ref().unwrap();
        assert_eq!(metadata["human_note"], "preserve this");
        assert_eq!(metadata["mtime_ns"], 100);
        assert_eq!(metadata["verified_stat_v1"]["mtime_ns"], 300);
        let mut published = original;
        published.generation = 3;
        published.metadata.as_mut().unwrap()["file_hash"] = "b".repeat(64).into();
        graph.upsert_nodes("project", &[published.clone()]).unwrap();
        assert_eq!(
            graph
                .patch_verified_file_stats("project", &[old])
                .unwrap()
                .changed,
            1
        );
        let retained = graph
            .get_node("project", &published.node_id)
            .unwrap()
            .unwrap();
        assert_eq!(retained.generation, 3);
        assert_eq!(retained.metadata, published.metadata);
        drop(graph);
        temporary.close().unwrap();
    }

    #[test]
    fn invalid_stat_batches_are_atomic_and_missing_files_are_not_recreated() {
        let temporary = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temporary.path().join("graph.redb")).unwrap();
        let original = file();
        graph.upsert_nodes("project", &[original.clone()]).unwrap();
        let observation = patch(&original, 200);
        assert!(
            graph
                .patch_verified_file_stats("project", &[observation.clone(), observation.clone()])
                .is_err()
        );
        assert_eq!(
            graph
                .get_node("project", &original.node_id)
                .unwrap()
                .unwrap()
                .metadata,
            original.metadata
        );
        assert_eq!(
            graph
                .patch_verified_file_stats("missing-project", &[observation])
                .unwrap()
                .missing,
            1
        );
        assert!(
            graph
                .get_node("missing-project", &original.node_id)
                .unwrap()
                .is_none()
        );
        drop(graph);
        temporary.close().unwrap();
    }

    #[test]
    fn an_opaque_or_unreadable_row_is_preserved_without_losing_healthy_stat_observations() {
        for problem in ["trailing", "truncated", "oversized"] {
            let temporary = tempfile::tempdir().unwrap();
            let graph = GraphStore::open(&temporary.path().join("graph.redb")).unwrap();
            let mut rows = Vec::new();
            for path in ["first.rs", "legacy.rs", "last.rs"] {
                let mut node = file();
                node.node_id = engram_core::ids::NodeId::file(path).0;
                node.file_path = RelPath::new(path);
                node.name = path.into();
                rows.push(node);
            }
            graph.upsert_nodes("project", &rows).unwrap();
            let observations: Vec<_> = rows.iter().map(|row| patch(row, 200)).collect();
            let mut opaque = bincode::serialize(&rows[1]).unwrap();
            match problem {
                "trailing" => {
                    opaque.extend_from_slice(b"\0unrecognized future extension");
                    assert!(
                        bincode::deserialize::<Node>(&opaque).is_ok(),
                        "The normal scan accepts this legacy row"
                    );
                }
                "truncated" => opaque.truncate(12),
                _ => opaque.resize(4 * 1024 * 1024 + 1, 0),
            }
            let key = format!("project\0{}", rows[1].node_id);
            {
                let tx = graph.db.begin_write().unwrap();
                tx.open_table(NODES)
                    .unwrap()
                    .insert(key.as_str(), opaque.as_slice())
                    .unwrap();
                tx.commit().unwrap();
            }
            let report = graph
                .patch_verified_file_stats("project", &observations)
                .unwrap();
            assert_eq!(
                report.applied, 2,
                "Both healthy neighbors should retain their observations: {problem}"
            );
            assert_eq!(report.deferred, 1);
            for row in [&rows[0], &rows[2]] {
                let updated = graph.get_node("project", &row.node_id).unwrap().unwrap();
                let metadata = updated.metadata.unwrap();
                assert_eq!(metadata["verified_stat_v1"]["mtime_ns"], 200);
                assert_eq!(metadata["human_note"], "preserve this");
                assert_eq!(updated.generation, row.generation);
            }
            {
                let tx = graph.db.begin_read().unwrap();
                let table = tx.open_table(NODES).unwrap();
                assert_eq!(
                    table.get(key.as_str()).unwrap().unwrap().value(),
                    opaque.as_slice(),
                    "Derived warming must not normalize, truncate or repair the deferred row"
                );
            }
            drop(graph);
            temporary.close().unwrap();
        }
    }
}
