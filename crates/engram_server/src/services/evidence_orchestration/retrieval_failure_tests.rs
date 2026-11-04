//! Configured HTTP failures are incomplete retrieval, not relevance measurements.
use super::*;
use anyhow::{Result, ensure};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Clone, Copy)]
enum Case {
    Immediate,
    Partial,
    Empty,
}
struct Provider {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    queries: Arc<AtomicBool>,
    seed: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    good: Arc<AtomicUsize>,
    bad: Arc<AtomicUsize>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
}
fn read_request(socket: &mut TcpStream) -> Result<serde_json::Value> {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0u8; 4096];
        let count = socket.read(&mut buffer)?;
        ensure!(count > 0, "Incomplete owned provider request");
        bytes.extend_from_slice(&buffer[..count]);
        ensure!(bytes.len() <= 64 * 1024, "Provider request exceeded 64KiB");
        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            let header = std::str::from_utf8(&bytes[..end])?;
            ensure!(
                header.starts_with("POST /embeddings HTTP/1.1"),
                "Wrong configured provider endpoint"
            );
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>())
                })
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!("Missing request length"))?;
            ensure!(length <= 60 * 1024);
            if bytes.len() >= end + 4 + length {
                return Ok(serde_json::from_slice(&bytes[end + 4..end + 4 + length])?);
            }
        }
    }
}
impl Provider {
    fn start(case: Case) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let queries = Arc::new(AtomicBool::new(false));
        let seed = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let good = Arc::new(AtomicUsize::new(0));
        let bad = Arc::new(AtomicUsize::new(0));
        let (halt, phase, s, c, g, b) = (
            stop.clone(),
            queries.clone(),
            seed.clone(),
            calls.clone(),
            good.clone(),
            bad.clone(),
        );
        let worker = std::thread::spawn(move || -> Result<()> {
            while !halt.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                if halt.load(Ordering::SeqCst) {
                    break;
                }
                socket.set_nonblocking(false)?;
                socket.set_read_timeout(Some(Duration::from_secs(1)))?;
                socket.set_write_timeout(Some(Duration::from_secs(1)))?;
                let request = match read_request(&mut socket) {
                    Ok(request) => request,
                    Err(_) if halt.load(Ordering::SeqCst) => break,
                    Err(error) => return Err(error),
                };
                let success = if phase.load(Ordering::SeqCst) {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    ensure!(n < 16, "Owned provider exceeded request bound");
                    let valid = match case {
                        Case::Immediate => false,
                        Case::Partial => n == 0,
                        Case::Empty => true,
                    };
                    if valid {
                        g.fetch_add(1, Ordering::SeqCst);
                    } else {
                        b.fetch_add(1, Ordering::SeqCst);
                    }
                    valid
                } else {
                    s.fetch_add(1, Ordering::SeqCst);
                    true
                };
                let (status, body) = if success {
                    let count = request["input"].as_array().map_or(1, Vec::len);
                    ensure!((1..=8).contains(&count));
                    let data=(0..count).map(|index|serde_json::json!({"index":index,"embedding":vec![0.25_f32;1536]})).collect::<Vec<_>>();
                    ("200 OK", serde_json::json!({"data":data}).to_string())
                } else {
                    (
                        "400 Bad Request",
                        r#"{"error":{"message":"owned benchmark provider failure"}}"#.to_owned(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes())?;
            }
            Ok(())
        });
        Ok(Self {
            address,
            stop,
            queries,
            seed,
            calls,
            good,
            bad,
            worker: Some(worker),
        })
    }
    fn close(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        drop(TcpStream::connect_timeout(
            &self.address,
            Duration::from_millis(100),
        ));
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Owned provider thread panicked"))??;
        }
        Ok(())
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn run_case(case: Case) -> Result<()> {
    let owned = tempfile::tempdir()?;
    // Runtime closes before the case's fixture, including unwinding.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result=runtime.block_on(async {
  let mut provider=Provider::start(case)?;
  let result=async {
   let root=owned.path().join("source");std::fs::create_dir(&root)?;
   let (state,_)=AppState::new(engram_core::Config {
    data_dir:owned.path().join("data"),allowed_roots:vec![root.clone()],embedding_backend:"openai".into(),
    embedding_model:Some("text-embedding-3-small".into()),openai_api_base:Some(format!("http://{}",provider.address)),
    openai_api_key:Some("owned-loopback-fixture-key".into()),embedding_request_timeout_secs:2,llm_backend:"none".into(),..Default::default()
   })?;
   let pid=format!("adp-http-{}",uuid::Uuid::new_v4());
   state.registry.register_source_project(&engram_core::ProjectRecord {project_id:pid.clone(),project_name:"ADP actual retrieval evidence".into(),
    project_type:"general".into(),directory:root.to_string_lossy().into(),created_at_ms:1,updated_at_ms:1,reindex_required_since_ms:None})?;
   let project=crate::services::project_service::ensure_project_runtime(&state,&pid).await?;
   let queries=crate::services::benchmark_service::generate_legacy_benchmark_queries();ensure!(queries.len()>1);
   let empty=matches!(case,Case::Empty);let path=if empty {"knowledge.txt"}else{"Login.aspx"};
   // Distinct seed text plus a fresh per-case engine/store prevents query-cache masking.
   let text=format!("{}; unique owned seed payload",queries[0].query);
   let paths=if empty {vec![]}else{vec![engram_core::RelPath::new(path)]};
   let head=state.registry.capture_source_head(&pid)?;let cancel=tokio_util::sync::CancellationToken::new();
   project.search.prepare_full_source_snapshot(&pid,0,1,&paths,&cancel).await?;
   project.search.index_docs(&pid,&[engram_index::IndexDoc {generation:1,chunk_id:1,path:engram_core::RelPath::new(path),
    language:"text".into(),content:text.clone(),namespace:if empty{"memory_bank".into()}else{"memory".into()},
    author:None,timestamp:None,start_line:1,end_line:1,doc_id:"owned-seed".into(),
    content_hash:blake3::hash(text.as_bytes()).to_hex().to_string()}],&cancel).await?;
   project.search.seal_source_snapshot(&pid,1)?;ensure!(state.registry.publish_source_head(&head,1)?);
   ensure!(project.search.count_vectors(&pid).await?==1,"A real valid native table must precede the provider failure");
   ensure!(provider.seed.load(Ordering::SeqCst)==1);
   if empty {ensure!(project.search.count_docs_in_generation(&pid,"memory",1)?==0);}
   provider.queries.store(true,Ordering::SeqCst);
   let result=gather_evidence(&state,&pid,&[],"",RiskProfile::Low,EvidenceDepth::Deep,
    &EvidenceOverrides {has_runtime_evidence:Some(false),..Default::default()},false,1).await?;
   match case {
    Case::Immediate|Case::Partial=>{
     ensure!(result.retrieval_mode==RetrievalMode::Skipped);
     ensure!(result.retrieval_production_ready.is_none()&&result.retrieval_ndcg.is_none()&&result.retrieval_recall.is_none(),
      "Incomplete actual retrieval must not publish a successful-prefix average or fabricated zero");
     let success=usize::from(matches!(case,Case::Partial));
     ensure!(provider.calls.load(Ordering::SeqCst)==success+1);
     ensure!(provider.good.load(Ordering::SeqCst)==success);
     ensure!(provider.bad.load(Ordering::SeqCst)==1,"An actual HTTP400 must reach ADP");
    }
    Case::Empty=>{
     ensure!(result.retrieval_mode==RetrievalMode::Live);
     ensure!(result.retrieval_production_ready==Some(false)&&result.retrieval_ndcg==Some(0.0)&&result.retrieval_recall==Some(0.0),
      "Complete successful zero-hit retrieval must remain measured low relevance");
     ensure!(provider.calls.load(Ordering::SeqCst)==queries.len());
     ensure!(provider.good.load(Ordering::SeqCst)==queries.len()&&provider.bad.load(Ordering::SeqCst)==0);
    }
   }
   Ok::<_,anyhow::Error>(())
  }.await;
  // Every ordinary outcome closes the accepted socket/listener and joins its thread.
  let closed=provider.close();result.and(closed)
 });
    drop(runtime);
    owned.close()?;
    result
}
#[test]
fn actual_remote_error_before_any_score_reports_skipped_without_metrics() -> Result<()> {
    run_case(Case::Immediate)
}
#[test]
fn actual_remote_error_after_successful_prefix_reports_skipped_without_metrics() -> Result<()> {
    run_case(Case::Partial)
}
#[test]
fn complete_successful_native_zero_hit_benchmark_reports_live_zero_metrics() -> Result<()> {
    run_case(Case::Empty)
}
