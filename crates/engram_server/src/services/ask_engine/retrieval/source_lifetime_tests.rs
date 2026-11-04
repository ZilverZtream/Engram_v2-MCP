//! Real source retrieval across public publication/GC and caller cancellation.
use super::*;
use anyhow::{Context, Result, ensure};
use crate::{state::AppState, tools::Engram};
use engram_core::{Config, ContentHash, RelPath};
use engram_index::{HybridQuery, IndexDoc};
use serde_json::{Value, json};
use std::{collections::{BTreeMap, BTreeSet, HashMap}, path::{Path, PathBuf},
    sync::{Condvar, LazyLock, Mutex}};
use tokio::sync::Notify;

type Key = (usize, String, String);
struct Pause {entered:Notify, resume:Notify, done:Notify, released:Mutex<bool>, gate:Condvar}
static PAUSES: LazyLock<Mutex<HashMap<Key, Arc<Pause>>>> = LazyLock::new(||Mutex::new(HashMap::new()));
fn key(search:&HybridSearchEngine,pid:&str,provider:&str)->Key {
    (search as *const HybridSearchEngine as usize,pid.into(),provider.into())
}
struct PauseOwner {key:Key,pause:Arc<Pause>}
impl PauseOwner {
    fn new(search:&HybridSearchEngine,pid:&str,provider:&str)->Self {
        let key=key(search,pid,provider);
        let pause=Arc::new(Pause {entered:Notify::new(),resume:Notify::new(),done:Notify::new(),
            released:Mutex::new(false),gate:Condvar::new()});
        assert!(PAUSES.lock().unwrap_or_else(|e|e.into_inner()).insert(key.clone(),pause.clone()).is_none());
        Self {key,pause}
    }
    async fn entered(&self)->Result<()> {
        tokio::time::timeout(Duration::from_secs(20),self.pause.entered.notified()).await?;
        Ok(())
    }
    fn release(&self) {
        *self.pause.released.lock().unwrap_or_else(|e|e.into_inner())=true;
        self.pause.gate.notify_all();self.pause.resume.notify_one();
    }
    async fn completed(&self)->Result<()> {
        tokio::time::timeout(Duration::from_secs(20),self.pause.done.notified()).await?;
        Ok(())
    }
}
impl Drop for PauseOwner {
    fn drop(&mut self) {
        PAUSES.lock().unwrap_or_else(|e|e.into_inner()).remove(&self.key);
        self.release();
    }
}
pub(crate) async fn after_search(search:&HybridSearchEngine,pid:&str,provider:&str) {
    let pause=PAUSES.lock().unwrap_or_else(|e|e.into_inner()).get(&key(search,pid,provider)).cloned();
    if let Some(pause)=pause {
        pause.entered.notify_one();pause.resume.notified().await;
    }
}
pub(crate) struct Completed(Arc<Pause>);
impl Drop for Completed {fn drop(&mut self){self.0.done.notify_one();}}
pub(crate) fn blocking_source_worker(engine:usize,pid:&str,provider:&str)->Option<Completed> {
    let pause=PAUSES.lock().unwrap_or_else(|e|e.into_inner())
        .get(&(engine,pid.into(),provider.into())).cloned()?;
    pause.entered.notify_one();
    let mut released=pause.released.lock().unwrap_or_else(|e|e.into_inner());
    while !*released {released=pause.gate.wait(released).unwrap_or_else(|e|e.into_inner());}
    drop(released);Some(Completed(pause))
}
struct OwnedTask<T>(Option<tokio::task::JoinHandle<T>>);
impl<T> Drop for OwnedTask<T> {fn drop(&mut self){if let Some(task)=&self.0 {task.abort();}}}
impl<T> OwnedTask<T> {
    async fn finish(&mut self)->Result<T> {
        let result=tokio::time::timeout(Duration::from_secs(30),self.0.as_mut().context("Missing owned request")?).await??;
        self.0.take();Ok(result)
    }
    async fn cancel(&mut self)->Result<()> {
        self.0.as_ref().context("Missing owned request")?.abort();
        let result=tokio::time::timeout(Duration::from_secs(10),self.0.as_mut().unwrap()).await?;
        ensure!(result.is_err_and(|error|error.is_cancelled()),"Owned request did not cancel");
        self.0.take();Ok(())
    }
}

#[derive(Clone,Debug,Eq,PartialEq,Ord,PartialOrd)]
struct Row {path:String,pk:String,doc:String,first:u32,last:u32,body:String}
// Two complete twelve-line functions per file. The >500-character prefix is
// inside each symbol; the final five short lines contain no tail marker, so
// overlap cannot either exceed the provider's 1200-character content allowance
// or make its first-500-character snippet satisfy our complete-byte oracle.
fn primary(version:&str)->String {
    (0..2).map(|n|format!("pub fn {version}_{n}() -> usize {{\n    // s06askneedle {}\n    // s06tail_{version}_{n}\n    let initial = {n};\n    let adjusted = initial + 1;\n    let restored = adjusted - 1;\n    let selected = restored;\n    let answer = selected;\n    // preserve the selected value\n    let result = answer;\n    result\n}}\n",
        "context ".repeat(60))).collect()
}
fn sibling()->String {
    (0..2).map(|n|format!("function stable_{n}() {{\n    // s06askneedle {}\n    // s06tail_stable_{n}\n    let initial = {n};\n    let adjusted = initial + 1;\n    let restored = adjusted - 1;\n    let selected = restored;\n    let answer = selected;\n    // preserve the selected value\n    let result = answer;\n    return result;\n}}\n",
        "context ".repeat(60))).collect()
}
fn query(pid:&str,generation:u64)->HybridQuery {
    HybridQuery {project_id:pid.into(),namespace:"memory".into(),generation,text:"s06askneedle".into(),
        top_k:128,fts_mode:"loose".into(),include_path_prefixes:None,exclude_path_prefixes:None,
        include_path_suffixes:None,language_filters:None,author_filter:None,date_after:None,date_before:None,use_mmr:false}
}
async fn rows(search:&HybridSearchEngine,pid:&str,generation:u64,version:&str)->Result<Vec<Row>> {
    let view=search.pin_source_generation(pid,generation)?;
    let request=query(pid,generation);
    let hits=search.lexical_search(&request)?;
    ensure!(hits.len()==search.count_docs_in_generation(pid,"memory",generation)?);
    #[cfg(feature="vector")]
    if search.semantic_quality() != engram_index::SemanticQuality::Off {
        let native=search.vector_search(&request,&CancellationToken::new()).await?;
        ensure!(native.iter().map(|h|h.pk.clone()).collect::<BTreeSet<_>>()
            ==hits.iter().map(|h|h.pk.clone()).collect::<BTreeSet<_>>(),"Selected native/lexical groups differ");
    }
    let known=BTreeMap::from([("primary.rs",primary(version)),("sibling.js",sibling())]);
    let mut covered=BTreeMap::<String,BTreeSet<u32>>::new();let mut result=Vec::new();
    for hit in hits {
        let (path,_,body,first,last)=search.recover_selected_source_hit(pid,&view,&hit)?.context("Missing selected source body")?;
        let text=known.get(path.as_str()).context("Unexpected source path")?;
        let lines=text.lines().collect::<Vec<_>>();
        ensure!(first>0 && first<=last && last as usize<=lines.len());
        ensure!(body==lines[first as usize-1..last as usize].join("\n")+"\n",
            "Complete selected body differs from known fixture bytes");
        let body_chars=body.chars().count();
        ensure!(body.len()>500 && body_chars<1200 && body.contains("s06tail_"),
            "Fixture must require complete recovery beyond the 500-character snippet: path={} lines={first}..={last} body_bytes={} body_chars={body_chars} snippet_chars={:?} has_tail={}",
            path.as_str(),body.len(),hit.snippet.as_deref().map(|snippet|snippet.chars().count()),body.contains("s06tail_"));
        ensure!(hit.snippet.as_deref().is_some_and(|snippet|!snippet.contains("s06tail_")),
            "Fallback snippet already contains the full-body tail oracle: path={} lines={first}..={last} snippet_chars={:?}",
            path.as_str(),hit.snippet.as_deref().map(|snippet|snippet.chars().count()));
        covered.entry(path.as_str().into()).or_default().extend(first..=last);
        result.push(Row {path:path.as_str().into(),pk:hit.pk,doc:hit.doc_id,first,last,body});
    }
    ensure!(result.len()==4 && covered.len()==2,
        "Expected four complete chunks across two authored source files: {result:?}");
    for path in ["primary.rs","sibling.js"] {
        ensure!(covered.get(path)==Some(&(1..=24).collect::<BTreeSet<u32>>()),
            "Complete authored source line coverage missing for {path}: {:?}",covered.get(path));
        let groups=result.iter().filter(|row|row.path==path)
            .map(|row|(row.first,row.last)).collect::<BTreeSet<_>>();
        ensure!(groups==BTreeSet::from([(1,12),(8,24)]),
            "Expected two complete twelve-line functions with five-line overlap for {path}: {groups:?}");
    }
    result.sort();Ok(result)
}
async fn fixture(root:&Path,backend:&str)->Result<(Engram,String,PathBuf)> {
    let repo=root.join("repo");std::fs::create_dir_all(&repo)?;
    std::fs::write(repo.join("primary.rs"),primary("old"))?;
    std::fs::write(repo.join("sibling.js"),sibling())?;
    {
        let git=git2::Repository::init(&repo)?;let mut index=git.index()?;
        index.add_path(Path::new("primary.rs"))?;index.add_path(Path::new("sibling.js"))?;
        let tree=git.find_tree(index.write_tree()?)?;
        let who=git2::Signature::now("Owned ask lifetime fixture","fixture@example.invalid")?;
        git.commit(Some("HEAD"),&who,&who,"owned source baseline",&tree,&[])?;
    }
    let cfg=Config {data_dir:root.join("data"),allowed_roots:vec![repo.clone()],
        embedding_backend:backend.into(),llm_backend:"none".into(),max_chunks_per_file:900,
        max_project_files:Some(8),max_project_bytes:Some(64*1024),..Default::default()};
    let app=Engram::new(AppState::new(cfg)?.0);
    let indexed=app.handle_index_project(serde_json::from_value(json!({
        "directory":repo,"project_name":"Owned ask source lifetime","project_type":"general",
        "wait":true,"dedupe_by_directory":true}))?).await?;
    let text=&indexed.content[0].as_text().context("Missing index response")?.text;
    let pid=text.split("project_id: ").nth(1).context("Missing real project ID")?
        .lines().next().unwrap().trim().to_owned();
    Ok((app,pid,repo))
}
async fn update(app:&Engram,pid:&str,repo:&Path,version:&str)->Result<u64> {
    std::fs::write(repo.join("primary.rs"),primary(version))?;
    app.handle_update_project(serde_json::from_value(json!({
        "project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false}))?).await?;
    Ok(app.state.registry.active_source_generation(pid)?)
}
async fn gc(app:&Engram,pid:&str)->Result<()> {
    app.handle_incremental_indexing_gc(serde_json::from_value(json!({"project_id":pid,"compact_vectors":false}))?).await?;
    Ok(())
}
fn assert_evidence(evidence:&[EvidenceItem],expected:&[Row])->Result<()> {
    let source=evidence.iter().filter(|e|e.document_namespace.as_deref()==Some("memory")).collect::<Vec<_>>();
    let matches = |item: &&EvidenceItem, row:&Row| item.document_id.as_deref()==Some(row.doc.as_str())
        && item.path.as_deref()==Some(row.path.as_str()) && item.lines==Some((row.first,row.last))
        && item.content==row.body;
    ensure!(source.iter().all(|item|expected.iter().any(|row|matches(item,row))),
        "An arm returned bytes outside the caller-selected complete source groups");
    for (provider,subset) in [
        ("code",expected.iter().collect::<Vec<_>>()),
        ("modality:script",expected.iter().filter(|row|row.path=="sibling.js").collect::<Vec<_>>()),
    ] {
        let arm=source.iter().copied().filter(|item|item.provider==provider).collect::<Vec<_>>();
        ensure!(arm.len()==subset.len(),"Incomplete selected provider {provider}: {arm:?}");
        for row in subset {
            ensure!(arm.iter().any(|item|matches(item,row)),
                "Original complete selected body missing from {provider}: {row:?}");
        }
    }
    Ok(())
}
fn assert_report(response:&rmcp::model::CallToolResult,generation:u64,expected:&[Row])->Result<()> {
    let report:Value=serde_json::from_str(&response.content[0].as_text().context("Missing ask report")?.text)?;
    ensure!(report["snapshot"]["project_generation"]==generation);
    let source=report["evidence"].as_array().context("Missing source evidence")?.iter()
        .filter(|e|e["document_namespace"]=="memory").collect::<Vec<_>>();
    // Public ask ranks, caps and reserves supporting evidence from multiple
    // providers; it does not promise every raw chunk or one item per document.
    // Its selected items must still contain exact complete selected bytes.
    for item in &source {
        ensure!(expected.iter().any(|row|item["document_id"]==row.doc && item["path"]==row.path
            && item["lines"]==json!([row.first,row.last]) && item["content"]==row.body),
            "Public ask returned source outside the complete selected groups: {item:?}");
    }
    for path in ["primary.rs","sibling.js"] {
        ensure!(source.iter().any(|item|item["path"]==path),
            "Public ask lost selected changed or inherited source {path}: {source:?}");
    }
    Ok(())
}
fn context(app:&Engram,pid:&str,generation:u64,search:Arc<HybridSearchEngine>,
    view:Option<Arc<SourceSnapshotGuard>>)->RetrievalCtx {
    RetrievalCtx {insights_enabled:false,search,graph:app.state.graph.clone(),registry:app.state.registry.clone(),
        project_id:pid.into(),generation,source_view:view}
}
fn ask_request(pid:&str)->crate::models::AskCodebaseRequest {
    serde_json::from_value(json!({"project_id":pid,"question":"How does s06askneedle JavaScript work?",
        "depth":"deep","deadline_ms":60000,"output_format":"json","include_insights":false})).unwrap()
}

#[test]
fn public_ask_keeps_complete_selected_source_through_publication_gc_and_retired_gather_context()->Result<()> {
    let owned=tempfile::tempdir()?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result:Result<(Config,String,PathBuf,u64)>=runtime.block_on(async {
        let backend=if cfg!(feature="vector"){"local"}else{"fts_only"};
        let (app,pid,repo)=fixture(owned.path(),backend).await?;
        let project=app.ensure_project_runtime(&pid).await?;
        let first=app.state.registry.active_source_generation(&pid)?;
        let old=rows(&project.search,&pid,first,"old").await?;
        // These hooks own no source guard: only the real public ask and its
        // source arms protect the old physical groups after actual hit lookup.
        let code=PauseOwner::new(&project.search,&pid,"code");
        let modality=PauseOwner::new(&project.search,&pid,"modality:script");
        let caller=Engram::new(app.state.clone());let request=ask_request(&pid);
        let mut task=OwnedTask(Some(tokio::spawn(async move {caller.handle_ask_codebase(request).await})));
        code.entered().await?;modality.entered().await?;
        let second=update(&app,&pid,&repo,"second").await?;ensure!(second>first);
        gc(&app,&pid).await?;
        for row in &old {ensure!(project.search.get_doc_by_pk(&row.pk)?.is_some(),"Public ask did not protect old physical source");}
        let second_rows=rows(&project.search,&pid,second,"second").await?;
        ensure!(second_rows.iter().filter(|r|r.path=="sibling.js").collect::<Vec<_>>()
            ==old.iter().filter(|r|r.path=="sibling.js").collect::<Vec<_>>());
        drop(code);drop(modality);
        assert_report(&task.finish().await??,first,&old)?;
        gc(&app,&pid).await?;
        for row in old.iter().filter(|r|r.path=="primary.rs") {
            ensure!(project.search.get_doc_by_pk(&row.pk)?.is_none(),"Completed ask retained an old source lease");
        }
        // A caller which really selected source2 before retirement can reuse
        // it; no helper may attempt a new pin of retired source2 inside it.
        let selected=Arc::new(project.search.pin_source_generation(&pid,second)?);
        let third=update(&app,&pid,&repo,"third").await?;ensure!(third>second);gc(&app,&pid).await?;
        ensure!(project.search.pin_source_generation(&pid,second).is_err(),"Fresh retired pin unexpectedly succeeded");
        let history="s06askneedle preserved_history_fact";
        project.search.index_docs(&pid,&[IndexDoc {generation:0,chunk_id:999,doc_id:"owned-history-fact".into(),
            content_hash:ContentHash::compute(history.as_bytes()).0,path:RelPath::new("commit:owned-ask-history"),
            language:"text".into(),content:history.into(),namespace:"history".into(),author:None,timestamp:None,
            start_line:0,end_line:0}],&CancellationToken::new()).await?;
        let ctx=context(&app,&pid,second,project.search.clone(),Some(selected.clone()));
        let mut plan=super::super::planner::plan_query("When did s06askneedle JavaScript last change?");
        plan.entities.clear();
        let (items,_)=gather_evidence(&ctx,&plan,"s06askneedle",Depth::Deep,Duration::from_secs(20),CancellationToken::new()).await;
        assert_evidence(&items,&second_rows)?;
        let wrong=RetrievalCtx {generation:third,..ctx.clone()};
        let (items,reports)=gather_evidence(&wrong,&plan,"s06askneedle",Depth::Deep,Duration::from_secs(20),CancellationToken::new()).await;
        ensure!(!items.iter().any(|item|item.document_namespace.as_deref()==Some("memory")));
        ensure!(reports.iter().any(|report|report.provider=="code" && report.status==ProviderStatus::Failed
            && report.note.as_deref().is_some_and(|note|note.contains("mismatch"))));
        ensure!(items.iter().any(|item|item.document_namespace.as_deref()==Some("history")
            && item.generation==Some(0) && item.content.contains("preserved_history_fact")),
            "Wrong source context must preserve actual requested history: intents={:?} modalities={:?} providers={reports:?} items={items:?}",plan.intents,plan.modalities);
        drop(wrong);drop(ctx);drop(selected);
        gc(&app,&pid).await?;
        for row in second_rows.iter().filter(|r|r.path=="primary.rs") {
            ensure!(project.search.get_doc_by_pk(&row.pk)?.is_none(),"Released caller view remained pinned");
        }
        let current=rows(&project.search,&pid,third,"third").await?;
        assert_report(&app.handle_ask_codebase(ask_request(&pid)).await?,third,&current)?;
        let scope=project.info.tantivy_dir.join("source-snapshots")
            .join(blake3::hash(format!("{pid}\0memory").as_bytes()).to_hex().as_str());
        let cfg=app.state.cfg.as_ref().clone();
        drop(project);drop(app);
        Ok((cfg,pid,scope,third))
    });
    drop(runtime);
    // A public mixed-namespace question must still return actual history when
    // cold source layout is unavailable. All old runtime/DB/source owners are
    // closed first; this fixture removes only its own source metadata files.
    let outcome:Result<()>=(|| {
        let (cfg,pid,scope,generation)=result?;
        for entry in std::fs::read_dir(&scope)? {std::fs::remove_file(entry?.path())?;}
        let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        runtime.block_on(async {
            let app=Engram::new(AppState::new(cfg)?.0);
            let mut request=ask_request(&pid);request.question="When did s06askneedle JavaScript last change?".into();
            let response=app.handle_ask_codebase(request).await?;
            ensure!(response.is_error!=Some(true));
            let report:Value=serde_json::from_str(&response.content[0].as_text().context("Missing degraded public report")?.text)?;
            ensure!(report["snapshot"]["project_generation"]==generation);
            let items=report["evidence"].as_array().context("Missing degraded evidence")?;
            ensure!(!items.iter().any(|item|item["document_namespace"]=="memory"));
            ensure!(items.iter().any(|item|item["document_namespace"]=="history"
                && item["generation"]==0 && item["content"].as_str().is_some_and(|s|s.contains("preserved_history_fact"))),
                "Public ask discarded actual change history when source layout failed: {report}");
            ensure!(report["providers"].as_array().unwrap().iter().any(|provider|
                provider["provider"]=="code" && provider["status"]=="failed"
                && provider["note"].as_str().is_some_and(|note|note.contains("source"))),
                "Public ask concealed its failed source selection");
            ensure!(!items.iter().any(|item|matches!(item["provider"].as_str(),Some("callee"|"callee_set"|"definition"|"concept"|"impact"|"usage"|"companion"))),
                "Failed source selection still inferred source graph/disk evidence");
            Ok(())
        })
    })();
    let cleanup=owned.close();
    match(outcome,cleanup){(Err(e),Err(cleanup))=>Err(e.context(format!("Owned ask cleanup failed: {cleanup}"))),
        (Err(e),_)=>Err(e),(Ok(()),cleanup)=>cleanup.map_err(Into::into)}
}

#[test]
fn cancelled_direct_gather_keeps_its_actual_blocking_source_worker_lease_until_completion()->Result<()> {
    let owned=tempfile::tempdir()?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result:Result<()>=runtime.block_on(async {
        let (app,pid,repo)=fixture(owned.path(),"fts_only").await?;
        let project=app.ensure_project_runtime(&pid).await?;
        let first=app.state.registry.active_source_generation(&pid)?;
        let old=rows(&project.search,&pid,first,"old").await?;
        let code=PauseOwner::new(&project.search,&pid,"code");
        let worker=PauseOwner::new(&project.search,&pid,"concept");
        let ctx=context(&app,&pid,first,project.search.clone(),None);
        let mut plan=super::super::planner::plan_query("How does s06askneedle work?");
        plan.entities.clear();plan.modalities.clear();plan.intents=vec![(Intent::Explain,1.0)];
        let mut task=OwnedTask(Some(tokio::spawn(async move {
            gather_evidence(&ctx,&plan,"s06askneedle",Depth::Deep,Duration::from_secs(60),CancellationToken::new()).await
        })));
        code.entered().await?;worker.entered().await?;
        // Search has really completed, so its backend workers no longer mask
        // the source lease owned by the separately paused blocking graph arm.
        drop(code);task.cancel().await?;
        let second=update(&app,&pid,&repo,"second").await?;ensure!(second>first);gc(&app,&pid).await?;
        for row in old.iter().filter(|r|r.path=="primary.rs") {
            let (_,_,body,_,_)=project.search.get_doc_by_pk(&row.pk)?.context("Cancelled waiter dropped the actual worker's source lease")?;
            ensure!(body==row.body);
        }
        worker.release();worker.completed().await?;drop(worker);
        gc(&app,&pid).await?;
        for row in old.iter().filter(|r|r.path=="primary.rs") {
            ensure!(project.search.get_doc_by_pk(&row.pk)?.is_none(),"Finished blocking worker retained its source lease");
        }
        rows(&project.search,&pid,second,"second").await?;
        Ok(())
    });
    drop(runtime);let cleanup=owned.close();
    match(result,cleanup){(Err(e),Err(cleanup))=>Err(e.context(format!("Owned cancelled-ask cleanup failed: {cleanup}"))),
        (Err(e),_)=>Err(e),(Ok(()),cleanup)=>cleanup.map_err(Into::into)}
}
