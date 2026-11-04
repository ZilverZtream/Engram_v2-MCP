//! Whole source revisions through public updates; all inputs belong to each fixture.
use super::*;
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Body {
    pk: String,
    first: u32,
    last: u32,
    text: String,
}
type Groups = BTreeMap<String, Vec<Body>>;

fn source(version: &str, lines: usize) -> String {
    (0..lines).map(|n| format!(
        "pub fn {version}_{n:03}() -> usize {{ {n} }} // s02marker {version} independently identifiable source line {n:03}\n"
    )).collect()
}

fn config(root: &Path, backend: &str) -> Config {
    Config {
        data_dir: root.join("data"),
        allowed_roots: vec![root.join("repo")],
        embedding_backend: backend.into(),
        max_chunks_per_file: 80,
        max_parse_concurrency: 1,
        llm_backend: "none".into(),
        ..Default::default()
    }
}

fn open(cfg: Config) -> Result<Engram> {
    Ok(Engram::new(AppState::new(cfg)?.0))
}

fn create(root: &Path, cfg: Config, text: &str) -> Result<Engram> {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    drop(git2::Repository::init(&repo)?);
    std::fs::write(repo.join("source.rs"), text)?;
    std::fs::write(repo.join("sibling.rs"), source("sibling", 1))?;
    let app = open(cfg)?;
    app.state.registry.put_project(&ProjectRecord {
        project_id: PROJECT.into(),
        project_name: "Complete source revisions".into(),
        project_type: "general".into(),
        directory: repo.to_string_lossy().into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        reindex_required_since_ms: None,
    })?;
    Ok(app)
}

async fn initial(app: &Engram) -> Result<()> {
    app.handle_repair_project(serde_json::from_value(json!({
        "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
    }))?)
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    finish_owned_jobs(app).await;
    Ok(())
}

async fn update(app: &Engram) -> Result<()> {
    app.handle_update_project(serde_json::from_value(json!({
        "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
    }))?)
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

fn generation(app: &Engram) -> Result<u64> {
    app.state.registry.active_source_generation(PROJECT)
}

async fn groups(app: &Engram, generation: u64, expected: &[(&str, &str)]) -> Result<Groups> {
    let ps = app
        .ensure_project_runtime(PROJECT)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let held = ps.search.pin_source_generation(PROJECT, generation)?;
    groups_under(app, generation, expected, &held).await
}

async fn groups_under(
    app: &Engram,
    generation: u64,
    expected: &[(&str, &str)],
    held: &engram_index::SourceSnapshotGuard,
) -> Result<Groups> {
    let ps = app
        .ensure_project_runtime(PROJECT)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut q = query(generation, "s02marker");
    q.top_k = 2048;
    let hits = ps.search.lexical_search(&q)?;
    ensure!(
        hits.len()
            == ps
                .search
                .count_docs_in_generation(PROJECT, "memory", generation)?,
        "Common marker must enumerate the complete selected source group"
    );
    let lexical: BTreeSet<_> = hits.iter().map(|h| h.pk.clone()).collect();
    ensure!(
        lexical.len() == hits.len(),
        "Duplicate selected physical identity"
    );
    #[cfg(feature = "vector")]
    if app.state.cfg.embedding_backend != "fts_only" {
        let vectors = ps
            .search
            .vector_search(&q, &CancellationToken::new())
            .await?;
        ensure!(
            vectors
                .iter()
                .map(|h| h.pk.clone())
                .collect::<BTreeSet<_>>()
                == lexical
                && vectors.len() == hits.len()
                && ps
                    .search
                    .count_vectors_in_generation(PROJECT, generation)
                    .await?
                    == hits.len(),
            "Native vectors and lexical selection must contain exactly the same complete groups"
        );
    }
    let mut result = Groups::new();
    for hit in hits {
        let (path, _, text, first, last) = ps
            .search
            .recover_selected_source_hit(PROJECT, held, &hit)?
            .ok_or_else(|| anyhow::anyhow!("Selected source body missing"))?;
        result.entry(path.as_str().into()).or_default().push(Body {
            pk: hit.pk,
            first,
            last,
            text,
        });
    }
    ensure!(
        result.keys().map(String::as_str).collect::<BTreeSet<_>>()
            == expected.iter().map(|(path, _)| *path).collect(),
        "Selected paths are incomplete or obsolete"
    );
    for (path, text) in expected {
        let lines: Vec<_> = text.lines().collect();
        let mut covered = BTreeSet::new();
        let bodies = result
            .get_mut(*path)
            .ok_or_else(|| anyhow::anyhow!("Missing {path}"))?;
        bodies.sort();
        for body in bodies {
            ensure!(body.first > 0 && body.first <= body.last && body.last as usize <= lines.len());
            let wanted = lines[body.first as usize - 1..body.last as usize].join("\n") + "\n";
            ensure!(
                body.text == wanted,
                "Stored body differs from known source lines for {path}"
            );
            covered.extend(body.first..=body.last);
        }
        ensure!(
            covered == (1..=lines.len() as u32).collect(),
            "A source revision is missing lines: {path}"
        );
    }
    Ok(result)
}

fn renamed(bodies: &[Body]) -> Vec<(u32, u32, String)> {
    let mut rows: Vec<_> = bodies
        .iter()
        .map(|b| (b.first, b.last, b.text.clone()))
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn multi_chunk_rename_preserves_complete_old_and_current_revisions_across_cold_reopen()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let cfg = config(
        temp.path(),
        if cfg!(feature = "vector") {
            "local"
        } else {
            "fts_only"
        },
    );
    let text = source("original", 8);
    let sibling = source("sibling", 1);
    let app = create(temp.path(), cfg.clone(), &text)?;
    let result = async {
        initial(&app).await?;
        let old_gen = generation(&app)?;
        let ps = app
            .ensure_project_runtime(PROJECT)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let held = ps.search.pin_source_generation(PROJECT, old_gen)?;
        let before = groups_under(
            &app,
            old_gen,
            &[("source.rs", &text), ("sibling.rs", &sibling)],
            &held,
        )
        .await?;
        ensure!(
            before["source.rs"].len() >= 2,
            "Fixture must exercise a real multi-chunk file"
        );
        std::fs::rename(
            temp.path().join("repo/source.rs"),
            temp.path().join("repo/renamed.rs"),
        )?;
        update(&app).await?;
        let current = generation(&app)?;
        ensure!(current > old_gen);
        let after = groups(
            &app,
            current,
            &[("renamed.rs", &text), ("sibling.rs", &sibling)],
        )
        .await?;
        ensure!(renamed(&after["renamed.rs"]) == renamed(&before["source.rs"]));
        ensure!(
            after["sibling.rs"] == before["sibling.rs"],
            "Inherited sibling physical identity changed"
        );
        ensure!(
            groups_under(
                &app,
                old_gen,
                &[("source.rs", &text), ("sibling.rs", &sibling)],
                &held,
            )
            .await?
                == before,
            "Pinned old bytes must survive the update's collection"
        );
        drop(held);
        drop(ps);
        Ok::<_, anyhow::Error>((current, after))
    }
    .await;
    finish_owned_jobs(&app).await;
    drop(app);
    let reopened = open(cfg)?;
    let verification = async {
        let (current, expected) = result?;
        ensure!(generation(&reopened)? == current);
        ensure!(
            groups(
                &reopened,
                current,
                &[("renamed.rs", &text), ("sibling.rs", &sibling)]
            )
            .await?
                == expected
        );
        Ok(())
    }
    .await;
    finish_owned_jobs(&reopened).await;
    drop(reopened);
    temp.close()?;
    verification
}

#[cfg(feature = "vector")]
mod provider {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    #[derive(Default)]
    pub(super) struct Transcript {
        pub replacement_ok: Vec<Vec<String>>,
        pub refused: Vec<String>,
    }
    pub(super) struct Provider {
        address: SocketAddr,
        stop: Arc<AtomicBool>,
        fail: Arc<AtomicBool>,
        pub seen: Arc<Mutex<Transcript>>,
        worker: Option<std::thread::JoinHandle<Result<()>>>,
    }
    impl Provider {
        pub fn new() -> Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let stop = Arc::new(AtomicBool::new(false));
            let fail = Arc::new(AtomicBool::new(false));
            let seen = Arc::new(Mutex::new(Transcript::default()));
            let (stopping, failing, record) = (stop.clone(), fail.clone(), seen.clone());
            let worker = std::thread::spawn(move || -> Result<()> {
                loop {
                    let (mut stream, _) = listener.accept()?;
                    if stopping.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        ensure!(header.len() < 16384);
                        let mut byte = [0];
                        stream.read_exact(&mut byte)?;
                        header.push(byte[0]);
                    }
                    let header = std::str::from_utf8(&header)?;
                    ensure!(header.starts_with("POST /v1/embeddings HTTP/1.1\r\n"));
                    let length = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>())
                        })
                        .transpose()?
                        .ok_or_else(|| anyhow::anyhow!("Missing request length"))?;
                    ensure!(length <= 1024 * 1024);
                    let mut bytes = vec![0; length];
                    stream.read_exact(&mut bytes)?;
                    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
                    let inputs: Vec<String> = if let Some(text) = body["input"].as_str() {
                        vec![text.into()]
                    } else {
                        body["input"]
                            .as_array()
                            .ok_or_else(|| anyhow::anyhow!("Expected batch"))?
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .map(String::from)
                                    .ok_or_else(|| anyhow::anyhow!("Expected text"))
                            })
                            .collect::<Result<_>>()?
                    };
                    ensure!(!inputs.is_empty() && inputs.len() <= 64);
                    let replacement = inputs.iter().all(|v| v.contains("replacement"));
                    let refuse = {
                        let mut observed = record
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Transcript poisoned"))?;
                        let refuse = replacement
                            && failing.load(Ordering::SeqCst)
                            && !observed.replacement_ok.is_empty();
                        if refuse {
                            observed.refused = inputs.clone();
                        } else if replacement {
                            observed.replacement_ok.push(inputs.clone());
                        }
                        refuse
                    };
                    let response = if refuse {
                        b"{\"error\":\"owned later replacement batch refused\"}".to_vec()
                    } else {
                        serde_json::to_vec(
                            &json!({"data":inputs.iter().enumerate().map(|(index,_)|
                            json!({"index":index,"embedding":vec![0.25f32;1536]})).collect::<Vec<_>>()}),
                        )?
                    };
                    write!(
                        stream,
                        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        if refuse { "400 Bad Request" } else { "200 OK" },
                        response.len()
                    )?;
                    stream.write_all(&response)?;
                }
            });
            Ok(Self {
                address,
                stop,
                fail,
                seen,
                worker: Some(worker),
            })
        }
        pub fn url(&self) -> String {
            format!("http://{}/v1", self.address)
        }
        pub fn fail_later(&self, on: bool) {
            self.fail.store(on, Ordering::SeqCst);
        }
        pub fn finish(&mut self) -> Result<()> {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
            if let Some(worker) = self.worker.take() {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("Provider panicked"))??;
            }
            Ok(())
        }
    }
    impl Drop for Provider {
        fn drop(&mut self) {
            let _ = self.finish();
        }
    }
}

#[cfg(feature = "vector")]
#[tokio::test]
async fn later_embedding_batch_failure_keeps_old_complete_revision_and_retry_publishes_all_chunks()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut provider = provider::Provider::new()?;
    let mut cfg = config(temp.path(), "openai");
    cfg.openai_api_base = Some(provider.url());
    cfg.openai_api_key = Some("owned-s02-fixture".into());
    cfg.embedding_model = Some("owned-s02-model".into());
    let old = source("original", 70);
    let replacement = source("replacement", 70);
    let sibling = source("sibling", 1);
    let app = create(temp.path(), cfg, &old)?;
    let result = async {
        initial(&app).await?;
        let old_gen = generation(&app)?;
        let old_head = app.state.registry.capture_source_head(PROJECT)?;
        let ps = app
            .ensure_project_runtime(PROJECT)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let held = ps.search.pin_source_generation(PROJECT, old_gen)?;
        let before = groups_under(
            &app,
            old_gen,
            &[("source.rs", &old), ("sibling.rs", &sibling)],
            &held,
        )
        .await?;
        ensure!(
            before["source.rs"].len() > 64,
            "Fixture must cross the real provider batch boundary"
        );
        std::fs::write(temp.path().join("repo/source.rs"), &replacement)?;
        provider.fail_later(true);
        let failed = update(&app).await;
        ensure!(
            failed
                .as_ref()
                .is_err_and(|e| format!("{e:#}").contains("400")),
            "Expected the later provider refusal: {failed:?}"
        );
        {
            let seen = provider
                .seen
                .lock()
                .map_err(|_| anyhow::anyhow!("Transcript poisoned"))?;
            ensure!(
                seen.replacement_ok.len() == 1
                    && seen.replacement_ok[0].len() == 64
                    && !seen.refused.is_empty()
            );
            ensure!(
                seen.refused
                    .iter()
                    .all(|v| !seen.replacement_ok[0].contains(v)),
                "Failure must follow a distinct successful replacement batch"
            );
        }
        ensure!(
            format!("{:?}", app.state.registry.capture_source_head(PROJECT)?)
                == format!("{old_head:?}")
        );
        ensure!(
            groups_under(
                &app,
                old_gen,
                &[("source.rs", &old), ("sibling.rs", &sibling)],
                &held,
            )
            .await?
                == before
        );
        let pending = ps
            .search
            .pin_source_generation(PROJECT, old_gen + 1)
            .err()
            .ok_or_else(|| anyhow::anyhow!("Failed attempt became selectable"))?;
        ensure!(
            pending.to_string().contains("unpublished/incomplete"),
            "{pending:#}"
        );
        // Embeddings succeeded before the refusal; this does not claim partial native upsert.
        provider.fail_later(false);
        update(&app).await?;
        let after = groups(
            &app,
            generation(&app)?,
            &[("source.rs", &replacement), ("sibling.rs", &sibling)],
        )
        .await?;
        ensure!(after["source.rs"].len() > 64 && after["sibling.rs"] == before["sibling.rs"]);
        ensure!(
            groups_under(
                &app,
                old_gen,
                &[("source.rs", &old), ("sibling.rs", &sibling)],
                &held,
            )
            .await?
                == before
        );
        drop(held);
        drop(ps);
        Ok(())
    }
    .await;
    finish_owned_jobs(&app).await;
    drop(app);
    provider.finish()?;
    drop(provider);
    temp.close()?;
    result
}

#[cfg(windows)]
#[tokio::test]
async fn extraction_open_failure_after_preparation_keeps_old_complete_revision_and_retry_converges()
-> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let temp = tempfile::tempdir()?;
    let old = source("original", 8);
    let replacement = source("replacement", 8);
    let sibling = source("sibling", 1);
    let app = create(temp.path(), config(temp.path(), "fts_only"), &old)?;
    let result = async {
        initial(&app).await?;
        let old_gen = generation(&app)?;
        let head = app.state.registry.capture_source_head(PROJECT)?;
        let ps = app
            .ensure_project_runtime(PROJECT)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let held = ps.search.pin_source_generation(PROJECT, old_gen)?;
        let before = groups_under(
            &app,
            old_gen,
            &[("source.rs", &old), ("sibling.rs", &sibling)],
            &held,
        )
        .await?;
        ensure!(before["source.rs"].len() >= 2);
        let path = temp.path().join("repo/source.rs");
        std::fs::write(&path, &replacement)?;
        let permit = app.state.parse_semaphore.clone().acquire_owned().await?;
        let control = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                if ps
                    .search
                    .pin_source_generation(PROJECT, old_gen + 1)
                    .err()
                    .is_some_and(|e| e.to_string().contains("unpublished/incomplete"))
                {
                    break;
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "Update did not reach prepared ingestion"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let denied = std::fs::OpenOptions::new()
                .write(true)
                .share_mode(2 | 4)
                .open(&path)?;
            drop(permit);
            Ok::<_, anyhow::Error>(denied)
        };
        let (failed, denied) = tokio::join!(update(&app), control);
        drop(denied?);
        let detail = failed
            .err()
            .ok_or_else(|| anyhow::anyhow!("Extraction unexpectedly published"))?;
        ensure!(
            format!("{detail:#}").contains("Cannot open source content"),
            "Wrong failure phase: {detail:#}"
        );
        ensure!(
            format!("{:?}", app.state.registry.capture_source_head(PROJECT)?)
                == format!("{head:?}")
        );
        ensure!(
            groups_under(
                &app,
                old_gen,
                &[("source.rs", &old), ("sibling.rs", &sibling)],
                &held,
            )
            .await?
                == before
        );
        ensure!(
            ps.search
                .pin_source_generation(PROJECT, old_gen + 1)
                .err()
                .is_some_and(|e| e.to_string().contains("unpublished/incomplete"))
        );
        update(&app).await?;
        let after = groups(
            &app,
            generation(&app)?,
            &[("source.rs", &replacement), ("sibling.rs", &sibling)],
        )
        .await?;
        ensure!(after["sibling.rs"] == before["sibling.rs"] && after["source.rs"].len() >= 2);
        ensure!(
            groups_under(
                &app,
                old_gen,
                &[("source.rs", &old), ("sibling.rs", &sibling)],
                &held,
            )
            .await?
                == before
        );
        drop(held);
        drop(ps);
        Ok(())
    }
    .await;
    finish_owned_jobs(&app).await;
    drop(app);
    temp.close()?;
    result
}
