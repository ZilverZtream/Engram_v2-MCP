#![allow(clippy::unwrap_used)]
//! Doc-11 P1e (round-2 audit residue): `project_health` prints
//! `tantivy_docs_total` as the sum of a HARDCODED namespace subset
//! (hybrid.rs count_docs_by_namespace) — live it printed 421,293 while the
//! store held 422,249. A doc in any namespace outside the subset
//! (business_logic, code, memory_bank, …) vanishes from the "total". The
//! label must print the project-wide count.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_index::hybrid::IndexDoc;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_total_counts_every_namespace_not_a_hardcoded_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("proj");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join("a.vb"), "Public Class A\nEnd Class\n").unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "totals".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // One doc OUTSIDE the hardcoded subset.
    let ps = engram_server::services::project_service::ensure_project_runtime(&state, &pid)
        .await
        .unwrap();
    {
        let fields = ps.search.fields();
        let mut guard = ps.search.create_bulk_writer().unwrap();
        engram_index::hybrid::HybridSearchEngine::write_docs_to_writer(
            &fields,
            &mut guard,
            &pid,
            &[IndexDoc {
                generation: 1,
                chunk_id: 0,
                path: RelPath::new("rules/r1.md"),
                language: "markdown".into(),
                content: "if the order ships partially then invoice the shipped part".into(),
                namespace: "business_logic".into(),
                author: None,
                timestamp: None,
                start_line: 1,
                end_line: 1,
                doc_id: "bl:r1".into(),
                content_hash: "h-bl-r1".into(),
            }],
        )
        .unwrap();
        guard.finish().unwrap();
    }
    let expected = ps.search.count_docs(&pid).unwrap();

    let res = engram
        .project_health(Parameters(engram_server::ProjectIdRequest {
            project_id: pid.clone(),
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("expected text"),
    };
    let printed: usize = text
        .split("tantivy_docs_total: ")
        .nth(1)
        .expect("label present")
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        printed, expected,
        "the printed total must be the project-wide count (business_logic \
         and every other namespace included), not a hardcoded subset:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_storage_reports_audit_records_added_and_removed_in_health_and_freshness() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Sample.vb"), "Public Class Sample\nEnd Class\n").unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().into_owned(),
            project_name: "availability".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let ps = engram_server::services::project_service::ensure_project_runtime(&state, &pid)
        .await
        .unwrap();
    let mut initial_generation = None;
    for phase in [0, 1, 2] {
        if phase == 1 {
            // Deliberately arbitrary records: count presence cannot certify that
            // contents are valid/approved rules. An unrelated namespace is not counted.
            let docs: Vec<_> = ["quality_gate", "quality_gate", "antipattern", "memory_bank"]
                .iter()
                .enumerate()
                .map(|(i, namespace)| IndexDoc {
                    generation: 0,
                    chunk_id: i as u64,
                    path: RelPath::new(&format!("notes/{i}.md")),
                    language: "markdown".into(),
                    content: format!("Unvalidated generic note {i}"),
                    namespace: (*namespace).into(),
                    author: None,
                    timestamp: None,
                    start_line: 1,
                    end_line: 1,
                    doc_id: format!("availability-{i}"),
                    content_hash: format!("note-{i}"),
                })
                .collect();
            // The index requires each batch to contain one namespace.
            for batch in [&docs[..2], &docs[2..3], &docs[3..]] {
                ps.search
                    .index_docs(&pid, batch, &tokio_util::sync::CancellationToken::new())
                    .await
                    .unwrap();
            }
        } else if phase == 2 {
            assert_eq!(
                ps.search
                    .delete_namespace(&pid, "quality_gate")
                    .await
                    .unwrap(),
                2
            );
            assert_eq!(ps.search.delete_namespace(&pid, "antipattern").await.unwrap(), 1);
        }
        let result = engram
            .project_health(Parameters(engram_server::ProjectIdRequest {
                project_id: pid.clone(),
            }))
            .await
            .unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.starts_with("Health: OK\n"), "{text}");
        assert!(
            text.contains("OK does not imply optional knowledge availability or audit coverage")
        );
        let generation = text
            .lines()
            .find(|line| line.starts_with("active_generation:"))
            .unwrap()
            .to_owned();
        if let Some(initial) = &initial_generation {
            assert_eq!(&generation, initial);
        } else {
            initial_generation = Some(generation);
        }
        if phase == 1 {
            assert!(text.contains("quality_gate_records: 2\n"), "{text}");
            assert!(text.contains("pre_push_audit_knowledge: RECORDS_PRESENT_NOT_VALIDATED"));
            assert!(
                text.contains(
                    "parsing, relevance, approval and audit execution are not established"
                )
            );
        } else {
            assert!(text.contains("quality_gate_records: 0\n"), "{text}");
            assert!(text.contains("pre_push_audit_knowledge: INACTIVE_EMPTY"));
        }
        let freshness = engram.handle_get_index_freshness(serde_json::from_value(serde_json::json!({
            "project_id":pid,"check_disk":true
        })).unwrap()).await.unwrap();
        let fresh_text = &freshness.content[0].as_text().unwrap().text;
        assert!(fresh_text.contains("generation_complete: true"), "{fresh_text}");
        assert!(fresh_text.contains("disk_check: no_changes_detected"), "{fresh_text}");
        assert!(fresh_text.contains("audit_knowledge_scope: optional stored evidence"), "{fresh_text}");
        for report in [text, fresh_text] {
            if phase == 1 {
                assert!(report.contains("antipattern_records: 1\n"), "{report}");
                assert!(report.contains("anti_pattern_knowledge: RECORDS_PRESENT_NOT_VALIDATED"), "{report}");
                assert!(report.contains("quality_gate_records: 2\n"), "{report}");
            } else {
                assert!(report.contains("antipattern_records: 0\n"), "{report}");
                assert!(report.contains("anti_pattern_knowledge: INACTIVE_EMPTY"), "{report}");
                assert!(report.contains("analyze_reverts"), "{report}");
                assert!(report.contains("quality_gate_records: 0\n"), "{report}");
            }
        }
    }
}
