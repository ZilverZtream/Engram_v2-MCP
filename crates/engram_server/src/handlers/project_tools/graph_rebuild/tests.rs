use super::*;
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use serde_json::json;

const PID: &str = "graph-rebuild-contract";

enum BeforePublication {
    Cancel,
    Write(PathBuf, String),
}
static BEFORE_PUBLICATION: std::sync::LazyLock<
    std::sync::Mutex<HashMap<usize, BeforePublication>>,
> = std::sync::LazyLock::new(Default::default);
struct Hook(usize);
impl Drop for Hook {
    fn drop(&mut self) {
        BEFORE_PUBLICATION.lock().unwrap().remove(&self.0);
    }
}
fn hook(app: &Engram, action: BeforePublication) -> Hook {
    let key = Arc::as_ptr(&app.state.registry) as usize;
    BEFORE_PUBLICATION.lock().unwrap().insert(key, action);
    Hook(key)
}
pub(super) fn before_publication(
    state: &crate::state::AppState,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let action = BEFORE_PUBLICATION
        .lock()
        .unwrap()
        .remove(&(Arc::as_ptr(&state.registry) as usize));
    match action {
        Some(BeforePublication::Cancel) => cancel.cancel(),
        Some(BeforePublication::Write(path, text)) => std::fs::write(path, text)?,
        None => {}
    }
    Ok(())
}

async fn seed(root: &Path) -> anyhow::Result<Engram> {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    std::fs::write(
        repo.join("Caller.cs"),
        "class Caller { int Run() { return Callee.Next(); } }\n",
    )?;
    std::fs::write(
        repo.join("Callee.cs"),
        "class Callee { public static int Next() { return 1; } }\n",
    )?;
    std::fs::write(repo.join("empty.cs"), "// no symbols\n")?;
    let (state, _) = crate::state::AppState::new(Config {
        data_dir: root.join("data"),
        allowed_roots: vec![repo.clone()],
        embedding_backend: "fts_only".into(),
        max_chunks_per_file: 100,
        ..Default::default()
    })?;
    state.registry.put_project(&ProjectRecord {
        project_id: PID.into(),
        project_name: "Owned graph rebuild".into(),
        project_type: "general".into(),
        directory: repo.to_string_lossy().into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        reindex_required_since_ms: None,
    })?;
    state.registry.set_meta(PID, "active_generation", "1")?;
    let app = Engram::new(state);
    app.handle_repair_project(serde_json::from_value(json!({
        "project_id":PID,"scope":"full","max_commits":0
    }))?)
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    // Produce an actual incremental selection with differing file revisions.
    std::fs::write(
        repo.join("Callee.cs"),
        "class Callee { public static int Next() { return 22; } }\n",
    )?;
    app.update_project_with_scope(PID, 2, 0, false, &[], None, &CancellationToken::new())
        .await?;
    Ok(app)
}

fn durable_graph(app: &Engram) -> anyhow::Result<serde_json::Value> {
    Ok(
        json!({"nodes":app.state.graph.query_nodes(PID,None,None,None,usize::MAX)?,
        "edges":app.state.graph.list_edges(PID,None)?}),
    )
}

fn stored_documents(
    search: &engram_index::HybridSearchEngine,
) -> anyhow::Result<Vec<(String, String, String)>> {
    let mut rows: Vec<_> = search
        .list_docs_in_namespace(PID, "memory")?
        .into_iter()
        .map(|row| (row.namespace, row.doc_id, row.path))
        .collect();
    rows.sort();
    Ok(rows)
}

#[test]
fn graph_rebuild_public_repair_restores_source_without_advancing_revisions_or_replaying_history()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let app = seed(temporary.path()).await?;
        let project = app
            .ensure_project_runtime(PID)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let generation = app.state.registry.active_source_generation(PID)?;
        let selected = project.search.pin_source_generation(PID, generation)?;
        let revisions = selected.selected_file_revisions().unwrap().clone();
        assert_ne!(revisions["Caller.cs"], revisions["Callee.cs"]);
        let before_docs = stored_documents(&project.search)?;
        let files = app
            .state
            .graph
            .query_nodes(PID, Some("file"), None, None, usize::MAX)?;
        let symbols = app
            .state
            .graph
            .query_nodes(PID, None, Some("Next"), None, usize::MAX)?;
        let callee = symbols
            .iter()
            .find(|node| node.file_path.as_str() == "Callee.cs")
            .unwrap();
        let mut chunk = Node {
            node_id: "owned-search-chunk".into(),
            node_type: "chunk".into(),
            name: "preserved".into(),
            namespace: "memory".into(),
            language: "csharp".into(),
            file_path: RelPath::new("Caller.cs"),
            start_line: 1,
            end_line: 1,
            generation: revisions["Caller.cs"],
            metadata: None,
        };
        chunk.metadata = Some(json!({"learned":"keep this"}));
        let learning = Edge {
            source_id: "file:Caller.cs".into(),
            target_id: chunk.node_id.clone(),
            namespace: "memory".into(),
            language: "csharp".into(),
            edge_kind: EdgeKind::Dependency,
            weight: 21,
            generation: 0,
            metadata: Some(json!({"learned":"exact evidence"})),
            updated_at_ms: 123,
        };
        let history = Edge {
            target_id: callee.node_id.clone(),
            edge_kind: EdgeKind::TemporalCoupling,
            ..learning.clone()
        };
        let obsolete = Edge {
            target_id: "::ObsoleteFromOldExtractor".into(),
            edge_kind: EdgeKind::Calls,
            ..learning.clone()
        };
        app.state.graph.upsert_nodes_and_edges(
            PID,
            &[chunk.clone()],
            &[learning.clone(), history.clone(), obsolete],
        )?;
        // Remove current extracted calls, leaving independently indexed source
        // and learning intact. Rebuild must recover them from the authored files.
        let calls = app.state.graph.list_edges(PID, Some(EdgeKind::Calls))?;
        let kept: Vec<_> = calls
            .iter()
            .filter(|edge| edge.target_id == "::ObsoleteFromOldExtractor")
            .cloned()
            .collect();
        app.state.graph.replace_edge_snapshot(PID, &calls, &kept)?;
        for key in [
            "last_git_oid",
            "oldest_indexed_git_oid",
            "pr_ingest_watermark",
        ] {
            app.state
                .registry
                .set_meta(PID, key, "retained-history-cursor")?;
        }
        let jobs = serde_json::to_value(app.state.registry.list_jobs(Some(PID))?)?;
        let reply = app
            .handle_repair_project(serde_json::from_value(
                json!({"project_id":PID,"scope":"graph_only"}),
            )?)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let text = &reply.content[0].as_text().unwrap().text;
        assert!(
            text.contains("embeddings=0") && text.contains("(unchanged)"),
            "{text}"
        );
        assert_eq!(
            app.state.registry.active_source_generation(PID)?,
            generation
        );
        assert_eq!(
            project
                .search
                .pin_source_generation(PID, generation)?
                .selected_file_revisions(),
            Some(&revisions)
        );
        assert_eq!(stored_documents(&project.search)?, before_docs);
        assert_eq!(
            serde_json::to_value(app.state.registry.list_jobs(Some(PID))?)?,
            jobs
        );
        for key in [
            "last_git_oid",
            "oldest_indexed_git_oid",
            "pr_ingest_watermark",
        ] {
            assert_eq!(
                app.state.registry.get_meta(PID, key)?.as_deref(),
                Some("retained-history-cursor")
            );
        }
        for file in files {
            let after = app.state.graph.get_node(PID, &file.node_id)?.unwrap();
            assert_eq!(after.generation, file.generation);
            assert_eq!(
                after.metadata.as_ref().unwrap()["file_hash"],
                file.metadata.as_ref().unwrap()["file_hash"]
            );
        }
        let edges = app.state.graph.list_edges(PID, None)?;
        for retained in [learning, history] {
            assert!(edges.iter().any(|actual| json!(actual) == json!(retained)));
        }
        assert!(
            edges
                .iter()
                .any(|edge| edge.edge_kind == EdgeKind::Calls && edge.target_id == callee.node_id),
            "{edges:?}"
        );
        assert!(
            !edges
                .iter()
                .any(|edge| edge.target_id == "::ObsoleteFromOldExtractor")
        );
        assert_eq!(
            json!(app.state.graph.get_node(PID, &chunk.node_id)?.unwrap()),
            json!(chunk)
        );
        assert!(!std::fs::read_dir(&app.state.cfg.data_dir)?.any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("graph-rebuild-")
        }));
        Ok::<_, anyhow::Error>(())
    });
    drop(runtime);
    temporary.close()?;
    result
}

#[test]
fn graph_rebuild_source_drift_cancellation_and_wipe_refuse_without_graph_changes()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let app = seed(temporary.path()).await?;
        let before = durable_graph(&app)?;
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(app.rebuild_project_graph(PID, &cancel).await.is_err());
        assert_eq!(durable_graph(&app)?, before);
        assert!(
            app.handle_repair_project(serde_json::from_value(json!({
                "project_id":PID,"scope":"graph_only","wipe_and_reindex":true
            }))?)
            .await
            .is_err()
        );
        assert_eq!(durable_graph(&app)?, before);
        std::fs::write(
            temporary.path().join("repo/Caller.cs"),
            "class Changed {}\n",
        )?;
        let error = app
            .rebuild_project_graph(PID, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Source content changed"),
            "{error:#}"
        );
        assert_eq!(durable_graph(&app)?, before);
        Ok::<_, anyhow::Error>(())
    });
    drop(runtime);
    temporary.close()?;
    result
}

#[test]
fn graph_rebuild_late_cancellation_and_disk_changes_preserve_the_published_graph()
-> anyhow::Result<()> {
    for action in ["cancel", "content", "file_set"] {
        let temporary = tempfile::tempdir()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            let app = seed(temporary.path()).await?;
            let before = durable_graph(&app)?;
            let generation = app.state.registry.active_source_generation(PID)?;
            let trigger = match action {
                "cancel" => BeforePublication::Cancel,
                "content" => BeforePublication::Write(
                    temporary.path().join("repo/Caller.cs"),
                    "class ChangedAfterParsing {}\n".into(),
                ),
                _ => BeforePublication::Write(
                    temporary.path().join("repo/Added.cs"),
                    "class AddedAfterParsing {}\n".into(),
                ),
            };
            let _hook = hook(&app, trigger);
            let error = app
                .rebuild_project_graph(PID, &CancellationToken::new())
                .await
                .unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains(match action {
                    "cancel" => "cancelled",
                    "content" => "Source content changed",
                    _ => "Source file set changed",
                }),
                "{action}: {error:#}"
            );
            assert_eq!(durable_graph(&app)?, before);
            assert_eq!(
                app.state.registry.active_source_generation(PID)?,
                generation
            );
            assert!(!std::fs::read_dir(&app.state.cfg.data_dir)?.any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("graph-rebuild-")
            }));
            Ok::<_, anyhow::Error>(())
        });
        drop(runtime);
        temporary.close()?;
        result?;
    }
    Ok(())
}
