//! Public MCP publication/GC in real peer processes, including Registry-open
//! admission while only an old source view retains the catalog's OS lock.
use super::*;
use anyhow::{ensure, Context, Result};
use engram_index::{HybridHit, HybridSearchEngine, SourceSnapshotGuard};
use rmcp::{ServiceExt, model::{CallToolRequestParam, CallToolResult}};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, BTreeSet}, fs, io::{Read, Write},
    path::{Path, PathBuf}, process::{Child, Command, Stdio}, time::{Duration, Instant}};

const CHILD: &str = "s17_server_mcp_child_owned_only";
const HEAD_KEYS: &[&str] = &["active_generation","source_head_revision_v1","source_head_required_v1",
    "source_identity_epoch_v1","source_identity_verified_v1","source_legacy_absence_attested_v1",
    "last_full_index_generation"];
type Head = BTreeMap<String,Option<String>>;

#[derive(Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Serialize,Deserialize)]
struct Row { path:String,pk:String,first:u32,last:u32,text:String }

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    nonce:String,root:PathBuf,cfg:Config,pid:String,first:u64,middle:u64,
    authority:Head,old_rows:Vec<Row>,middle_rows:Vec<Row>,native_rows:usize,
}
#[derive(Clone,Debug,Serialize,Deserialize)]
struct Rpc { ok:bool,text:String }
#[derive(Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    nonce:String,phase:String,root:PathBuf,registry:PathBuf,tantivy:PathBuf,
    registry_open:bool,registry_locked:bool,before:Option<Head>,after:Option<Head>,
    update:Option<Rpc>,gc:Option<Rpc>,rows:Vec<Row>,
}

fn source(version:&str,lines:usize)->String {
    (0..lines).map(|n|format!(
        "pub fn {version}_{n:03}() -> usize {{ {n} }} // s17marker complete owned source line {version}_{n:03}\n"
    )).collect()
}
fn configuration(root:&Path)->Config {
    Config {data_dir:root.join("data"),allowed_roots:vec![root.join("repo")],
        embedding_backend:if cfg!(feature="vector") {"local"} else {"fts_only"}.into(),
        llm_backend:"none".into(),multi_client:false,max_chunks_per_file:80,
        max_project_files:Some(8),max_project_bytes:Some(64*1024),..Default::default()}
}
fn head(registry:&engram_core::Registry,pid:&str)->Result<Head> {
    HEAD_KEYS.iter().map(|key|Ok(((*key).into(),registry.get_meta_bounded(pid,key,4096)?))).collect()
}
fn text(result:&CallToolResult)->String {
    result.content.iter().filter_map(|item|item.as_text().map(|t|t.text.as_str())).collect::<Vec<_>>().join("\n")
}
fn rpc(result:Result<CallToolResult,impl std::fmt::Display>)->Rpc {
    match result {
        Ok(result)=>Rpc {ok:result.is_error!=Some(true),text:text(&result)},
        Err(error)=>Rpc {ok:false,text:error.to_string()},
    }
}
fn query(pid:&str,generation:u64)->HybridQuery {
    HybridQuery {project_id:pid.into(),namespace:"memory".into(),generation,text:"s17marker".into(),
        top_k:128,fts_mode:"loose".into(),include_path_prefixes:None,exclude_path_prefixes:None,
        include_path_suffixes:None,language_filters:None,author_filter:None,date_after:None,date_before:None,use_mmr:false}
}

async fn rows(search:&HybridSearchEngine,pid:&str,generation:u64,primary:&str)->Result<(Vec<Row>,Vec<HybridHit>)> {
    let request=query(pid,generation);
    let hits=search.lexical_search(&request)?;
    ensure!(hits.len()==search.count_docs_in_generation(pid,"memory",generation)?,
        "Common marker must enumerate every selected source chunk");
    let keys=hits.iter().map(|h|h.pk.clone()).collect::<BTreeSet<_>>();
    ensure!(keys.len()==hits.len(),"Duplicate selected physical identity");
    #[cfg(feature="vector")]
    {
        let native=search.vector_search(&request,&CancellationToken::new()).await?;
        ensure!(native.len()==hits.len() && native.iter().map(|h|h.pk.clone()).collect::<BTreeSet<_>>()==keys,
            "Complete native/lexical source physical identities differ");
    }
    let sibling=source("sibling",2);
    let expected=BTreeMap::from([("source.rs",primary),("sibling.rs",sibling.as_str())]);
    let view=search.pin_source_generation(pid,generation)?;
    let mut result=Vec::new(); let mut covered=BTreeMap::<String,BTreeSet<u32>>::new();
    for hit in &hits {
        let (path,_,body,first,last)=search.recover_selected_source_hit(pid,&view,hit)?
            .context("Selected physical body missing")?;
        let known=expected.get(path.as_str()).context("Unexpected selected file")?;
        let lines=known.lines().collect::<Vec<_>>();
        ensure!(first>0 && first<=last && last as usize<=lines.len());
        ensure!(body==lines[first as usize-1..last as usize].join("\n")+"\n",
            "Selected bytes differ from complete fixture source lines");
        covered.entry(path.as_str().into()).or_default().extend(first..=last);
        result.push(Row {path:path.as_str().into(),pk:hit.pk.clone(),first,last,text:body});
    }
    ensure!(covered.len()==2);
    for (path,body) in expected {
        ensure!(covered.get(path)==Some(&(1..=body.lines().count() as u32).collect()),
            "Incomplete source file group {path}");
    }
    ensure!(result.iter().filter(|r|r.path=="source.rs").count()>=2,
        "The source fixture must be genuinely multi-chunk");
    result.sort();Ok((result,hits))
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(),Ok(Some(_))) {let _=self.0.kill();}
        let _=self.0.wait();
    }
}
fn bounded_log(path:&Path)->Result<String> {
    let mut prefix=Vec::with_capacity(16*1024);
    fs::File::open(path)?.take(16*1024).read_to_end(&mut prefix)?;
    Ok(String::from_utf8_lossy(&prefix).into_owned())
}
fn child(root:&Path,m:&Manifest,phase:&str)->Result<Report> {
    ensure!(["registry","catalog","released"].contains(&phase));
    let report_path=root.join(format!("{phase}.json"));
    let log_path=root.join(format!("{phase}.log"));
    let log=fs::OpenOptions::new().write(true).create_new(true).open(&log_path)?;
    let module=module_path!().split_once("::").context("No parent test module")?.1;
    let mut command=Command::new(std::env::current_exe()?);
    command.args(["--exact",&format!("{module}::{CHILD}"),"--ignored","--nocapture","--test-threads=1"])
        .env("ENGRAM_S17_SERVER_ROOT",root).env("ENGRAM_S17_SERVER_NONCE",&m.nonce)
        .env("ENGRAM_S17_SERVER_PHASE",phase).env("ENGRAM_S17_SERVER_REPORT",&report_path)
        .env("TEMP",root.join("scratch")).env("TMP",root.join("scratch")).env("RUST_LOG","warn")
        .stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
    #[cfg(windows)]
    {use std::os::windows::process::CommandExt;command.creation_flags(0x08000000|0x4000);}
    let mut owned=OwnedChild(command.spawn()?);
    drop(command);
    let deadline=Instant::now()+Duration::from_secs(120);
    loop {
        ensure!(fs::metadata(&log_path)?.len()<=2*1024*1024,
            "Owned server child exceeded its 2 MiB log budget: {}",log_path.display());
        if let Some(status)=owned.0.try_wait()? {
            ensure!(status.success(),"Owned server child failed ({phase}): {}",
                bounded_log(&log_path)?);
            break;
        }
        ensure!(Instant::now()<deadline,"Owned server child timed out: {}",log_path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
    ensure!(fs::metadata(&report_path)?.len()<=128*1024,"Oversized child report");
    let report:Report=serde_json::from_slice(&fs::read(&report_path)?)?;
    ensure!(report.nonce==m.nonce && report.phase==phase && report.root==root.canonicalize()?);
    ensure!(report.registry==m.cfg.data_dir.join("registry/registry.redb").canonicalize()?);
    ensure!(report.tantivy==m.cfg.data_dir.join("projects").join(&m.pid).join("tantivy").canonicalize()?);
    Ok(report)
}

async fn child_calls(m:&Manifest,phase:&str)->Result<Report> {
    let registry_path=m.cfg.data_dir.join("registry/registry.redb").canonicalize()?;
    let tantivy=m.cfg.data_dir.join("projects").join(&m.pid).join("tantivy").canonicalize()?;
    let mut report=Report {nonce:m.nonce.clone(),phase:phase.into(),root:m.root.clone(),
        registry:registry_path,tantivy,registry_open:false,registry_locked:false,
        before:None,after:None,update:None,gc:None,rows:Vec::new()};
    let (state,_)=match AppState::new(m.cfg.clone()) {
        Ok(value)=>value,
        Err(error)=>{
            report.registry_locked=error.chain().any(|cause|matches!(
                cause.downcast_ref::<redb::DatabaseError>(),Some(redb::DatabaseError::DatabaseAlreadyOpen)));
            ensure!(report.registry_locked,"Unexpected AppState open failure: {error:#}");
            return Ok(report);
        }
    };
    report.registry_open=true;
    report.before=Some(head(&state.registry,&m.pid)?);
    let lifecycle=Engram::new(state.clone());
    let app=Engram::new(state.clone());
    let (client_io,server_io)=tokio::io::duplex(1024*1024);
    let mut startup=tokio::spawn(async move {app.serve(server_io).await});
    let initialized=tokio::time::timeout(Duration::from_secs(15),().serve(client_io)).await;
    let client=match initialized {
        Ok(Ok(client))=>client,
        Ok(Err(error))=>{
            startup.abort();let _=tokio::time::timeout(Duration::from_secs(5),&mut startup).await;
            return Err(error.into());
        }
        Err(error)=>{
            startup.abort();let _=tokio::time::timeout(Duration::from_secs(5),&mut startup).await;
            return Err(error.into());
        }
    };
    let started=tokio::time::timeout(Duration::from_secs(15),&mut startup).await;
    let server=match started {
        Ok(result)=>result??,
        Err(error)=>{
            startup.abort();let _=tokio::time::timeout(Duration::from_secs(5),&mut startup).await;
            let _=tokio::time::timeout(Duration::from_secs(10),client.cancel()).await;
            return Err(error.into());
        }
    };
    let scenario:Result<()> = async {
        let tools=tokio::time::timeout(Duration::from_secs(10),client.list_all_tools()).await??;
        for name in ["update_project","incremental_indexing_gc"] {
            ensure!(tools.iter().any(|tool|tool.name==name),"Required public MCP tool missing: {name}");
        }
        report.update=Some(rpc(tokio::time::timeout(Duration::from_secs(30),
            client.call_tool(CallToolRequestParam {name:"update_project".into(),arguments:Some(json!({
                "project_id":m.pid,"wait":true,"max_commits":1,"index_antipatterns":false
            }).as_object().unwrap().clone())})).await?));
        // Use the identical public GC request in both catalog-refused and
        // released phases; no numeric Registry head injection is involved.
        report.gc=Some(rpc(tokio::time::timeout(Duration::from_secs(30),
            client.call_tool(CallToolRequestParam {name:"incremental_indexing_gc".into(),arguments:Some(json!({
                "project_id":m.pid,"compact_vectors":false
            }).as_object().unwrap().clone())})).await?));
        report.after=Some(head(&state.registry,&m.pid)?);
        if report.update.as_ref().unwrap().ok && report.gc.as_ref().unwrap().ok {
            let generation=state.registry.active_source_generation(&m.pid)?;
            let project=lifecycle.ensure_project_runtime(&m.pid).await
                .map_err(|e|anyhow::anyhow!("{e}"))?;
            report.rows=rows(&project.search,&m.pid,generation,&source("new",6)).await?.0;
        }
        Ok(())
    }.await;
    // Close both protocol services on success AND ordinary error, then drain all
    // owned jobs. Runtime teardown below joins native/blocking work before exit.
    let client_closed=tokio::time::timeout(Duration::from_secs(10),client.cancel()).await;
    let server_closed=tokio::time::timeout(Duration::from_secs(10),server.cancel()).await;
    let jobs_closed=tokio::time::timeout(Duration::from_secs(20),finish_owned_jobs(&lifecycle)).await;
    client_closed??;server_closed??;jobs_closed?;
    scenario?;
    Ok(report)
}

#[test]
#[ignore="S17 same-executable child only; requires parent-owned manifest and nonce"]
fn s17_server_mcp_child_owned_only()->Result<()> {
    let location=PathBuf::from(std::env::var_os("ENGRAM_S17_SERVER_ROOT").context("Parent-owned root required")?);
    let root=location.canonicalize()?;
    let manifest_path=root.join("s17-manifest.json");
    ensure!(fs::metadata(&manifest_path)?.len()<=128*1024);
    let m:Manifest=serde_json::from_slice(&fs::read(manifest_path)?)?;
    let phase=std::env::var("ENGRAM_S17_SERVER_PHASE")?;
    ensure!(["registry","catalog","released"].contains(&phase.as_str()));
    ensure!(!m.nonce.is_empty() && m.nonce.len()<=128
        && std::env::var("ENGRAM_S17_SERVER_NONCE")?==m.nonce);
    ensure!(m.root==root && m.cfg.data_dir.canonicalize()?==root.join("data").canonicalize()?);
    ensure!(m.cfg.allowed_roots.len()==1
        && m.cfg.allowed_roots[0].canonicalize()?==root.join("repo").canonicalize()?);
    ensure!(m.cfg.embedding_backend==if cfg!(feature="vector") {"local"} else {"fts_only"});
    ensure!(m.cfg.llm_backend=="none" && !m.cfg.multi_client);
    let destination=PathBuf::from(std::env::var_os("ENGRAM_S17_SERVER_REPORT").context("Report path required")?);
    ensure!(destination==location.join(format!("{phase}.json"))
        && destination.parent().context("Missing report parent")?.canonicalize()?==root,
        "Report outside exact owned path");
    ensure!(fs::read_to_string(root.join(".s17-owned"))?==m.nonce);
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result=runtime.block_on(child_calls(&m,&phase));
    drop(runtime);
    let report=result?;
    let bytes=serde_json::to_vec(&report)?;
    ensure!(bytes.len()<=128*1024);
    let mut file=fs::OpenOptions::new().write(true).create_new(true).open(destination)?;
    file.write_all(&bytes)?;file.sync_all()?;
    Ok(())
}

fn source_tree(root:&Path,pid:&str)->Result<BTreeMap<String,String>> {
    let base=root.join("data/projects").join(pid);
    let mut directories=vec![base.clone()];let mut bytes=0u64;let mut result=BTreeMap::new();
    while let Some(directory)=directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry=entry?;let meta=fs::symlink_metadata(entry.path())?;
            ensure!(!meta.file_type().is_symlink(),"Unexpected source-store symlink");
            if meta.is_dir() {directories.push(entry.path());continue;}
            if matches!(entry.file_name().to_str(),
                Some(".source-snapshot-catalog.lock"|".engram-vector-owner.lock")) {
                ensure!(meta.len()==0,"Expected a content-free OS admission lock");
                continue;
            }
            ensure!(meta.is_file() && result.len()<2048);
            bytes=bytes.checked_add(meta.len()).context("Source inventory overflow")?;
            ensure!(bytes<=32*1024*1024,"Owned source inventory exceeds its read budget");
            let path=entry.path();
            let relative=path.strip_prefix(&base)?.to_str().context("Non-UTF8 owned path")?.replace('\\',"/");
            result.insert(relative,blake3::hash(&fs::read(path)?).to_hex().to_string());
        }
    }
    Ok(result)
}

async fn setup(root:&Path)->Result<(Manifest,SourceSnapshotGuard,Vec<HybridHit>)> {
    fs::create_dir_all(root.join("repo"))?;fs::create_dir_all(root.join("scratch"))?;
    fs::write(root.join("repo/source.rs"),source("old",6))?;
    fs::write(root.join("repo/sibling.rs"),source("sibling",2))?;
    {
        let git=git2::Repository::init(root.join("repo"))?;
        let mut index=git.index()?;index.add_path(Path::new("source.rs"))?;index.add_path(Path::new("sibling.rs"))?;
        let tree=git.find_tree(index.write_tree()?)?;
        let signature=git2::Signature::now("Owned S17 fixture","fixture@example.invalid")?;
        git.commit(Some("HEAD"),&signature,&signature,"owned source baseline",&tree,&[])?;
    }
    let cfg=configuration(root);
    let app=Engram::new(AppState::new(cfg.clone())?.0);
    let indexed=app.handle_index_project(serde_json::from_value(json!({
        "directory":root.join("repo"),"project_name":"S17 server process source owner",
        "project_type":"rust","wait":true,"dedupe_by_directory":true
    }))?).await.map_err(|e|anyhow::anyhow!("{e}"))?;
    let indexed=text(&indexed);
    let pid=indexed.split("project_id: ").nth(1).context("Missing real project ID")?
        .lines().next().unwrap().trim().to_owned();
    let first=app.state.registry.active_source_generation(&pid)?;
    let project=app.ensure_project_runtime(&pid).await.map_err(|e|anyhow::anyhow!("{e}"))?;
    let held=project.search.pin_source_generation(&pid,first)?;
    let (old_rows,old_hits)=rows(&project.search,&pid,first,&source("old",6)).await?;
    fs::write(root.join("repo/source.rs"),source("middle",6))?;
    app.handle_update_project(serde_json::from_value(json!({
        "project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false
    }))?).await.map_err(|e|anyhow::anyhow!("{e}"))?;
    let middle=app.state.registry.active_source_generation(&pid)?;
    ensure!(middle>first);
    let (middle_rows,_)=rows(&project.search,&pid,middle,&source("middle",6)).await?;
    let original_sibling=old_rows.iter().filter(|r|r.path=="sibling.rs").collect::<Vec<_>>();
    ensure!(middle_rows.iter().filter(|r|r.path=="sibling.rs").collect::<Vec<_>>()==original_sibling,
        "Setup update did not inherit the exact sibling physical group");
    let native_rows=project.search.count_vectors(&pid).await?;
    let authority=head(&app.state.registry,&pid)?;
    let m=Manifest {nonce:uuid::Uuid::new_v4().to_string(),root:root.canonicalize()?,cfg,pid,
        first,middle,authority,old_rows,middle_rows,native_rows};
    fs::write(root.join(".s17-owned"),&m.nonce)?;
    let mut manifest=fs::OpenOptions::new().write(true).create_new(true).open(root.join("s17-manifest.json"))?;
    manifest.write_all(&serde_json::to_vec(&m)?)?;manifest.sync_all()?;drop(manifest);
    // This is a real edit: an accidentally admitted peer must attempt source
    // publication rather than passing as an unchanged no-op.
    fs::write(root.join("repo/source.rs"),source("new",6))?;
    let refused=child(root,&m,"registry")?;
    ensure!(!refused.registry_open && refused.registry_locked,"{refused:#?}");
    ensure!(head(&app.state.registry,&m.pid)?==m.authority);
    ensure!(rows(&project.search,&m.pid,m.middle,&source("middle",6)).await?.0==m.middle_rows);
    finish_owned_jobs(&app).await;
    drop(project);drop(app);
    Ok((m,held,old_hits))
}

#[test]
fn independent_server_process_cannot_publish_registry_head_or_gc_until_the_sole_source_view_releases()->Result<()> {
    let owned=tempfile::tempdir()?;
    // Keep the SDK URI in the ordinary absolute spelling used by the server;
    // canonical paths are retained separately for ownership/report identity.
    let root=owned.path().to_path_buf();
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let prepared=runtime.block_on(setup(&root));
    drop(runtime); // closes every AppState/Registry/Graph/native worker first
    let result=(||->Result<()> {
        let (m,held,old_hits)=prepared?;
        let before=source_tree(&root,&m.pid)?;
        let refused=child(&root,&m,"catalog")?;
        ensure!(refused.registry_open && !refused.registry_locked,"{refused:#?}");
        ensure!(refused.before.as_ref()==Some(&m.authority) && refused.after==refused.before,"{refused:#?}");
        for call in [&refused.update,&refused.gc] {
            let call=call.as_ref().context("Missing actual MCP operation report")?;
            ensure!(!call.ok && call.text.contains("source index is already open in another process"),
                "Refusal must originate from source catalog admission: {call:?}");
        }
        ensure!(source_tree(&root,&m.pid)?==before,"A refused peer mutated source stores/retention");
        // Only now reopen an observer in the original process. It joins the
        // held catalog; the refused child never bypasses backend admission.
        let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let observed=runtime.block_on(async {
            let app=Engram::new(AppState::new(m.cfg.clone())?.0);
            ensure!(head(&app.state.registry,&m.pid)?==m.authority);
            let project=app.ensure_project_runtime(&m.pid).await.map_err(|e|anyhow::anyhow!("{e}"))?;
            ensure!(rows(&project.search,&m.pid,m.middle,&source("middle",6)).await?.0==m.middle_rows);
            let mut old=Vec::new();
            for hit in &old_hits {
                let (path,_,text,first,last)=project.search.recover_selected_source_hit(&m.pid,&held,hit)?
                    .context("Held old source body vanished after refused peer")?;
                old.push(Row {path:path.as_str().into(),pk:hit.pk.clone(),first,last,text});
            }
            old.sort();ensure!(old==m.old_rows,"Held complete old bytes changed");
            ensure!(project.search.count_vectors(&m.pid).await?==m.native_rows);
            finish_owned_jobs(&app).await;
            Ok::<(),anyhow::Error>(())
        });
        drop(runtime);observed?;
        drop(held); // the sole source-view owner, never a test-owned extra pin
        let completed=child(&root,&m,"released")?;
        ensure!(completed.registry_open && !completed.registry_locked,"{completed:#?}");
        ensure!(completed.update.as_ref().is_some_and(|r|r.ok)
            && completed.gc.as_ref().is_some_and(|r|r.ok),"{completed:#?}");
        let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let cold=runtime.block_on(async {
            let app=Engram::new(AppState::new(m.cfg.clone())?.0);
            let current=app.state.registry.active_source_generation(&m.pid)?;
            ensure!(current>m.middle,"Public peer did not publish a new authoritative HEAD");
            ensure!(Some(head(&app.state.registry,&m.pid)?)==completed.after);
            let project=app.ensure_project_runtime(&m.pid).await.map_err(|e|anyhow::anyhow!("{e}"))?;
            let current_rows=rows(&project.search,&m.pid,current,&source("new",6)).await?.0;
            ensure!(current_rows==completed.rows);
            ensure!(current_rows.iter().filter(|r|r.path=="sibling.rs").collect::<Vec<_>>()
                ==m.old_rows.iter().filter(|r|r.path=="sibling.rs").collect::<Vec<_>>());
            for obsolete in m.old_rows.iter().chain(&m.middle_rows).filter(|r|r.path=="source.rs") {
                ensure!(project.search.get_doc_by_pk(&obsolete.pk)?.is_none(),
                    "Real public GC did not collect an obsolete unleased physical row");
            }
            finish_owned_jobs(&app).await;
            Ok::<(),anyhow::Error>(())
        });
        drop(runtime);cold?;
        Ok(())
    })();
    let cleanup=owned.close();
    match (result,cleanup) {
        (Err(error),Err(cleanup))=>Err(error.context(format!("Owned process fixture cleanup failed: {cleanup}"))),
        (Err(error),_)=>Err(error),(Ok(()),cleanup)=>cleanup.map_err(Into::into),
    }
}
