//! SQL guidance retains exact declaration evidence without claiming database state.
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

const GUIDE: &str = "---\napplyTo: **/*.sql\n---\nPreserve `dbo.Orders`, `dbo.OrdersView`, `dbo.OrderAudit` and `GetOrders`.\nException: approved exports remain read-only.\n";
const SQL: &str = "CREATE TABLE dbo.Orders (Id INT NOT NULL, Name VARCHAR(10));\nGO\nCREATE TABLE archive.Orders (Id INT);\nGO\nCREATE VIEW dbo.OrdersView AS SELECT Id FROM dbo.Orders;\nGO\nCREATE TRIGGER dbo.OrderAudit ON dbo.Orders AFTER UPDATE AS INSERT INTO dbo.AuditRows (OrderId) SELECT Id FROM inserted;\nGO\nCREATE PROCEDURE dbo.GetOrders AS SELECT Id FROM dbo.Orders;\nGO\n";

#[tokio::test]
async fn sql_guidance_distinguishes_changed_table_from_unchanged_view_trigger_and_procedure() {
    let temp = match std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT") {
        Some(root) => tempfile::tempdir_in(root).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let root = temp.path().join("repo");
    std::fs::create_dir_all(root.join("rules")).unwrap();
    std::fs::write(root.join("rules/schema.md"), GUIDE).unwrap();
    std::fs::write(root.join("schema.sql"), SQL).unwrap();
    let base = {
        let repo = git2::Repository::init(&root).unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature =
            git2::Signature::now("Owned source fixture", "fixture@example.invalid").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Owned SQL guidance base",
            &tree,
            &[],
        )
        .unwrap()
        .to_string()
    };
    let after = SQL.replace("VARCHAR(10)", "VARCHAR(20)");
    std::fs::write(root.join("schema.sql"), &after).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: "sql-guidance".into(),
            project_name: "SQL guidance".into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    let engram = Engram::new(state.clone());
    let response = engram.handle_pre_push_audit(serde_json::from_value(json!({
        "project_id":"sql-guidance","instruction_maintenance":{
            "base_commit":base,"instruction_paths":["rules/schema.md"],"evidence_paths":["schema.sql"]
        }
    })).unwrap()).await.unwrap();
    let report: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    let instruction = &manifest["instructions"][0];
    let changes = instruction["symbol_changes"].as_array().unwrap();
    assert_eq!(changes.len(), 4, "{report}");
    for (name, kind, changed) in [
        ("dbo.Orders", "table", true),
        ("dbo.OrdersView", "view", false),
        ("dbo.OrderAudit", "trigger", false),
        ("GetOrders", "stored_procedure", false),
    ] {
        let row = changes.iter().find(|row| row["literal"] == name).unwrap();
        assert_eq!(
            row["status"],
            if changed {
                "changed_observed_candidate_bytes"
            } else {
                "same_observed_candidate_bytes"
            }
        );
        for (version, content) in [
            ("before_candidates", SQL),
            ("after_candidates", after.as_str()),
        ] {
            let candidates = row[version].as_array().unwrap();
            assert_eq!(candidates.len(), 1);
            let candidate = &candidates[0];
            assert_eq!(candidate["kind"], kind);
            assert_eq!(candidate["qualified_name"], name);
            assert_eq!(candidate["absence_proof"], false);
            assert_eq!(
                candidate["source_blake3"],
                blake3::hash(content.as_bytes()).to_hex().to_string()
            );
            let start = candidate["byte_range"][0].as_u64().unwrap() as usize;
            let end = candidate["byte_range"][1].as_u64().unwrap() as usize;
            assert_eq!(
                candidate["evidence_blake3"],
                blake3::hash(&content.as_bytes()[start..end])
                    .to_hex()
                    .to_string()
            );
            assert!(content[start..end].contains(name));
        }
    }
    assert_eq!(report["authored_files_written"], 0);
    assert_eq!(
        std::fs::read_to_string(root.join("rules/schema.md")).unwrap(),
        GUIDE
    );
    assert_eq!(report["packet"]["evidence"][0]["before"]["content"], SQL);
    assert_eq!(report["packet"]["evidence"][0]["after"]["content"], after);
    drop(engram);
    drop(state);
    temp.close().unwrap();
}
