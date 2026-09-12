#![allow(clippy::unwrap_used)]
use engram_core::config::Config;
use engram_server::{AppState, services::pre_commit_review_service::{
    ReviewConfig, run_pre_commit_review_with, gates::AddedConventionsGate,
}};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_conventions_gate_accepts_attribute_docs_and_retains_missing_doc_finding() {
    for documented in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let config = Config { data_dir: tmp.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default() };
        let (state, _) = AppState::new(config).unwrap();
        let pid = "documentation-attributes";
        state.registry.put_project(&engram_core::ProjectRecord {
            project_id: pid.into(), project_name: pid.into(), directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(), created_at_ms: 0, updated_at_ms: 0, reindex_required_since_ms: None,
        }).unwrap();
        state.registry.set_meta(pid, "active_generation", "1").unwrap();
        let header = if documented { "''' <summary>Input.</summary>\n" } else { "" };
        let source = format!("{header}<Validator(GetType(InputValidator))>\nPublic Class Input\n\
            ''' <summary>A.</summary>\nPublic Sub A()\nEnd Sub\n\
            ''' <summary>B.</summary>\nPublic Sub B()\nEnd Sub\n\
            ''' <summary>C.</summary>\nPublic Sub C()\nEnd Sub\nEnd Class\n");
        std::fs::write(root.join("Input.vb"), &source).unwrap();
        let mut diff = format!("diff --git a/Input.vb b/Input.vb\nnew file mode 100644\n--- /dev/null\n+++ b/Input.vb\n@@ -0,0 +1,{} @@\n", source.lines().count());
        for line in source.lines() { diff.push('+'); diff.push_str(line); diff.push('\n'); }
        let (findings, count, files, outcomes) = run_pre_commit_review_with(
            &state, pid, &root, 1, &diff, &ReviewConfig::default(), vec![Box::new(AddedConventionsGate)],
        ).await.unwrap();
        assert_eq!((count, files, outcomes.len()), (1, 1, 1));
        let docs: Vec<_> = findings.iter().filter(|f| f.title.contains("missing doc comments")).collect();
        assert_eq!(docs.len(), usize::from(!documented), "{findings:?}");
        if let Some(finding) = docs.first() { assert!(finding.detail.contains("`Input`"), "{finding:?}"); }
    }
}
