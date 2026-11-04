//! Real direct-service source reads across publication, collection and cancellation.
use super::*;
use anyhow::{Result, ensure};
use engram_core::RelPath;
use engram_index::{HybridSearchEngine, IndexDoc};
use std::sync::{LazyLock, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const GATE: &str = "source_lifetime_probe";
const DIFF: &str = "diff --git a/permission_company_upload_geometry.rs b/permission_company_upload_geometry.rs\n--- a/permission_company_upload_geometry.rs\n+++ b/permission_company_upload_geometry.rs\n@@ -1 +1 @@\n-pub fn previous() {}\n+pub fn permission_company_upload_converted_geometry_audit() {}\n";

#[derive(Default)]
struct Pause { entered: Notify, resume: Notify, worker_entered: Notify, worker_finished: Notify }
static PAUSES: LazyLock<Mutex<HashMap<String, Arc<Pause>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) async fn after_selection(pid: &str) {
    let pause = PAUSES.lock().unwrap_or_else(|error| error.into_inner()).get(pid).cloned();
    if let Some(pause) = pause { pause.entered.notify_one(); pause.resume.notified().await; }
}

pub(super) fn after_blocking_gate(pid: &str, gate: &str) {
    if gate != GATE { return; }
    let pause = PAUSES.lock().unwrap_or_else(|error| error.into_inner()).get(pid).cloned();
    if let Some(pause) = pause { pause.worker_finished.notify_one(); }
}

struct Release { pid: String, pause: Arc<Pause>, worker: Option<std::sync::mpsc::SyncSender<()>> }
impl Drop for Release {
    fn drop(&mut self) {
        PAUSES.lock().unwrap_or_else(|error| error.into_inner()).remove(&self.pid);
        self.pause.resume.notify_one();
        if let Some(worker) = self.worker.take() { let _ = worker.try_send(()); }
    }
}

struct Fixture { state: AppState, pid: String, root: std::path::PathBuf, search: Arc<HybridSearchEngine>, old: IndexDoc }

async fn fixture(owned: &Path) -> Result<Fixture> {
    let root = owned.join("repo"); std::fs::create_dir_all(&root)?;
    git2::Repository::init(&root)?;
    let (state, _) = AppState::new(engram_core::Config {
        data_dir: owned.join("data"), allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default()
    })?;
    let pid = format!("review-source-{}", uuid::Uuid::new_v4());
    state.registry.register_source_project(&engram_core::ProjectRecord {
        project_id: pid.clone(), project_name: "Owned review source".into(), project_type: "rust".into(),
        directory: root.to_string_lossy().into(), created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    })?;
    state.registry.set_meta(&pid, "total_commits", "0")?;
    let search = crate::services::project_service::ensure_project_runtime(&state, &pid).await?.search;
    let old = source_doc(1);
    std::fs::write(root.join(old.path.as_str()), &old.content)?;
    state.graph.upsert_nodes(&pid, &[engram_graph::Node {
        node_id: "file:permission_company_upload_geometry.rs".into(), node_type: "file".into(), name: "permission_company_upload_geometry.rs".into(),
        namespace: "memory".into(), language: "rust".into(), file_path: old.path.clone(),
        start_line: 1, end_line: 1, generation: 1, metadata: None,
    }])?;
    let fixture = Fixture { state, pid, root, search, old };
    publish(&fixture, 1).await?;
    Ok(fixture)
}

fn source_doc(generation: u64) -> IndexDoc {
    let content = format!("pub fn retained_review_source_{generation}() {{}}\n");
    IndexDoc { generation, chunk_id: generation, path: RelPath::new("permission_company_upload_geometry.rs"), language: "rust".into(),
        content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(), content,
        namespace: "memory".into(), author: None, timestamp: None, start_line: 1, end_line: 1,
        doc_id: format!("source-{generation}"), }
}

async fn publish(fixture: &Fixture, generation: u64) -> Result<()> {
    let cancel = CancellationToken::new(); let doc = source_doc(generation);
    let base = fixture.state.registry.capture_source_head(&fixture.pid)?;
    fixture.search.prepare_full_source_snapshot(&fixture.pid, generation - 1, generation, &[doc.path.clone()], &cancel).await?;
    fixture.search.index_docs(&fixture.pid, &[doc], &cancel).await?;
    fixture.search.seal_source_snapshot(&fixture.pid, generation)?;
    ensure!(fixture.state.registry.publish_source_head(&base, generation)?);
    Ok(())
}

struct SourceGate {
    expected: IndexDoc,
    worker: Option<(Arc<Pause>, Mutex<std::sync::mpsc::Receiver<()>>)>,
    observed: Arc<Mutex<Option<String>>>,
}
impl Gate for SourceGate {
    fn name(&self) -> &'static str { GATE }
    fn run(&self, ctx: &GateContext<'_>) -> Result<Vec<ReviewFinding>> {
        if let Some((pause, resume)) = &self.worker {
            pause.worker_entered.notify_one();
            resume.lock().unwrap_or_else(|error| error.into_inner()).recv_timeout(std::time::Duration::from_secs(15))?;
        }
        ensure!(ctx.search_index_note.is_none(), "Selected complete source became unknown: {:?}", ctx.search_index_note);
        let search = ctx.state.get_project_cached(ctx.project_id).ok_or_else(|| anyhow::anyhow!("Review runtime missing"))?.search;
        ensure!(search.count_docs_in_generation(ctx.project_id, "memory", ctx.generation)? == 1);
        ensure!(search.paths_in_generation(ctx.project_id, Some("memory"), ctx.generation)? == BTreeSet::from([self.expected.path.as_str().to_string()]));
        let pk = engram_core::build_pk(ctx.project_id, "memory", ctx.generation, &self.expected.doc_id);
        let (path, _, body, _, _) = search.get_doc_by_pk(&pk)?.ok_or_else(|| anyhow::anyhow!("Selected source vanished during review"))?;
        ensure!(path == self.expected.path && body == self.expected.content, "Review read another source revision");
        *self.observed.lock().unwrap_or_else(|error| error.into_inner()) = Some(body.clone());
        Ok(vec![ReviewFinding::new(Severity::Info, GATE, self.expected.path.as_str(), "Selected source evidence", body, "Review this exact source")])
    }
}

fn owned_case(cancelled: bool) -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let mut requests = tokio::task::JoinSet::new();
        let result = async {
            let fixture = fixture(owned.path()).await?;
            let old_pk = engram_core::build_pk(&fixture.pid, "memory", 1, &fixture.old.doc_id);
            let pause = Arc::new(Pause::default());
            PAUSES.lock().unwrap_or_else(|error| error.into_inner()).insert(fixture.pid.clone(), pause.clone());
            let (worker_tx, worker_rx) = std::sync::mpsc::sync_channel(1);
            let release = Release { pid: fixture.pid.clone(), pause: pause.clone(), worker: Some(worker_tx) };
            let observed = Arc::new(Mutex::new(None));
            let gate = SourceGate { expected: fixture.old.clone(), worker: cancelled.then_some((pause.clone(), Mutex::new(worker_rx))), observed: observed.clone() };
            let state = fixture.state.clone(); let pid = fixture.pid.clone(); let root = fixture.root.clone();
            let task = requests.spawn(async move {
                run_pre_commit_review_with(&state, &pid, &root, 1, DIFF, &ReviewConfig::default(), vec![Box::new(gate)]).await
            });
            tokio::time::timeout(std::time::Duration::from_secs(15), pause.entered.notified()).await?;
            // The service owns the only source guard; the fixture has none.
            publish(&fixture, 2).await?;
            fixture.search.purge_old_generations(&fixture.pid, 2).await?;
            ensure!(fixture.search.get_doc_by_pk(&old_pk)?.is_some(), "Direct review lost selected source before completeness");
            pause.resume.notify_one();
            if cancelled {
                tokio::time::timeout(std::time::Duration::from_secs(15), pause.worker_entered.notified()).await?;
                task.abort();
                ensure!(requests.join_next().await.is_some_and(|result| result.is_err_and(|error| error.is_cancelled())));
                fixture.search.purge_old_generations(&fixture.pid, 2).await?;
                ensure!(fixture.search.get_doc_by_pk(&old_pk)?.is_some(), "Cancelled review dropped its still-running gate's source");
                release.worker.as_ref().unwrap().try_send(())?;
                // Completion is signalled after the worker's shared owner drops.
                tokio::time::timeout(std::time::Duration::from_secs(15), pause.worker_finished.notified()).await?;
                ensure!(observed.lock().unwrap_or_else(|error| error.into_inner()).as_deref() == Some(fixture.old.content.as_str()),
                    "Detached gate did not recover its selected bytes after cancellation");
            } else {
                let (findings, dispatched, files, outcomes) = tokio::time::timeout(std::time::Duration::from_secs(15), requests.join_next()).await?
                    .ok_or_else(|| anyhow::anyhow!("Review task missing"))???;
                ensure!(dispatched == 1 && files == 1 && findings.len() == 1 && findings[0].detail == fixture.old.content);
                ensure!(matches!(outcomes[0].status, GateStatus::Findings(1)));
            }
            drop(release);
            fixture.search.purge_old_generations(&fixture.pid, 2).await?;
            ensure!(fixture.search.get_doc_by_pk(&old_pk)?.is_none(), "Finished review retained obsolete source");
            Ok::<_, anyhow::Error>(())
        }.await;
        requests.abort_all(); while requests.join_next().await.is_some() {}
        result
    });
    drop(runtime);
    match (result, owned.close()) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn direct_pre_commit_review_keeps_selected_source_through_completeness_and_collection() -> Result<()> { owned_case(false) }

#[test]
fn cancelled_direct_pre_commit_review_keeps_source_until_blocking_gate_finishes() -> Result<()> { owned_case(true) }

#[test]
fn pre_commit_review_accepts_held_retired_caller_view_and_refuses_wrong_guard() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let fixture = fixture(owned.path()).await?;
        let view = Arc::new(fixture.search.pin_source_generation(&fixture.pid, 1)?);
        let config = ReviewConfig { skip_gates: all_gates().iter().filter(|gate| gate.name() != "product_intent")
            .map(|gate| gate.name().to_string()).collect(), ..Default::default() };
        // The ordinary entrypoint must reject a capability for another selected
        // generation before running any gates; do not test a validator in isolation.
        let wrong = run_pre_commit_review_with_source_view(&fixture.state, &fixture.pid, &fixture.root,
            2, DIFF, &config, view.clone()).await;
        ensure!(wrong.is_err_and(|error| error.to_string().contains("does not match")));
        let mut note = source_doc(0); note.namespace = "memory_bank".into(); note.path = RelPath::new("memory_bank:review-policy");
        note.doc_id = "review-policy".into(); note.content = "Recorded permission company upload geometry requirements.".into();
        note.content_hash = blake3::hash(note.content.as_bytes()).to_hex().to_string();
        fixture.search.index_docs(&fixture.pid, &[note], &CancellationToken::new()).await?;
        publish(&fixture, 2).await?;
        fixture.search.purge_old_generations(&fixture.pid, 2).await?;
        ensure!(fixture.search.pin_source_generation(&fixture.pid, 1).is_err(), "Fresh retired pin was unexpectedly admitted");
        // Moves the last held guard into the same entrypoint used by the MCP
        // handler. Its real completeness check and knowledge gate must run.
        let (findings, dispatched, _, outcomes) = run_pre_commit_review_with_source_view(&fixture.state,
            &fixture.pid, &fixture.root, 1, DIFF, &config, view).await?;
        ensure!(dispatched == 1 && findings.iter().any(|finding| finding.gate == "product_intent"));
        ensure!(outcomes.iter().any(|outcome| outcome.name == "product_intent" && matches!(outcome.status, GateStatus::Findings(_))),
            "A held retired source made the real gate degrade: {outcomes:?}");
        fixture.search.purge_old_generations(&fixture.pid, 2).await?;
        ensure!(fixture.search.get_doc_by_pk(&engram_core::build_pk(&fixture.pid, "memory", 1, &fixture.old.doc_id))?.is_none());
        Ok::<_, anyhow::Error>(())
    });
    drop(runtime);
    match (result, owned.close()) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn review_withholds_future_and_unverified_conventions_but_keeps_inherited_source_and_knowledge() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let root = owned.path().join("repo"); std::fs::create_dir_all(root.join("inherited"))?;
        std::fs::create_dir_all(root.join("deleted"))?;
        git2::Repository::init(&root)?;
        let (state, _) = AppState::new(engram_core::Config {
            data_dir: owned.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default()
        })?;
        let pid = format!("review-conventions-{}", uuid::Uuid::new_v4());
        state.registry.register_source_project(&engram_core::ProjectRecord {
            project_id: pid.clone(), project_name: "Owned review conventions".into(), project_type: "rust".into(),
            directory: root.to_string_lossy().into(), created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
        })?;
        state.registry.set_meta(&pid, "total_commits", "0")?;
        let search = crate::services::project_service::ensure_project_runtime(&state, &pid).await?.search;
        let paths = ["source.rs", "inherited/known_one.rs", "inherited/known_two.rs", "inherited/known_three.rs",
            "deleted/deleted_one.rs", "deleted/deleted_two.rs", "deleted/deleted_three.rs", "Audit.rs"];
        let docs = paths.iter().enumerate().map(|(index, path)| {
            let mut doc = source_doc(1); doc.path = RelPath::new(path); doc.doc_id = format!("known-{index}");
            doc.chunk_id = index as u64; doc.content = format!("pub fn known_{index}() {{}}\n");
            if *path == "Audit.rs" { doc.content = "pub mod AuditLog { pub fn Create() {} }\n".into(); }
            doc.content_hash = blake3::hash(doc.content.as_bytes()).to_hex().to_string(); doc
        }).collect::<Vec<_>>();
        for doc in &docs { std::fs::write(root.join(doc.path.as_str()), &doc.content)?; }
        let node = |id: &str, kind: &str, name: &str, path: &str, namespace: &str, generation| engram_graph::Node {
            node_id: id.into(), node_type: kind.into(), name: name.into(), namespace: namespace.into(), language: "rust".into(),
            file_path: RelPath::new(path), start_line: 1, end_line: 1, generation, metadata: None,
        };
        let mut nodes = paths.iter().enumerate().map(|(index, path)| node(
            &format!("known-file-{index}"), "file", path, path, "memory", 1,
        )).collect::<Vec<_>>();
        nodes.push(node("stale-rewritten-audit", "function", "AuditLog.Create", "Audit.rs", "memory", 1));
        state.graph.upsert_nodes(&pid, &nodes)?;
        let cancel = CancellationToken::new();
        let initial = state.registry.capture_source_head(&pid)?;
        let source_paths = docs.iter().map(|doc| doc.path.clone()).collect::<Vec<_>>();
        search.prepare_full_source_snapshot(&pid, 0, 1, &source_paths, &cancel).await?;
        search.index_docs(&pid, &docs, &cancel).await?;
        search.seal_source_snapshot(&pid, 1)?;
        ensure!(state.registry.publish_source_head(&initial, 1)?);
        // Source 2 inherits the legitimate siblings, deletes a whole sibling
        // family and replaces the audit helper. Simulate retained cleanup debt:
        // the old graph rows and old physical chunks are deliberately kept.
        let base = state.registry.capture_source_head(&pid)?;
        let deleted = paths[4..7].iter().map(|path| RelPath::new(path)).collect::<Vec<_>>();
        search.prepare_source_snapshot(&pid, 1, 2, &[RelPath::new("Audit.rs")], &deleted, &cancel).await?;
        let mut replacement = source_doc(2); replacement.path = RelPath::new("Audit.rs");
        replacement.content = "pub fn ordinary_save_without_an_audit_api() {}\n".into();
        replacement.content_hash = blake3::hash(replacement.content.as_bytes()).to_hex().to_string();
        search.index_docs(&pid, &[replacement], &cancel).await?;
        search.seal_source_snapshot(&pid, 2)?;
        ensure!(state.registry.publish_source_head(&base, 2)?);
        for path in &deleted { std::fs::remove_file(root.join(path.as_str()))?; }
        std::fs::write(root.join("Audit.rs"), "pub fn ordinary_save_without_an_audit_api() {}\n")?;
        ensure!(search.get_doc_by_pk(&engram_core::build_pk(&pid, "memory", 1, "known-4"))?.is_some()
            && search.get_doc_by_pk(&engram_core::build_pk(&pid, "memory", 1, "known-7"))?.is_some(),
            "This proof requires retained old chunks so stored presence cannot substitute for selected membership");

        nodes.clear();
        for (namespace, generation, parent, prefix) in [
            ("memory", 3, "future", "pending"),
            ("memory", 0, "future", "unverified"),
            ("memory_bank", 1, "foreign", "foreign"),
        ] {
            for index in 0..3 {
                let path = format!("{parent}/{prefix}_{index}.vb");
                nodes.push(node(&format!("{prefix}-file-{index}"), "file", &path, &path, namespace, generation));
            }
            nodes.push(node(&format!("{prefix}-audit"), "function", &format!("{prefix}.AuditLog.Create"),
                &format!("{parent}/Audit.rs"), namespace, generation));
        }
        state.graph.upsert_nodes(&pid, &nodes)?;
        let mut knowledge = source_doc(0); knowledge.namespace = "memory_bank".into();
        knowledge.path = RelPath::new("memory_bank:review-policy"); knowledge.doc_id = "review-policy".into();
        knowledge.content = "Recorded permission company upload geometry requirements.".into();
        knowledge.content_hash = blake3::hash(knowledge.content.as_bytes()).to_hex().to_string();
        search.index_docs(&pid, &[knowledge], &cancel).await?;
        let mut diff = String::new();
        for parent in ["future", "foreign", "deleted", "inherited"] {
            let path = format!("{parent}/permission_company_upload_geometry.cs");
            let body = "public class Upload {\n    public void Save() { db.SubmitChanges(); }\n}\n";
            std::fs::create_dir_all(root.join(parent))?;
            std::fs::write(root.join(&path), body)?;
            diff.push_str(&format!("diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,3 @@\n"));
            for line in body.lines() { diff.push('+'); diff.push_str(line); diff.push('\n'); }
        }
        let (findings, dispatched, files, outcomes) = run_pre_commit_review_with(
            &state, &pid, &root, 2, &diff, &ReviewConfig::default(), vec![
                Box::new(gates::AuditGate), Box::new(gates::NewFileGate), Box::new(gates::ProductIntentGate),
                Box::new(gates::SyncContractGate), Box::new(gates::CoAddedFamilyGate),
            ],
        ).await?;
        ensure!(dispatched == 5 && files == 4);
        ensure!(!findings.iter().any(|finding| finding.gate == "audit"),
            "A replaced, future, unverified or knowledge node invented an audit API: {findings:?}");
        let conventions = findings.iter().filter(|finding| finding.gate == "new_file").collect::<Vec<_>>();
        ensure!(!conventions.is_empty() && conventions.iter().all(|finding| finding.file_path.starts_with("inherited/")),
            "Deleted/unpublished/foreign siblings invented a file convention: {conventions:?}");
        ensure!(conventions.iter().any(|finding| finding.title.contains("extension"))
            && conventions.iter().any(|finding| finding.title.contains("naming")),
            "Legitimate inherited older-source conventions were lost: {conventions:?}");
        ensure!(findings.iter().any(|finding| finding.gate == "product_intent" && finding.file_path == "review-policy"),
            "Source convention filtering discarded append-only knowledge evidence: {findings:?}");
        for (gate, label) in [("audit", "audit convention"), ("new_file", "file convention"),
            ("sync_contract", "file convention"), ("co_added_family", "file convention")] {
            let outcome = outcomes.iter().find(|outcome| outcome.name == gate).ok_or_else(|| anyhow::anyhow!("Missing {gate} outcome"))?;
            ensure!(matches!(&outcome.status, GateStatus::Degraded { notes, .. }
                if notes.iter().any(|note| note.contains(&format!("{label} coverage unknown"))
                    && note.contains("newer") && note.contains("generation-0"))),
                "Withheld live graph was reported as complete by {gate}: {outcome:?}");
        }
        ensure!(outcomes.iter().any(|outcome| outcome.name == "audit" && matches!(&outcome.status,
            GateStatus::Degraded { notes, .. } if notes.iter().any(|note| note.contains("unselected file revisions")))),
            "A replaced file's retained audit graph row needs explicit unknown coverage");
        ensure!(state.registry.capture_source_head(&pid)?.generation() == 2);
        ensure!(search.get_doc_by_pk(&engram_core::build_pk(&pid, "memory", 1, "known-2"))?
            .is_some_and(|(path, _, body, _, _)| path == docs[2].path && body == docs[2].content));
        Ok::<_, anyhow::Error>(())
    });
    drop(runtime);
    match (result, owned.close()) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
