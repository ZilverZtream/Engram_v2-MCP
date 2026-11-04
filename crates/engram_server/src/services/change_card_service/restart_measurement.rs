//! Process-cold card reconstruction, distinct from durable embedding reuse.
use super::*;
use anyhow::{Result, ensure};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const COMMITS: usize = 64;
const DIM: usize = 1536;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Expected {
    oid: String,
    title: String,
    description: String,
    author: String,
    timestamp: u64,
    files: Vec<String>,
    file_count: usize,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Manifest {
    nonce: String,
    endpoint: String,
    expected: Vec<Expected>,
}

fn vector(text: &str) -> Vec<f32> {
    let hash = blake3::hash(text.as_bytes());
    (0..DIM)
        .map(|i| hash.as_bytes()[i % 32] as f32 / 255.0)
        .collect()
}

fn expected_text(row: &Expected) -> String {
    format!(
        "Title: {}\nDescription: {}\nFiles:\n{}",
        row.title,
        row.description,
        row.files.join("\n")
    )
}

struct OfflineProvider {
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    requests: Arc<AtomicU64>,
    texts: Arc<AtomicU64>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
}

impl OfflineProvider {
    fn new() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicU64::new(0));
        let texts = Arc::new(AtomicU64::new(0));
        let (stop, calls, inputs) = (stopping.clone(), requests.clone(), texts.clone());
        let worker = std::thread::spawn(move || -> Result<()> {
            loop {
                let (mut stream, _) = listener.accept()?;
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                let mut bytes = Vec::new();
                let header_end = loop {
                    ensure!(bytes.len() <= 16 * 1024, "oversized HTTP headers");
                    let mut byte = [0u8; 1];
                    stream.read_exact(&mut byte)?;
                    bytes.push(byte[0]);
                    if bytes.ends_with(b"\r\n\r\n") {
                        break bytes.len();
                    }
                };
                let headers = std::str::from_utf8(&bytes)?;
                ensure!(headers.starts_with("POST /v1/embeddings HTTP/1.1\r\n"));
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>())
                    })
                    .transpose()?
                    .ok_or_else(|| anyhow::anyhow!("missing body length"))?;
                ensure!(length <= 1024 * 1024, "oversized embedding request");
                bytes.resize(header_end + length, 0);
                stream.read_exact(&mut bytes[header_end..])?;
                let body: serde_json::Value = serde_json::from_slice(&bytes[header_end..])?;
                ensure!(
                    body["model"] == "nomic-eg22-fixture" || body["model"] == "other-eg22-fixture"
                );
                let batch = body["input"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("expected input array"))?;
                ensure!(!batch.is_empty() && batch.len() <= 32);
                let mut data = Vec::new();
                for (index, text) in batch.iter().enumerate() {
                    let text = text
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("expected text"))?;
                    ensure!(text.len() <= 16 * 1024);
                    data.push(serde_json::json!({"index":index,"embedding":vector(text)}));
                }
                calls.fetch_add(1, Ordering::Relaxed);
                inputs.fetch_add(batch.len() as u64, Ordering::Relaxed);
                let response = serde_json::to_vec(&serde_json::json!({"data":data}))?;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )?;
                stream.write_all(&response)?;
            }
        });
        Ok(Self {
            address,
            stopping,
            requests,
            texts,
            worker: Some(worker),
        })
    }

    fn counts(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.texts.load(Ordering::Relaxed),
        )
    }

    fn finish(&mut self) -> Result<()> {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("offline provider panicked"))??;
        }
        Ok(())
    }
}

impl Drop for OfflineProvider {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn create_history(root: &Path) -> Result<Vec<Expected>> {
    std::fs::create_dir_all(root.join("src"))?;
    let repo = Repository::init(root)?;
    let mut expected = Vec::new();
    let mut parent = None;
    for number in 0..COMMITS {
        let mut index = repo.index()?;
        let changed: Vec<String> = match number {
            17 => {
                std::fs::remove_file(root.join("src/common.rs"))?;
                index.remove_path(Path::new("src/common.rs"))?;
                std::fs::write(root.join("src/renamed.rs"), "pub fn renamed() {}\n")?;
                vec!["src/common.rs".into(), "src/renamed.rs".into()]
            }
            23 => {
                std::fs::write(root.join("asset.min.js"), "generated output\n")?;
                Vec::new()
            }
            45 => {
                std::fs::create_dir(root.join("src/batch"))?;
                let files: Vec<_> = (0..45)
                    .map(|n| format!("src/batch/file{n:02}.rs"))
                    .collect();
                for file in &files {
                    std::fs::write(root.join(file), "pub fn bounded() {}\n")?;
                }
                files
            }
            _ => {
                let file = if number < 17 {
                    "src/common.rs"
                } else {
                    "src/renamed.rs"
                };
                std::fs::write(
                    root.join(file),
                    format!("pub fn revision_{number}() {{}}\n"),
                )?;
                vec![file.into()]
            }
        };
        index.add_all(["*"], git2::IndexAddOption::FORCE, None)?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let title = format!("Feature {number:02}");
        let description = format!("Retain contract {number:02} and its authored qualification.");
        let timestamp = 1_700_000_000 + number as u64;
        let signature = git2::Signature::new(
            "EG22 fixture",
            "fixture@example.invalid",
            &git2::Time::new(timestamp as i64, 0),
        )?;
        let parents = parent.map(|oid| repo.find_commit(oid)).transpose()?;
        let references: Vec<_> = parents.iter().collect();
        let oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            &format!("{title}\n\n{description}"),
            &tree,
            &references,
        )?;
        parent = Some(oid);
        expected.push(Expected {
            oid: oid.to_string(),
            title,
            description,
            author: "EG22 fixture".into(),
            timestamp,
            file_count: changed.len(),
            files: changed.into_iter().take(40).collect(),
        });
    }
    Ok(expected)
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn child(root: &Path, nonce: &str, name: &str, off: bool) -> Result<serde_json::Value> {
    child_with_model(root, nonce, name, off, "nomic-eg22-fixture")
}

fn child_with_model(
    root: &Path,
    nonce: &str,
    name: &str,
    off: bool,
    model: &str,
) -> Result<serde_json::Value> {
    let report = root.join(format!("{name}.json"));
    let log_path = root.join(format!("{name}.log"));
    let log = std::fs::File::create(&log_path)?;
    let module = module_path!().split_once("::").unwrap().1;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            &format!("{module}::card_process_child"),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ENGRAM_EG22_ROOT", root)
        .env("ENGRAM_EG22_NONCE", nonce)
        .env("ENGRAM_EG22_MODEL", model)
        .env("ENGRAM_EG22_REPORT", &report)
        .env("ENGRAM_EG22_OFF", if off { "1" } else { "0" })
        .env("TEMP", root.join("scratch"))
        .env("TMP", root.join("scratch"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x4000);
    }
    let started = Instant::now();
    let mut child = OwnedChild(command.spawn()?);
    loop {
        if let Some(status) = child.0.try_wait()? {
            ensure!(
                status.success(),
                "child failed: {}",
                std::fs::read_to_string(&log_path)?
            );
            break;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(120),
            "owned card child timed out"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    ensure!(std::fs::metadata(&report)?.len() <= 128 * 1024);
    let mut result: serde_json::Value = serde_json::from_slice(&std::fs::read(report)?)?;
    ensure!(result["nonce"] == nonce);
    result["process_wall_seconds"] = serde_json::json!(started.elapsed().as_secs_f64());
    Ok(result)
}

#[cfg(windows)]
fn cpu_ns() -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct Time {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessTimes(
            process: *mut std::ffi::c_void,
            created: *mut Time,
            exited: *mut Time,
            kernel: *mut Time,
            user: *mut Time,
        ) -> i32;
    }
    let (mut created, mut exited, mut kernel, mut user) = (
        Time::default(),
        Time::default(),
        Time::default(),
        Time::default(),
    );
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return None;
    }
    Some(
        (((kernel.high as u64) << 32 | kernel.low as u64)
            + ((user.high as u64) << 32 | user.low as u64))
            * 100,
    )
}
#[cfg(not(windows))]
fn cpu_ns() -> Option<u64> {
    None
}

#[test]
#[ignore = "same-executable child; needs parent-owned fixture and nonce"]
fn card_process_child() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("ENGRAM_EG22_ROOT")
            .ok_or_else(|| anyhow::anyhow!("missing owned root"))?,
    );
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(root.join("manifest.json"))?)?;
    ensure!(std::env::var("ENGRAM_EG22_NONCE")? == manifest.nonce);
    let report = PathBuf::from(std::env::var_os("ENGRAM_EG22_REPORT").unwrap());
    ensure!(report.parent() == Some(root.as_path()));
    let off = std::env::var("ENGRAM_EG22_OFF")? == "1";
    let model = std::env::var("ENGRAM_EG22_MODEL")?;
    ensure!(["nomic-eg22-fixture", "other-eg22-fixture"].contains(&model.as_str()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let observations = runtime.block_on(async {
        let cfg = engram_core::Config { data_dir:root.join("data"),
            embedding_backend:if off {"fts_only"} else {"openai"}.into(),
            embedding_model:Some(model.clone()), openai_api_key:Some("offline-fixture".into()),
            openai_api_base:Some(manifest.endpoint.clone()), llm_backend:"none".into(), ..Default::default() };
        let registry_started = Instant::now();
        std::fs::create_dir_all(cfg.data_dir.join("registry"))?;
        let registry = engram_core::Registry::open(&cfg.data_dir.join("registry/registry.redb"))?;
        let registry_initialization_ns = registry_started.elapsed().as_nanos();
        let initialized = Instant::now();
        let search = engram_index::HybridSearchEngine::new(root.join("search/tantivy"), root.join("search/vectors"), &cfg).await?;
        let initialization_ns = initialized.elapsed().as_nanos();
        let prefixes = EmbedPrefixes::for_model(cfg.embedding_model.as_deref());
        let tip = Oid::from_str(&manifest.expected.last().unwrap().oid)?;
        let mut observations = Vec::new();
        for request in 0..2 {
            let before = search.embedding_cache_stats();
            let cpu_before = cpu_ns();
            let started = Instant::now();
            let (cards, stats) = cards_for_line_observed(&root.join("repo"), tip, &search, prefixes,Some(registry.clone())).await?;
            let wall_ns = started.elapsed().as_nanos();
            let cpu = cpu_before.zip(cpu_ns()).map(|(a,b)|b-a);
            ensure!(cards.len() == manifest.expected.len());
            let mut metadata = blake3::Hasher::new();
            let mut vectors = blake3::Hasher::new();
            for (card, expected) in cards.iter().zip(&manifest.expected) {
                let actual = Expected {oid:card.oid.clone(),title:card.title.clone(),description:card.description.clone(),
                    author:card.author.clone(),timestamp:card.timestamp,files:card.files.clone(),file_count:card.file_count};
                ensure!(serde_json::to_value(&actual)? == serde_json::to_value(expected)?, "independent metadata oracle mismatch: {}", expected.oid);
                metadata.update(&serde_json::to_vec(&actual)?);
                if off { ensure!(card.vector.is_empty()); }
                else {
                    let raw = vector(&format!("{}{}", prefixes.document, expected_text(expected)));
                    // Independently evaluate the public unit-vector contract
                    // in f64, allowing the provider's f32 rounding.
                    let norm = raw.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
                    ensure!(card.vector.len() == DIM && card.vector.iter().zip(&raw)
                        .all(|(actual, original)| ((*actual as f64) - (*original as f64) / norm).abs() < 2e-7),
                        "independent normalized vector oracle mismatch");
                }
                for value in &card.vector { vectors.update(&value.to_le_bytes()); }
            }
            observations.push(serde_json::json!({"request":request,"wall_ns":wall_ns,"cpu_ns":cpu,"card_build":stats,
                "cache_before":before,"cache_after":search.embedding_cache_stats(),"metadata_blake3":metadata.finalize().to_hex().to_string(),
                "vectors_blake3":vectors.finalize().to_hex().to_string()}));
        }
        Ok::<_,anyhow::Error>(serde_json::json!({"initialization_ns":initialization_ns,"registry_initialization_ns":registry_initialization_ns,"requests":observations}))
    })?;
    drop(runtime);
    std::fs::write(
        report,
        serde_json::to_vec(
            &serde_json::json!({"nonce":manifest.nonce,"semantic_off":off,"observations":observations}),
        )?,
    )?;
    Ok(())
}

#[test]
fn restart_rebuilds_cards_but_reuses_persisted_embeddings() -> Result<()> {
    let owned = tempfile::tempdir()?;
    std::fs::create_dir(owned.path().join("scratch"))?;
    let mut provider = OfflineProvider::new()?;
    let manifest = Manifest {
        nonce: uuid::Uuid::new_v4().to_string(),
        endpoint: format!("http://{}/v1", provider.address),
        expected: create_history(&owned.path().join("repo"))?,
    };
    std::fs::write(
        owned.path().join("manifest.json"),
        serde_json::to_vec(&manifest)?,
    )?;
    let a = child(owned.path(), &manifest.nonce, "a-cold-then-warm", false)?;
    ensure!(
        provider.counts() == (2, COMMITS as u64),
        "cold provider work must match genuine cache misses"
    );
    let b = child(
        owned.path(),
        &manifest.nonce,
        "b-restarted-then-warm",
        false,
    )?;
    ensure!(
        provider.counts() == (2, COMMITS as u64),
        "process restart must not repeat HTTP embedding work"
    );
    let c = child(owned.path(), &manifest.nonce, "c-semantic-off", true)?;
    ensure!(
        provider.counts() == (2, COMMITS as u64),
        "semantic-off must not call the provider"
    );
    let requests = |value: &serde_json::Value| {
        value["observations"]["requests"]
            .as_array()
            .unwrap()
            .clone()
    };
    let (ar, br, cr) = (requests(&a), requests(&b), requests(&c));
    for (rows, described) in [(&ar, COMMITS), (&br, 0)] {
        ensure!(rows[0]["card_build"]["described_commits"] == described);
        ensure!(rows[0]["card_build"]["fresh_embedding_texts"] == COMMITS);
        ensure!(rows[1]["card_build"]["described_commits"] == 0);
        ensure!(rows[1]["card_build"]["fresh_embedding_texts"] == 0);
        ensure!(
            rows[1]["cache_after"] == rows[1]["cache_before"],
            "memory hits must not write or query the durable cache"
        );
    }
    ensure!(
        ar[0]["cache_after"]["misses"] == COMMITS
            && ar[0]["cache_after"]["provider_texts"] == COMMITS
    );
    ensure!(
        br[0]["cache_after"]["hits"] == COMMITS
            && br[0]["cache_after"]["misses"] == 0
            && br[0]["cache_after"]["provider_texts"] == 0
            && br[0]["cache_after"]["write_batches"] == 0
    );
    for row in ar.iter().chain(&br).chain(&cr) {
        ensure!(row["metadata_blake3"] == ar[0]["metadata_blake3"]);
    }
    for row in &br {
        ensure!(row["vectors_blake3"] == ar[0]["vectors_blake3"]);
    }
    for row in &cr {
        ensure!(row["card_build"]["described_commits"] == 0 && row["cache_after"].is_null());
        ensure!(
            row["card_build"]["metadata_cache_hits"] == COMMITS
                && row["card_build"]["metadata_cache_writes"] == 0
        );
    }
    ensure!(
        br[0]["card_build"]["metadata_cache_hits"] == COMMITS
            && br[0]["card_build"]["metadata_cache_writes"] == 0
    );
    let d = child_with_model(
        owned.path(),
        &manifest.nonce,
        "d-other-embedding-profile",
        false,
        "other-eg22-fixture",
    )?;
    ensure!(
        provider.counts() == (4, 2 * COMMITS as u64),
        "A new provider profile must obtain its own vectors"
    );
    let dr = requests(&d);
    ensure!(
        dr[0]["card_build"]["described_commits"] == 0
            && dr[0]["card_build"]["metadata_cache_hits"] == COMMITS
    );
    ensure!(dr[0]["cache_after"]["provider_texts"] == COMMITS);
    ensure!(dr[0]["vectors_blake3"] != ar[0]["vectors_blake3"]);
    for row in &dr {
        ensure!(row["metadata_blake3"] == ar[0]["metadata_blake3"]);
    }
    provider.finish()?;
    println!(
        "EG22_RESTART_CARD_COST {}",
        serde_json::json!({"commits":COMMITS,"process_a":a,"process_b":b,"semantic_off":c,"other_profile":d,
        "provider_http_requests":4,"provider_texts":2*COMMITS,"limits":"Owned synthetic Git history; real configured provider/cache path against loopback offline responses. Independent metadata/vector oracles, actual process restarts and metadata reuse across embedding profiles. Process-cold is not disk-cold; not an API billing, real-project performance or whole-app memory claim."})
    );
    owned.close()?;
    Ok(())
}

#[test]
#[ignore = "explicitly named existing repository and frozen tip; read-only Git metadata"]
fn existing_repository_metadata_cost() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("ENGRAM_EG22_REPOSITORY")
            .ok_or_else(|| anyhow::anyhow!("missing explicit repository"))?,
    );
    let tip = Oid::from_str(&std::env::var("ENGRAM_EG22_TIP")?)?;
    let count: usize = std::env::var("ENGRAM_EG22_EXPECTED_COMMITS")?.parse()?;
    ensure!(root.is_absolute() && (1..=4000).contains(&count));
    let owned = tempfile::tempdir()?;
    let path = owned.path().join("registry.redb");
    let mut registry = engram_core::Registry::open(&path)?;
    let mut observations = Vec::new();
    let mut original = None;
    for phase in ["cold_metadata", "warm_metadata", "reopened_registry"] {
        if phase == "reopened_registry" {
            drop(registry);
            registry = engram_core::Registry::open(&path)?;
        }
        let cpu = cpu_ns();
        let started = Instant::now();
        let (line, cards, stats) =
            metadata_cache::read(&root, tip, &Default::default(), Some(&registry))?;
        let wall_ns = started.elapsed().as_nanos();
        let cpu_ns = cpu.zip(cpu_ns()).map(|(a, b)| b - a);
        ensure!(line.len() == count && cards.len() == count);
        let mut digest = blake3::Hasher::new();
        for card in cards {
            digest.update(&serde_json::to_vec(&Expected {
                oid: card.oid,
                title: card.title,
                description: card.description,
                author: card.author,
                timestamp: card.timestamp,
                files: card.files,
                file_count: card.file_count,
            })?);
        }
        let hash = digest.finalize().to_hex().to_string();
        if let Some(expected) = &original {
            ensure!(&hash == expected);
        } else {
            original = Some(hash.clone());
        }
        if phase != "cold_metadata" {
            ensure!(
                stats.described_commits == 0
                    && stats.metadata_cache_hits == count
                    && stats.metadata_cache_writes == 0
            );
        }
        observations.push(serde_json::json!({"phase":phase,"wall_ns":wall_ns,"cpu_ns":cpu_ns,"card_build":stats,"metadata_blake3":hash}));
    }
    println!(
        "EG22_EXISTING_METADATA_COST {}",
        serde_json::json!({"repository":root,"frozen_tip":tip.to_string(),
        "commits":count,"observations":observations,
        "limits":"Read-only Git commit/tree metadata, no worktree/index/source changes or embedding calls. Prototype metadata cache in one new owned temporary Registry, removed after use; warm and reopened-handle reads. Not disk-cold, new-process restart or whole-app performance; independent metadata oracle/process restart covered by owned synthetic fixture."})
    );
    drop(registry);
    owned.close()?;
    Ok(())
}
