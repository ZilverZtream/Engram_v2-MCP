#![allow(clippy::unwrap_used)]
use engram_core::{Config, RelPath};
use engram_server::services::pre_commit_review_service::{
    Gate, GateContext, ReviewConfig, ReviewFinding, run_pre_commit_review_with,
};
use engram_server::{state::AppState, tools::Engram};
use rmcp::handler::server::tool::Parameters;
use std::sync::{Arc, Mutex};

struct ObserveCompleteness(Arc<Mutex<Option<Option<String>>>>);
impl Gate for ObserveCompleteness {
    fn name(&self) -> &'static str { "observe_completeness" }
    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        *self.0.lock().unwrap() = Some(ctx.search_index_note.clone());
        Ok(Vec::new())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_review_opens_runtime_but_does_not_conceal_corrupt_source_index() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let source = format!(
        "Public Class Sample\n    ' {}\n    Public Function Enabled() As Boolean\n        Return True\n    End Function\nEnd Class\n",
        "A generic review fixture with a persisted source index. ".repeat(5)
    );
    std::fs::write(root.join("Sample.vb"), &source).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    }).unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(engram_server::IndexProjectRequest {
        directory: root.to_string_lossy().into_owned(),
        project_name: "ColdReview".into(),
        project_type: engram_server::models::ProjectType::General,
        wait: true,
        dedupe_by_directory: false,
    })).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    let generation = engram_server::services::project_service::get_active_generation(&state, &pid).await.unwrap();
    let diff = "diff --git a/Sample.vb b/Sample.vb\n--- a/Sample.vb\n+++ b/Sample.vb\n@@ -4 +4 @@\n-        Return False\n+        Return True\n";

    for corrupt in [false, true] {
        if corrupt {
            state.get_project_cached(&pid).unwrap().search
                .delete_files(&pid, "memory", &[RelPath::new("Sample.vb")])
                .await.unwrap();
        }
        // Reproduce startup/cache eviction without changing persisted registry,
        // graph or source bytes. No search/freshness warmup occurs before review.
        state.projects.remove(&pid);
        assert!(state.get_project_cached(&pid).is_none());
        let observed = Arc::new(Mutex::new(None));
        let (_, dispatched, _, _) = run_pre_commit_review_with(
            &state, &pid, &root, generation, diff, &ReviewConfig::default(),
            vec![Box::new(ObserveCompleteness(observed.clone()))],
        ).await.unwrap();
        assert_eq!(dispatched, 1);
        let note = observed.lock().unwrap().clone().expect("gate ran");
        if corrupt {
            assert!(note.as_deref().is_some_and(|text| text.contains("INCOMPLETE")), "{note:?}");
        } else {
            assert!(note.is_none(), "cold but complete index should be usable: {note:?}");
        }
        assert!(state.get_project_cached(&pid).is_some());
        assert_eq!(std::fs::read_to_string(root.join("Sample.vb")).unwrap(), source);
        assert_eq!(engram_server::services::project_service::get_active_generation(&state, &pid).await.unwrap(), generation);
    }
}
