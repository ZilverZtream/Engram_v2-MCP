#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P0-2) — follow-up from the live r35
//! run: the path-set check found 5 "missing" paths on OciusX that were REAL —
//! Latin-1 source files skipped at ingest as "Invalid UTF-8 encoding" (they got
//! a graph File node from the fingerprint, no Tantivy/LanceDB document, and
//! none of their content was searchable) — and the tolerance (max(3, 1 %))
//! called the generation "complete" anyway.
//!
//! Now: a non-UTF-8 source file is decoded lossily and indexed; files the
//! indexer skips BY RULE (binary, too large, unreadable) are recorded in a
//! per-generation ledger and subtracted from the expectation; the tolerance
//! is ZERO — a missing path is never "complete".

use engram_core::config::Config;
use engram_index::hybrid::HybridQuery;
use engram_server::models::{GetIndexFreshnessRequest, ProjectIdRequest};
use engram_server::services::project_service::{ensure_project_runtime, get_active_generation};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

async fn build() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    std::fs::create_dir_all(root.join("Site/modules/~.css")).unwrap();
    for i in 0..6 {
        std::fs::write(
            root.join(format!("Site/App_Code/mod{i}.vb")),
            format!("Public Class mod{i}\n    Public Function Get{i}() As String\n        Return \"x\"\n    End Function\nEnd Class\n"),
        )
        .unwrap();
    }
    // A Latin-1 stylesheet: `ä` is the single byte 0xE4 — invalid UTF-8.
    let mut latin1 = b"/* best".to_vec();
    latin1.push(0xE4);
    latin1.extend_from_slice(b"mmer schemafargen */\n.schema1 { background: #ffcc00; }\n.schema2 { background: #00ccff; }\n");
    std::fs::write(root.join("Site/modules/~.css/custom.css"), &latin1).unwrap();
    // A Latin-1 VB file with a Swedish comment.
    let mut vb = b"Public Class Personalliggare\n    ' Anst".to_vec();
    vb.push(0xE4);
    vb.extend_from_slice(b"llda per arbetsplats\n    Public Function LiggareCount() As Integer\n        Return 1\n    End Function\nEnd Class\n");
    std::fs::write(root.join("Site/App_Code/personalliggare.vb"), &vb).unwrap();
    // A binary blob with a code extension: skipped BY RULE (NUL bytes).
    let mut bin = b"var x = 1;".to_vec();
    bin.extend_from_slice(&[0u8; 64]);
    bin.extend_from_slice(b"\nvar y = 2;\n");
    std::fs::write(root.join("Site/App_Code/blob.js"), &bin).unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(100),
        max_project_bytes: Some(4 * 1024 * 1024),
        embedding_backend: "local".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "SkipLedger".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_latin1_source_file_is_indexed_lossily_and_searchable() {
    let (_tmp, state, _engram, pid) = build().await;
    let ps = ensure_project_runtime(&state, &pid).await.unwrap();
    let gen_ = get_active_generation(&state, &pid).await.unwrap();
    for (needle, file) in [
        ("schema1", "custom.css"),
        ("LiggareCount", "personalliggare.vb"),
    ] {
        let hits = ps
            .search
            .lexical_search(&HybridQuery {
                project_id: pid.clone(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                generation: gen_,
                text: needle.into(),
                top_k: 5,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                include_path_suffixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            })
            .unwrap();
        assert!(
            hits.iter().any(|h| h.path.as_str().ends_with(file)),
            "`{needle}` from the Latin-1 file {file} must be searchable (lossy decode, not a skip): {:?}",
            hits.iter()
                .map(|h| h.path.as_str().to_string())
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn known_skips_are_a_ledger_and_the_tolerance_is_zero() {
    let (_tmp, _state, engram, pid) = build().await;
    let req: ProjectIdRequest = serde_json::from_value(json!({"project_id": pid})).unwrap();
    let res = engram.handle_project_health(req).await.unwrap();
    let h = res.content[0].as_text().unwrap().text.clone();
    assert!(h.contains("Health: OK"), "{h}");
    assert!(
        h.contains("missing: 0"),
        "the Latin-1 files are indexed and the binary blob is a KNOWN skip, not a missing path:\n{h}"
    );
    assert!(
        h.contains("skipped by rule: 1") && h.to_lowercase().contains("binary"),
        "the skip ledger is reported with its reason:\n{h}"
    );
    assert!(
        !h.contains("tolerance 3") && !h.contains("≤ tolerance"),
        "no tolerance can hide a missing path any more:\n{h}"
    );
    let req: GetIndexFreshnessRequest =
        serde_json::from_value(json!({"project_id": pid, "check_disk": false})).unwrap();
    let res = engram.handle_get_index_freshness(req).await.unwrap();
    let f = res.content[0].as_text().unwrap().text.clone();
    assert!(
        f.contains("generation_complete: true") && f.contains("skipped by rule 1"),
        "{f}"
    );
}
