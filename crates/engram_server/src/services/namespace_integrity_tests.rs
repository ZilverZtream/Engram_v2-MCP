use super::{integrity_service, namespace_integrity, project_service};
use crate::{AppState, Engram};
use engram_core::{Config, ProjectRecord, RelPath};
use engram_index::{HybridSearchEngine, IndexDoc};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn doc(namespace: &str, generation: u64, id: &str) -> IndexDoc {
    IndexDoc {
        namespace: namespace.into(),
        generation,
        path: RelPath::new(&format!("{id}.vb")),
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

// Database/runtime owners are dropped before their directory, including unwind.
struct Fixture {
    state: AppState,
    _temp: tempfile::TempDir,
}
impl Fixture {
    async fn new(backend: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        std::fs::create_dir(&root).unwrap();
        let data = temp.path().join("data");
        let project = data.join("projects").join("namespace-check");
        let cfg = Config {
            data_dir: data,
            allowed_roots: vec![root.clone()],
            embedding_backend: backend.into(),
            ..Default::default()
        };
        let seed = HybridSearchEngine::new(project.join("tantivy"), project.join("lancedb"), &cfg)
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        seed.index_docs(
            "namespace-check",
            &[doc("memory", 1, "a"), doc("memory", 1, "b")],
            &cancel,
        )
        .await
        .unwrap();
        seed.prepare_source_snapshot(
            "namespace-check",
            1,
            2,
            &[RelPath::new("b.vb")],
            &[],
            &cancel,
        )
        .await
        .unwrap();
        seed.index_docs("namespace-check", &[doc("memory", 2, "b")], &cancel)
            .await
            .unwrap();
        seed.seal_source_snapshot("namespace-check", 2).unwrap();
        for namespace in [
            "history",
            "memory_bank",
            "business_logic",
            "insights",
            "wontfix_patterns",
            "antipattern",
            "quality_gate",
        ] {
            // Retained old global generations must count too.
            seed.index_docs("namespace-check", &[doc(namespace, 7, namespace)], &cancel)
                .await
                .unwrap();
        }
        seed.index_docs("foreign", &[doc("review_verdict", 0, "foreign")], &cancel)
            .await
            .unwrap();
        drop(seed);
        let (state, _) = AppState::new(cfg).unwrap();
        state
            .registry
            .put_project(&ProjectRecord {
                project_id: "namespace-check".into(),
                project_name: "Owned namespace integrity".into(),
                project_type: "general".into(),
                directory: root.to_string_lossy().into(),
                created_at_ms: 1,
                updated_at_ms: 1,
                reindex_required_since_ms: None,
            })
            .unwrap();
        state
            .registry
            .set_meta("namespace-check", "active_generation", "2")
            .unwrap();
        Self { state, _temp: temp }
    }
    async fn check(&self, repair: bool) -> Value {
        let result = Engram::new(self.state.clone())
            .handle_check_integrity(
                serde_json::from_value(
                    json!({"project_id":"namespace-check", "auto_repair":repair}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
    }
}

#[cfg(feature = "vector")]
#[tokio::test]
async fn namespace_integrity_balanced_totals_repair_once_preserving_source_and_foreign_rows() {
    let fixture = Fixture::new("local").await;
    let runtime = project_service::ensure_project_runtime(&fixture.state, "namespace-check")
        .await
        .unwrap();
    let search = &runtime.search;
    let before = fixture.check(false).await;
    assert_eq!(before["overall_healthy"], true);
    assert_eq!(
        before["retained_namespaces"]["namespaces"]
            .as_object()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(
        search.count_all_namespace_docs("foreign").unwrap()["review_verdict"],
        1
    );
    let foreign = search
        .count_namespace_vectors("foreign", vec!["review_verdict".into()])
        .await
        .unwrap()
        .unwrap();
    search
        .delete_vector_rows_for_paths(
            "namespace-check",
            "memory_bank",
            &[RelPath::new("memory_bank.vb")],
        )
        .await
        .unwrap();
    search
        .embed_and_upsert_vectors(
            "namespace-check",
            &[doc("review_verdict", 0, "orphan")],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let broken = fixture.check(false).await;
    assert_eq!(broken["tantivy_doc_count"], broken["vector_doc_count"]);
    assert_eq!(broken["source_generation"]["counts_match"], true);
    assert_eq!(
        broken["retained_namespaces"]["namespaces"]["memory_bank"]["vector_doc_count"],
        0
    );
    assert_eq!(
        broken["retained_namespaces"]["namespaces"]["review_verdict"]["tantivy_doc_count"],
        0
    );
    assert_eq!(broken["mismatches"].as_array().unwrap().len(), 2);
    assert_eq!(broken["overall_healthy"], false);
    let repaired = fixture.check(true).await;
    assert_eq!(repaired["overall_healthy"], true, "{repaired}");
    let repairs = repaired["repairs_attempted"].as_array().unwrap();
    assert_eq!(repairs.len(), 2);
    assert_eq!(
        repairs
            .iter()
            .filter(|repair| repair["action"] == "vector_recovery")
            .count(),
        1
    );
    assert!(repairs.iter().all(|repair| repair["success"] == true));
    assert_eq!(
        repaired["post_repair"]["retained_namespaces"]["counts_match"],
        true
    );
    assert_eq!(
        repaired["post_repair"]["source_generation"],
        before["source_generation"]
    );
    assert_eq!(
        repaired["retained_namespaces"], broken["retained_namespaces"],
        "before counts retained"
    );
    assert_eq!(
        fixture.check(false).await["retained_namespaces"],
        before["retained_namespaces"]
    );
    assert_eq!(
        search.count_all_namespace_docs("foreign").unwrap()["review_verdict"],
        1
    );
    let foreign_after = search
        .count_namespace_vectors("foreign", vec!["review_verdict".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(foreign.total, foreign_after.total);
    assert_eq!(foreign.by_namespace, foreign_after.by_namespace);
    assert_eq!(
        project_service::repair_vectors_if_mismatch(&fixture.state, "namespace-check")
            .await
            .unwrap(),
        project_service::VectorRepairOutcome::NoCurrentCountMismatch
    );
}

#[cfg(feature = "vector")]
#[tokio::test]
async fn namespace_integrity_vector_only_unknown_namespace_is_reported_and_recovered() {
    let fixture = Fixture::new("local").await;
    let runtime = project_service::ensure_project_runtime(&fixture.state, "namespace-check")
        .await
        .unwrap();
    runtime
        .search
        .embed_and_upsert_vectors(
            "namespace-check",
            &[doc("legacy_import", 0, "orphan")],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let broken = fixture.check(false).await;
    assert_eq!(broken["retained_namespaces"]["unattributed_vector_rows"], 1);
    assert_eq!(broken["mismatches"][0]["kind"], "VectorOrphan");
    assert_eq!(broken["overall_healthy"], false);
    let repaired = fixture.check(true).await;
    assert_eq!(
        repaired["post_repair"]["retained_namespaces"]["unattributed_vector_rows"],
        0
    );
    assert_eq!(repaired["overall_healthy"], true);
}

#[tokio::test]
async fn namespace_integrity_disabled_embeddings_keep_complete_lexical_inventory_without_claiming_vectors()
 {
    let fixture = Fixture::new("fts_only").await;
    let result = fixture.check(false).await;
    let namespaces = &result["retained_namespaces"];
    assert_eq!(namespaces["coverage"], "embeddings_disabled");
    assert!(namespaces["counts_match"].is_null());
    assert!(namespaces["vector_doc_count"].is_null());
    assert_eq!(
        namespaces["namespaces"]["business_logic"]["tantivy_doc_count"],
        1
    );
    assert!(namespaces["namespaces"]["business_logic"]["vector_doc_count"].is_null());
    assert_eq!(
        namespaces["namespaces"]["business_logic"]["compared"],
        false
    );
    assert!(result["mismatches"].as_array().unwrap().is_empty());
    assert_eq!(result["overall_healthy"], true);
}

#[cfg(not(feature = "vector"))]
#[tokio::test]
async fn namespace_integrity_uncompiled_vectors_are_unknown() {
    let fixture = Fixture::new("local").await;
    let result = fixture.check(false).await;
    assert_eq!(
        result["retained_namespaces"]["coverage"],
        "vector_support_unavailable"
    );
    assert!(result["retained_namespaces"]["counts_match"].is_null());
    assert_eq!(result["overall_healthy"], false);
}

#[test]
fn namespace_integrity_no_duplicate_repairs_and_unavailable_is_not_empty() {
    let source = integrity_service::SourceGenerationIntegrity {
        generation: 2,
        namespace: "memory".into(),
        coverage: "managed_source_snapshot_counts_only".into(),
        tantivy_doc_count: Some(2),
        vector_doc_count: Some(1),
        counts_match: Some(false),
        matching_vector_ids: None,
    };
    let mut namespaces = namespace_integrity::NamespaceIntegrity {
        coverage: "retained_non_memory_namespace_counts_only".into(),
        tantivy_doc_count: 300,
        vector_doc_count: Some(0),
        namespaces: Default::default(),
        unattributed_vector_rows: Some(0),
        counts_match: Some(false),
    };
    for name in ["memory_bank", "history", "business_logic"] {
        namespaces.namespaces.insert(
            name.into(),
            namespace_integrity::NamespaceCounts {
                tantivy_doc_count: 10,
                vector_doc_count: Some(0),
                compared: true,
            },
        );
    }
    let found = integrity_service::observed_mismatches(&source, &namespaces, true);
    assert_eq!(found.len(), 1);
    for name in ["memory_bank", "history", "business_logic"] {
        assert!(found[0].description.contains(name));
    }
    namespaces.vector_doc_count = None;
    namespaces.counts_match = None;
    for counts in namespaces.namespaces.values_mut() {
        counts.compared = false;
        counts.vector_doc_count = None;
    }
    let mut mismatches = Vec::new();
    namespace_integrity::include_mismatches(&mut mismatches, &namespaces);
    assert!(
        mismatches.is_empty(),
        "unavailable observations cannot be treated as zero vectors"
    );
}
