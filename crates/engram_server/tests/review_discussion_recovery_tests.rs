//! Actual public imported-discussion retention/recovery; no fabricated verdicts
//! or tuple-only budget mirrors. Every fixture owns its stores/runtime/HTTP child.
use engram_core::Config;
use engram_server::{Engram,state::AppState};
use rmcp::handler::server::tool::Parameters;
use serde_json::{json,Value};
use std::path::{Path,PathBuf};

fn text(response:&rmcp::model::CallToolResult)->&str {&response.content[0].as_text().unwrap().text}
async fn fixture(parent:&Path,mut config:Config)->anyhow::Result<(Engram,String,PathBuf,Config)> {
    let root=parent.join("repo");std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("a.rs"),"pub fn retained_original_source() {}\n")?;
    config.data_dir=parent.join("data");config.allowed_roots=vec![root.clone()];
    let (state,_)=AppState::new(config.clone())?;
    let engram=Engram::new(state);
    let response=engram.index_project(Parameters(serde_json::from_value(json!({"directory":root,
        "project_name":"Captured review evidence","project_type":"rust","wait":true}))?)).await?;
    let pid=text(&response).split("project_id: ").nth(1).ok_or_else(||anyhow::anyhow!("missing project ID"))?
        .lines().next().unwrap().trim().to_owned();
    Ok((engram,pid,root,config))
}
fn original(marker:&str)->Value {
    json!({"pr_id":100,"thread_id":7,"pr_title":"Qualified export exception","pr_author":"Author",
        "pr_date":"2026-01-01T12:00:00Z","status":"fixed","file_path":"src/export.rs","line":10,
        "unknown_thread_field":{"future_metadata":"preserve exactly", "n":17},
        "comments":[
            {"author":"Reviewer","text":format!("Export guidance {} END_FINDING_{marker}","reasoning ".repeat(450)),
                "likes":["Owner"],"comment_id":91,"created_at":"2026-01-01T12:01:00Z"},
            {"author":"Owner","text":format!("This is intended behaviour. {} EXCEPTION_TAIL_{marker}: only the old demonstration; production guard is mandatory.","qualified explanation ".repeat(250)),"likes":[],"unknown_reply_field":true},
            {"author":"System record","text":"System/blank comment metadata must survive original input capture.","commentType":"system","unknown_system_field":{"nested":[1,2,3]}},
            {"author":"Blank record","text":"","likes":[],"unmodeled_blank_metadata":"preserve"}
        ]})
}
async fn ingest(engram:&Engram,pid:&str,file:&str)->Result<rmcp::model::CallToolResult,rmcp::ErrorData> {
    engram.ingest_review_verdicts(Parameters(serde_json::from_value(json!({"project_id":pid,
        "source":"json_file","file_path":file,"decision_makers":["Owner"]})).unwrap())).await
}
async fn catalog(engram:&Engram,pid:&str)->anyhow::Result<Value> {
    let response=engram.get_review_decisions(Parameters(serde_json::from_value(json!({
        "project_id":pid,"review_id":"PR-100","discussion":{}}))?)).await?;
    Ok(serde_json::from_str(text(&response))?)
}
async fn recover(engram:&Engram,mut call:Value)->anyhow::Result<(String,Value)> {
    let mut assembled=String::new();
    let expected=call["arguments"]["discussion"]["capture_blake3"].as_str().unwrap().to_owned();
    let mut recovery_hash=None;let mut pages=0;
    loop {
        pages+=1;anyhow::ensure!(pages<=256,"bounded capture recovery must finish");
        let response=engram.get_review_decisions(Parameters(serde_json::from_value(call["arguments"].clone())?)).await?;
        anyhow::ensure!(text(&response).len()<=24*1024);
        let page:Value=serde_json::from_str(text(&response))?;
        anyhow::ensure!(page["capture_blake3"]==expected);
        let observed=page["recovery_blake3"].as_str().unwrap().to_owned();
        if let Some(expected)=&recovery_hash {anyhow::ensure!(expected==&observed);}else{recovery_hash=Some(observed);}
        anyhow::ensure!(page["start_byte"]==assembled.len());assembled.push_str(page["capture_json"].as_str().unwrap());
        anyhow::ensure!(page["end_byte"]==assembled.len());
        if page["complete"]==true {anyhow::ensure!(page["next_call"].is_null());break;}
        call=page["next_call"].clone();
    }
    anyhow::ensure!(blake3::hash(assembled.as_bytes()).to_hex().as_str()==recovery_hash.as_deref().unwrap());
    let envelope:Value=serde_json::from_str(&assembled)?;
    let exact_metadata=envelope["capture_metadata_json"].as_str().unwrap();
    anyhow::ensure!(blake3::hash(exact_metadata.as_bytes()).to_hex().as_str()==expected);
    let mut decoded:Value=serde_json::from_str(exact_metadata)?;
    let body=envelope["raw_utf8"].as_str().unwrap();
    anyhow::ensure!(decoded["raw_utf8_blake3"]==blake3::hash(body.as_bytes()).to_hex().as_str());
    anyhow::ensure!(decoded["raw_utf8_bytes"]==body.len());decoded["raw_utf8"]=json!(body);
    Ok((assembled,decoded))
}

async fn precedents(engram:&Engram,pid:&str)->anyhow::Result<String> {
    let result=engram.get_review_precedents(Parameters(serde_json::from_value(json!({
        "project_id":pid,"query":"export guidance qualified","file_path":"src/export.rs"}))?)).await?;
    Ok(text(&result).to_owned())
}

#[test]
fn public_full_discussion_recovery_preserves_original_bytes_unknown_fields_and_old_reimport_versions() -> anyhow::Result<()> {
    let owned=tempfile::tempdir()?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result: anyhow::Result<()> = runtime.block_on(async {
        let (engram,pid,root,config)=fixture(owned.path(),Config {embedding_backend:"fts_only".into(),..Default::default()}).await?;
        let head=engram.state.registry.capture_source_head(&pid)?;
        let source=original("OLD_é🙂");
        let exact=format!("  {}\r\n",serde_json::to_string(&source)?);
        std::fs::write(root.join("threads.jsonl"),&exact)?;
        let ingested=ingest(&engram,&pid,"threads.jsonl").await?;
        anyhow::ensure!(text(&ingested).contains("stored=1") && text(&ingested).contains("Recovery catalog"));
        let before_catalog=catalog(&engram,&pid).await?;
        let first=before_catalog["captures"][0].clone();
        let (capture_bytes,captured)=recover(&engram,first.clone()).await?;
        anyhow::ensure!(captured["raw_utf8"]==exact);
        anyhow::ensure!(serde_json::from_str::<Value>(captured["raw_utf8"].as_str().unwrap())?==source);
        anyhow::ensure!(captured["raw_utf8_blake3"]==blake3::hash(exact.as_bytes()).to_hex().as_str());
        anyhow::ensure!(captured["provenance"]["source_file_blake3"]==blake3::hash(exact.as_bytes()).to_hex().as_str());
        anyhow::ensure!(captured["provenance"]["source_line"]==1);
        let found=precedents(&engram,&pid).await?;
        anyhow::ensure!(found.contains("capture_reference") && found.contains("get_review_decisions"));
        anyhow::ensure!(!found.contains("EXCEPTION_TAIL_OLD"),"search summary must remain clipped; recovery, not larger index payload, supplies the missing tail");
        let duplicate=ingest(&engram,&pid,"threads.jsonl").await?;
        anyhow::ensure!(text(&duplicate).contains("stored=0, unchanged=1"));
        anyhow::ensure!(catalog(&engram,&pid).await?["captures"]==before_catalog["captures"]);
        let next=original("NEW_é🙂");
        let next_exact=format!("{}\n",serde_json::to_string(&next)?);
        std::fs::write(root.join("threads.jsonl"),&next_exact)?;
        ingest(&engram,&pid,"threads.jsonl").await?;
        let after=catalog(&engram,&pid).await?;
        anyhow::ensure!(after["captures"].as_array().unwrap().len()==2);
        let newest=after["captures"].as_array().unwrap().iter().find(|entry|
            entry["arguments"]["discussion"]["capture_blake3"]!=first["arguments"]["discussion"]["capture_blake3"]).unwrap().clone();
        let (_,new_capture)=recover(&engram,newest.clone()).await?;
        anyhow::ensure!(new_capture["raw_utf8"]==next_exact);
        anyhow::ensure!(recover(&engram,first.clone()).await?.0==capture_bytes);
        anyhow::ensure!(precedents(&engram,&pid).await?.contains(newest["arguments"]["discussion"]["capture_blake3"].as_str().unwrap()));
        anyhow::ensure!(engram.state.registry.source_head_is_current(&head)?);
        // Full source recovery is separate from immutable provider-neutral events.
        let default=engram.get_review_decisions(Parameters(serde_json::from_value(json!({"project_id":pid,"review_id":"PR-100"}))?)).await?;
        anyhow::ensure!(serde_json::from_str::<Value>(text(&default))?["events"]==json!([]));
        for change in ["wrong_review","wrong_thread","unknown_hash","bad_cursor","wrong_recovery_hash"] {
            let mut args=first["arguments"].clone();
            match change {
                "wrong_review"=>args["review_id"]=json!("PR-101"),
                "wrong_thread"=>args["discussion"]["thread_id"]=json!(8),
                "unknown_hash"=>args["discussion"]["capture_blake3"]=json!("0".repeat(64)),
                "bad_cursor"=>{
                    args["discussion"]["offset"]=json!(capture_bytes.find('é').unwrap()+1);
                    args["discussion"]["expected_recovery_blake3"]=json!(blake3::hash(capture_bytes.as_bytes()).to_hex().to_string());
                }
                "wrong_recovery_hash"=>args["discussion"]["expected_recovery_blake3"]=json!("0".repeat(64)),
                _=>unreachable!(),
            }
            anyhow::ensure!(engram.get_review_decisions(Parameters(serde_json::from_value(args)?)).await.is_err(),"{change}");
        }
        drop(engram);
        let (state,_)=AppState::new(config)?;
        let reopened=Engram::new(state);
        anyhow::ensure!(recover(&reopened,first).await?.0==capture_bytes);
        anyhow::ensure!(recover(&reopened,newest).await?.1["raw_utf8"]==next_exact);
        Ok(())
    });
    drop(runtime);let cleanup=owned.close();result?;cleanup?;Ok(())
}

#[test]
fn public_capture_admission_failures_preserve_old_summary_capture_and_source_head() -> anyhow::Result<()> {
    let owned=tempfile::tempdir()?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result: anyhow::Result<()> = runtime.block_on(async {
        let (engram,pid,root,config)=fixture(owned.path(),Config {embedding_backend:"fts_only".into(),..Default::default()}).await?;
        let original=format!("{}\n",original("PRESERVED"));
        std::fs::write(root.join("threads.jsonl"),&original)?;
        ingest(&engram,&pid,"threads.jsonl").await?;
        let original_refs=catalog(&engram,&pid).await?;
        let first=original_refs["captures"][0].clone();
        let original_capture=recover(&engram,first.clone()).await?.0;
        let original_summary=precedents(&engram,&pid).await?;
        let head=engram.state.registry.capture_source_head(&pid)?;
        let too_large={let mut value:Value=serde_json::from_str(&original)?;value["unknown_padding"]=json!("x".repeat(256*1024));value.to_string()};
        for (name,raw) in [("oversized-thread",too_large),("malformed","{invalid".into()),
            ("too-many-rows","\n".repeat(1025)),("oversized-file","x".repeat(4*1024*1024+1))] {
            std::fs::write(root.join("threads.jsonl"),raw)?;
            anyhow::ensure!(ingest(&engram,&pid,"threads.jsonl").await.is_err(),"{name}");
            anyhow::ensure!(catalog(&engram,&pid).await?["captures"]==original_refs["captures"]);
            anyhow::ensure!(recover(&engram,first.clone()).await?.0==original_capture);
            anyhow::ensure!(precedents(&engram,&pid).await?==original_summary);
            anyhow::ensure!(engram.state.registry.source_head_is_current(&head)?);
        }
        // Actual lost/inconsistent accounting in this cold-owned Registry must
        // block EMPTY and fully filtered PUBLIC imports too. Different incoming
        // decision-maker configuration makes an accidental AUTHORS_PATH write
        // observable through the normal public precedent response.
        let quota_key=format!("{pid}\0review_discussion_quota_v1");
        let quota_bytes=engram.state.registry.map_meta_bounded(&pid,"review_discussion_quota_v1",24,
            |bytes|Ok(bytes.to_vec()))?.unwrap();
        let registry_path=config.data_dir.join("registry").join("registry.redb");
        drop(engram);
        for fault in ["missing","mismatched"] {
            {
                let db=engram_core::storage::open_database(&registry_path,engram_core::storage::DatabaseCache::Metadata)?;
                let write=db.begin_write()?;
                {
                    let mut table=write.open_table(redb::TableDefinition::<&str,&[u8]>::new("meta"))?;
                    if fault=="missing" {table.remove(quota_key.as_str())?;}
                    else {
                        let mut wrong=quota_bytes.clone();wrong[8..].fill(0);
                        table.insert(quota_key.as_str(),wrong.as_slice())?;
                    }
                }
                write.commit()?;
            }
            let (state,_)=AppState::new(config.clone())?;
            let reopened=Engram::new(state);
            for filtered in [false,true] {
                std::fs::write(root.join("threads.jsonl"),if filtered{original.as_str()}else{""})?;
                let attempted=reopened.ingest_review_verdicts(Parameters(serde_json::from_value(json!({
                    "project_id":pid,"source":"json_file","file_path":"threads.jsonl",
                    "decision_makers":["Incorrect replacement owner"],"min_pr_id":if filtered{101}else{0}}))?)).await;
                anyhow::ensure!(attempted.is_err(),"{fault} quota must refuse empty/filtered import");
                anyhow::ensure!(catalog(&reopened,&pid).await?["captures"]==original_refs["captures"]);
                anyhow::ensure!(recover(&reopened,first.clone()).await?.0==original_capture);
                anyhow::ensure!(precedents(&reopened,&pid).await?==original_summary,
                    "{fault} quota must not replace existing current author configuration or summary");
                anyhow::ensure!(reopened.state.registry.source_head_is_current(&head)?);
            }
            drop(reopened);
            {
                let db=engram_core::storage::open_database(&registry_path,engram_core::storage::DatabaseCache::Metadata)?;
                let write=db.begin_write()?;
                write.open_table(redb::TableDefinition::<&str,&[u8]>::new("meta"))?
                    .insert(quota_key.as_str(),quota_bytes.as_slice())?;
                write.commit()?;
            }
        }
        Ok(())
    });
    drop(runtime);let cleanup=owned.close();result?;cleanup?;Ok(())
}


#[cfg(feature="vector")]
mod failed_publication {
    use super::*;
    use std::{io::{Read,Write},net::{TcpListener,SocketAddr},sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}},thread,time::Duration};
    struct Provider {
        address:SocketAddr,stop:Arc<AtomicBool>,fail:Arc<AtomicBool>,calls:Arc<AtomicUsize>,errors:Arc<AtomicUsize>,worker:Option<thread::JoinHandle<()>>,
    }
    impl Provider {
        fn start()->anyhow::Result<Self> {
            let listener=TcpListener::bind("127.0.0.1:0")?;listener.set_nonblocking(true)?;
            let address=listener.local_addr()?;
            let stop=Arc::new(AtomicBool::new(false));let fail=Arc::new(AtomicBool::new(false));
            let calls=Arc::new(AtomicUsize::new(0));let errors=Arc::new(AtomicUsize::new(0));
            let (st,fl,cs,es)=(stop.clone(),fail.clone(),calls.clone(),errors.clone());
            let worker=thread::spawn(move || {
                while !st.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut socket,_))=>{
                            let handled=(||->anyhow::Result<()> {
                                // Keep the listener cancellable, but each accepted request
                                // uses bounded blocking I/O on Windows too.
                                socket.set_nonblocking(false)?;
                                socket.set_read_timeout(Some(Duration::from_secs(2)))?;
                                socket.set_write_timeout(Some(Duration::from_secs(2)))?;
                                let mut request=Vec::new();let mut buffer=[0u8;4096];
                                let (header,body_len)=loop {
                                    let n=socket.read(&mut buffer)?;anyhow::ensure!(n>0 && request.len()+n<=1024*1024);
                                    request.extend_from_slice(&buffer[..n]);
                                    if let Some(end)=request.windows(4).position(|v|v==b"\r\n\r\n") {
                                        let headers=std::str::from_utf8(&request[..end])?;
                                        let length=headers.lines().find_map(|line|line.split_once(':')
                                            .filter(|(name,_)|name.eq_ignore_ascii_case("content-length"))
                                            .and_then(|(_,n)|n.trim().parse::<usize>().ok())).ok_or_else(||anyhow::anyhow!("missing request length"))?;
                                        anyhow::ensure!(length<=1024*1024);break(end+4,length);
                                    }
                                };
                                while request.len()<header+body_len {
                                    let n=socket.read(&mut buffer)?;anyhow::ensure!(n>0 && request.len()+n<=1024*1024);
                                    request.extend_from_slice(&buffer[..n]);
                                }
                                let request:Value=serde_json::from_slice(&request[header..header+body_len])?;
                                cs.fetch_add(1,Ordering::SeqCst);
                                let (status,body)=if fl.load(Ordering::SeqCst) {
                                    es.fetch_add(1,Ordering::SeqCst);
                                    ("400 Bad Request",json!({"error":{"message":"controlled owned embedding refusal","type":"invalid_request_error"}}).to_string())
                                } else {
                                    let count=request["input"].as_array().map_or(1,Vec::len);
                                    let data=(0..count).map(|index|json!({"index":index,"object":"embedding","embedding":vec![0.125f32;1536]})).collect::<Vec<_>>();
                                    ("200 OK",json!({"object":"list","model":"text-embedding-3-small","data":data}).to_string())
                                };
                                write!(socket,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())?;
                                socket.flush()?;Ok(())
                            })();
                            let _=handled; // Failed/closed client is confined to this owned socket.
                        }
                        Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>thread::sleep(Duration::from_millis(5)),
                        Err(_)=>break,
                    }
                }
            });
            Ok(Self {address,stop,fail,calls,errors,worker:Some(worker)})
        }
        fn close(&mut self)->anyhow::Result<()> {
            self.stop.store(true,Ordering::SeqCst);
            if let Some(worker)=self.worker.take() {worker.join().map_err(|_|anyhow::anyhow!("owned HTTP provider panicked"))?;}
            Ok(())
        }
    }
    impl Drop for Provider {fn drop(&mut self){let _=self.close();}}

    #[test]
    fn real_remote_embedding_refusal_keeps_retained_capture_discoverable_and_retry_does_not_consume_quota_twice()->anyhow::Result<()> {
        let owned=tempfile::tempdir()?;
        let mut provider=Provider::start()?;
        let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let result:anyhow::Result<()>=runtime.block_on(async {
            let config=Config {embedding_backend:"openai".into(),embedding_model:Some("text-embedding-3-small".into()),
                openai_api_base:Some(format!("http://{}",provider.address)),openai_api_key:Some("owned-fixture-key".into()),
                embedding_request_timeout_secs:3,..Default::default()};
            let (engram,pid,root,_)=fixture(owned.path(),config).await?;
            let old_exact=format!("{}\n",original("OLD"));
            std::fs::write(root.join("threads.jsonl"),&old_exact)?;
            ingest(&engram,&pid,"threads.jsonl").await?;
            let old_catalog=catalog(&engram,&pid).await?;
            let old=old_catalog["captures"][0].clone();
            let old_hash=old["arguments"]["discussion"]["capture_blake3"].as_str().unwrap().to_owned();
            let old_recovery=recover(&engram,old.clone()).await?;
            anyhow::ensure!(old_recovery.1["raw_utf8"]==old_exact);
            let head=engram.state.registry.capture_source_head(&pid)?;
            let new_exact=format!("{}\n",original("AFTER_REFUSAL"));
            std::fs::write(root.join("threads.jsonl"),&new_exact)?;
            let calls_before=provider.calls.load(Ordering::SeqCst);
            provider.fail.store(true,Ordering::SeqCst);
            let failed=ingest(&engram,&pid,"threads.jsonl").await.unwrap_err();
            anyhow::ensure!(provider.calls.load(Ordering::SeqCst)>calls_before && provider.errors.load(Ordering::SeqCst)>0,
                "wanted actual configured native/remote write error, not a fabricated fault result");
            anyhow::ensure!(failed.message.contains("publication was not confirmed")
                && failed.message.contains("partial cross-store work is possible")
                && failed.message.contains("immutable evidence is retained") && failed.message.contains("get_review_decisions"),"{failed}");
            provider.fail.store(false,Ordering::SeqCst);
            let retained=catalog(&engram,&pid).await?;
            anyhow::ensure!(retained["captures"].as_array().unwrap().len()==2);
            let new=retained["captures"].as_array().unwrap().iter().find(|value|
                value["arguments"]["discussion"]["capture_blake3"]!=old_hash).unwrap().clone();
            let new_recovery=recover(&engram,new.clone()).await?;
            anyhow::ensure!(new_recovery.1["raw_utf8"]==new_exact,
                "Failed summary publication must retain the complete exact new input");
            anyhow::ensure!(recover(&engram,old.clone()).await?.0==old_recovery.0,
                "Failed summary publication must not alter the complete old capture");
            anyhow::ensure!(engram.state.registry.source_head_is_current(&head)?,
                "A review namespace failure must not change selected source authority");
            let retained_quota=engram.state.registry.map_meta_bounded(&pid,"review_discussion_quota_v1",24,
                |bytes|Ok(bytes.to_vec()))?.unwrap();
            // Generic indexing commits lexical changes before native embedding.
            // A failed import may therefore expose old or new mutable summary
            // content; its public references must still recover exact retained
            // evidence. Do not fabricate cross-store rollback as a test oracle.
            let summary=precedents(&engram,&pid).await?;
            let references=summary.lines().filter_map(|line|
                line.strip_prefix("Imported discussion recovery: "))
                .map(serde_json::from_str::<Value>).collect::<Result<Vec<_>,_>>()?;
            anyhow::ensure!(!references.is_empty(),"Partial mutable publication must keep recoverable public evidence leads");
            for reference in references {
                anyhow::ensure!(reference["status"]=="capture_reference");
                let recovered=recover(&engram,reference["recovery"].clone()).await?;
                anyhow::ensure!(recovered.0==old_recovery.0 || recovered.0==new_recovery.0,
                    "Every visible summary reference must recover one complete known immutable input");
            }
            let calls_before_retry=provider.calls.load(Ordering::SeqCst);
            let errors_before_retry=provider.errors.load(Ordering::SeqCst);
            let retry=ingest(&engram,&pid,"threads.jsonl").await?;
            anyhow::ensure!(text(&retry).contains("stored=0, unchanged=1"));
            anyhow::ensure!(provider.calls.load(Ordering::SeqCst)>calls_before_retry
                && provider.errors.load(Ordering::SeqCst)==errors_before_retry,
                "Retry must actually complete the configured remote write without another injected refusal");
            anyhow::ensure!(catalog(&engram,&pid).await?["captures"]==retained["captures"]);
            anyhow::ensure!(engram.state.registry.map_meta_bounded(&pid,"review_discussion_quota_v1",24,
                |bytes|Ok(bytes.to_vec()))?==Some(retained_quota),
                "Retry must consume neither immutable object quota nor retained byte quota twice");
            anyhow::ensure!(recover(&engram,old).await?.0==old_recovery.0
                && recover(&engram,new.clone()).await?.0==new_recovery.0);
            anyhow::ensure!(precedents(&engram,&pid).await?.contains(new["arguments"]["discussion"]["capture_blake3"].as_str().unwrap()));
            anyhow::ensure!(engram.state.registry.source_head_is_current(&head)?);
            Ok(())
        });
        drop(runtime);let joined=provider.close();drop(provider);let cleanup=owned.close();
        result?;joined?;cleanup?;Ok(())
    }
}


#[test]
fn public_two_thread_reimport_reuses_unchanged_raw_body_and_retains_exact_old_source_provenance()->anyhow::Result<()> {
    let owned=tempfile::tempdir()?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result:anyhow::Result<()>=runtime.block_on(async {
        let (engram,pid,root,_)=fixture(owned.path(),Config {embedding_backend:"fts_only".into(),..Default::default()}).await?;
        let a=original("INITIAL_A");let mut b=original("UNCHANGED_B");b["thread_id"]=json!(8);
        let b_raw=format!("{b}\n");let initial=format!("{a}\n{b_raw}");
        std::fs::write(root.join("threads.jsonl"),&initial)?;
        let first_write=ingest(&engram,&pid,"threads.jsonl").await?;
        anyhow::ensure!(text(&first_write).contains("stored_payloads=2") && text(&first_write).contains("project_retained_objects=4"));
        let before=catalog(&engram,&pid).await?;
        let a_old=before["captures"].as_array().unwrap().iter().find(|entry|entry["arguments"]["discussion"]["thread_id"]==7).unwrap().clone();
        let b_old=before["captures"].as_array().unwrap().iter().find(|entry|entry["arguments"]["discussion"]["thread_id"]==8).unwrap().clone();
        let old_a=recover(&engram,a_old.clone()).await?;let old_b=recover(&engram,b_old.clone()).await?;
        anyhow::ensure!(old_b.1["raw_utf8"]==b_raw && old_b.1["provenance"]["source_line"]==2);
        let next=format!("{}\n{b_raw}",original("CHANGED_A"));
        std::fs::write(root.join("threads.jsonl"),&next)?;
        let second_write=ingest(&engram,&pid,"threads.jsonl").await?;
        // Provenance changes for both threads because the source-file hash
        // changed. Only A's large raw payload is new; B reuses its old object.
        let report=text(&second_write);
        anyhow::ensure!(report.contains("stored=2, unchanged=0, stored_payloads=1, reused_payloads=1")
            && report.contains("project_retained_objects=7") && report.contains("project_retained_captures=4")
            && report.contains("project_retained_payloads=3"),"{report}");
        let after=catalog(&engram,&pid).await?;anyhow::ensure!(after["captures"].as_array().unwrap().len()==4);
        let a_new=after["captures"].as_array().unwrap().iter().find(|entry|entry["arguments"]["discussion"]["thread_id"]==7
            && entry["arguments"]["discussion"]["capture_blake3"]!=a_old["arguments"]["discussion"]["capture_blake3"]).unwrap().clone();
        let b_new=after["captures"].as_array().unwrap().iter().find(|entry|entry["arguments"]["discussion"]["thread_id"]==8
            && entry["arguments"]["discussion"]["capture_blake3"]!=b_old["arguments"]["discussion"]["capture_blake3"]).unwrap().clone();
        let new_a=recover(&engram,a_new).await?;let new_b=recover(&engram,b_new).await?;
        anyhow::ensure!(new_b.1["raw_utf8"]==b_raw && new_b.1["raw_utf8_blake3"]==old_b.1["raw_utf8_blake3"]);
        anyhow::ensure!(new_a.1["raw_utf8_blake3"]!=old_a.1["raw_utf8_blake3"]);
        let old_file_hash=blake3::hash(initial.as_bytes()).to_hex().to_string();let new_file_hash=blake3::hash(next.as_bytes()).to_hex().to_string();
        for capture in [&old_a.1,&old_b.1] {anyhow::ensure!(capture["provenance"]["source_file_blake3"]==old_file_hash);}
        for capture in [&new_a.1,&new_b.1] {anyhow::ensure!(capture["provenance"]["source_file_blake3"]==new_file_hash);}
        anyhow::ensure!(recover(&engram,a_old).await?.0==old_a.0 && recover(&engram,b_old).await?.0==old_b.0);
        Ok(())
    });drop(runtime);let cleanup=owned.close();result?;cleanup?;Ok(())
}
