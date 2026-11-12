#![cfg(feature = "vector")]
#![allow(clippy::unwrap_used)]

use engram_core::{Config, ProjectRecord, RelPath};
use engram_index::{HybridSearchEngine, IndexDoc};
use engram_server::{
    AppState, Engram,
    services::{integrity_service, project_service},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const PID: &str = "source-integrity";

fn doc(namespace: &str, generation: u64, path: &str, id: &str) -> IndexDoc {
    IndexDoc {
        namespace: namespace.into(),
        generation,
        path: RelPath::new(path),
        doc_id: id.into(),
        chunk_id: 1,
        language: "vb".into(),
        content: format!("Public Class {id}\nEnd Class"),
        content_hash: id.into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 2,
    }
}

// Declaration order ensures both databases and their cached runtimes drop
// before the owned directory, including assertion unwinding.
struct Fixture {
    state: AppState,
    _temp: tempfile::TempDir,
}

impl Fixture {
    async fn new(managed: bool, backend: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        std::fs::create_dir(&root).unwrap();
        let data = temp.path().join("data");
        let project = data.join("projects").join(PID);
        let config = Config {
            data_dir: data,
            allowed_roots: vec![root.clone()],
            embedding_backend: backend.into(),
            ..Default::default()
        };
        let seed =
            HybridSearchEngine::new(project.join("tantivy"), project.join("lancedb"), &config)
                .await
                .unwrap();
        let cancel = CancellationToken::new();
        seed.index_docs(
            PID,
            &[
                doc("memory", 1, "a.vb", "a1"),
                doc("memory", 1, "b.vb", "b1"),
            ],
            &cancel,
        )
        .await
        .unwrap();
        if managed {
            seed.prepare_source_snapshot(
                PID,
                1,
                2,
                &[RelPath::new("b.vb"), RelPath::new("c.vb")],
                &[],
                &cancel,
            )
            .await
            .unwrap();
            seed.index_docs(
                PID,
                &[
                    doc("memory", 2, "b.vb", "b2"),
                    doc("memory", 2, "c.vb", "c2"),
                ],
                &cancel,
            )
            .await
            .unwrap();
            // Same path and physical revision as selected source: namespace
            // filtering must exclude this row in both stores.
            seed.index_docs(PID, &[doc("history", 1, "a.vb", "history1")], &cancel)
                .await
                .unwrap();
            seed.seal_source_snapshot(PID, 2).unwrap();
            seed.prepare_source_snapshot(PID, 2, 3, &[RelPath::new("c.vb")], &[], &cancel)
                .await
                .unwrap();
            seed.index_docs(PID, &[doc("memory", 3, "c.vb", "staged3")], &cancel)
                .await
                .unwrap();
        }
        drop(seed);
        let (state, _) = AppState::new(config).unwrap();
        state
            .registry
            .put_project(&ProjectRecord {
                project_id: PID.into(),
                project_name: "VB source integrity".into(),
                project_type: "general".into(),
                directory: root.to_string_lossy().into(),
                created_at_ms: 1,
                updated_at_ms: 1,
                reindex_required_since_ms: None,
            })
            .unwrap();
        state
            .registry
            .set_meta(PID, "active_generation", if managed { "2" } else { "1" })
            .unwrap();
        Self { state, _temp: temp }
    }

    async fn check(&self, repair: bool) -> Value {
        let result = Engram::new(self.state.clone())
            .handle_check_integrity(
                serde_json::from_value(json!({"project_id": PID, "auto_repair": repair})).unwrap(),
            )
            .await
            .unwrap();
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
    }
}

#[tokio::test]
async fn published_source_detects_masked_shortage_and_surplus_and_repairs_both() {
    let fixture = Fixture::new(true, "local").await;
    let runtime = project_service::ensure_project_runtime(&fixture.state, PID)
        .await
        .unwrap();
    let search = &runtime.search;
    let initial = fixture.check(false).await;
    assert_eq!(initial["tantivy_doc_count"], 6);
    assert_eq!(initial["source_generation"]["tantivy_doc_count"], 3);
    assert_eq!(initial["source_generation"]["vector_doc_count"], 3);
    assert_eq!(initial["source_generation"]["counts_match"], true);
    assert_eq!(initial["overall_healthy"], true);

    for shortage in [true, false] {
        let (namespace, path, stray, expected_vector_count, kind) = if shortage {
            (
                "memory",
                "a.vb",
                doc("history", 1, "a.vb", "extra-history"),
                2,
                "VectorShortfall",
            )
        } else {
            (
                "history",
                "a.vb",
                doc("memory", 2, "b.vb", "extra-source"),
                4,
                "VectorOrphan",
            )
        };
        search
            .delete_vector_rows_for_paths(PID, namespace, &[RelPath::new(path)])
            .await
            .unwrap();
        search
            .embed_and_upsert_vectors(PID, &[stray], &CancellationToken::new())
            .await
            .unwrap();
        let observed = fixture.check(false).await;
        assert_eq!(
            observed["tantivy_doc_count"], observed["vector_doc_count"],
            "global totals mask this defect"
        );
        assert_eq!(
            observed["source_generation"]["vector_doc_count"],
            expected_vector_count
        );
        assert_eq!(observed["mismatches"][0]["kind"], kind);
        assert_eq!(observed["mismatches"].as_array().unwrap().len(), 1);
        assert_eq!(observed["overall_healthy"], false);
        let repaired = fixture.check(true).await;
        assert_eq!(
            repaired["repairs_attempted"][0]["success"], true,
            "{repaired}"
        );
        let after = fixture.check(false).await;
        assert_eq!(after["source_generation"]["vector_doc_count"], 3);
        assert_eq!(after["overall_healthy"], true);
        assert_eq!(after["generation"], 2);
        assert_eq!(
            search.count_docs(PID).unwrap(),
            6,
            "recovery preserves retained and staged lexical rows"
        );
        assert_eq!(
            project_service::repair_vectors_if_mismatch(&fixture.state, PID)
                .await
                .unwrap(),
            project_service::VectorRepairOutcome::NoCurrentCountMismatch
        );
    }
}

#[tokio::test]
async fn disabled_embeddings_and_legacy_source_are_explicitly_uncompared() {
    for (managed, backend, coverage) in [
        (true, "fts_only", "embeddings_disabled"),
        (
            false,
            "local",
            "unavailable_managed_source_or_vector_support",
        ),
    ] {
        let fixture = Fixture::new(managed, backend).await;
        let report = fixture.check(false).await;
        assert_eq!(report["source_generation"]["coverage"], coverage);
        assert!(report["source_generation"]["counts_match"].is_null());
        assert!(report["source_generation"]["vector_doc_count"].is_null());
        assert!(report["mismatches"].as_array().unwrap().is_empty());
        assert_eq!(report["overall_healthy"], backend == "fts_only");
    }
}

#[test]
fn source_evidence_retains_distinct_project_signal_without_duplicate_repairs() {
    let source = integrity_service::SourceGenerationIntegrity {
        generation: 2,
        namespace: "memory".into(),
        coverage: "managed_source_snapshot_counts_only".into(),
        tantivy_doc_count: Some(3),
        vector_doc_count: Some(2),
        counts_match: Some(false),
    };
    let mut same = integrity_service::build_integrity_mismatches(200, 0, true);
    integrity_service::include_source_mismatch(&mut same, &source);
    assert_eq!(same.len(), 1);
    assert_eq!((same[0].expected, same[0].actual), (3, 2));
    let mut opposite = integrity_service::build_integrity_mismatches(200, 400, true);
    integrity_service::include_source_mismatch(&mut opposite, &source);
    assert_eq!(opposite.len(), 2);
}
