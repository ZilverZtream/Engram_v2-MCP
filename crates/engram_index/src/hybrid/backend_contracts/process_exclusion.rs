//! Cross-process engine mutation and collection admission. Registry HEAD
//! publication is covered separately by the server publication contracts.
use super::*;
use std::fs;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    nonce: String,
    action: String,
    outcome: String,
    canonical_tantivy: PathBuf,
    operations: Vec<String>,
    lexical: Vec<(String, String)>,
    vectors: Vec<(String, String)>,
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        // Child teardown must finish before the owning fixture can be removed.
        let _ = self.0.wait();
    }
}

impl OwnedChild {
    fn wait(&mut self) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = self.0.try_wait()? {
                anyhow::ensure!(status.success(), "child exited with {status}");
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "source exclusion child timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn config() -> engram_core::Config {
    engram_core::Config {
        embedding_backend: "local".into(),
        ..Default::default()
    }
}

fn rows(hits: Vec<HybridHit>) -> Vec<(String, String)> {
    let mut rows: Vec<_> = hits.into_iter().map(|hit| (hit.doc_id, hit.pk)).collect();
    rows.sort();
    rows
}

fn run_child(
    root: &Path,
    evidence: &Path,
    nonce: &str,
    action: &str,
    sequence: usize,
) -> anyhow::Result<Report> {
    let report_path = evidence.join(format!("s17-child-{sequence}.json"));
    let log_path = evidence.join(format!("s17-child-{sequence}.log"));
    let log = fs::File::create(&log_path)?;
    let module = module_path!();
    let module = module.split_once("::").map_or(module, |(_, rest)| rest);
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            &format!("{module}::catalog_process_child"),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ENGRAM_S17_ROOT", root)
        .env("ENGRAM_S17_EVIDENCE", evidence)
        .env("ENGRAM_S17_NONCE", nonce)
        .env("ENGRAM_S17_ACTION", action)
        .env("ENGRAM_S17_REPORT", &report_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00004000);
    }
    let mut child = OwnedChild(command.spawn()?);
    child
        .wait()
        .map_err(|error| error.context(format!("child diagnostics: {}", log_path.display())))?;
    // Zero matching libtest cases cannot pass without the authenticated report.
    anyhow::ensure!(
        fs::metadata(&report_path)?.len() <= 64 * 1024,
        "oversized child report"
    );
    let report: Report = serde_json::from_slice(&fs::read(report_path)?)?;
    anyhow::ensure!(
        report.nonce == nonce && report.action == action,
        "wrong child report"
    );
    anyhow::ensure!(
        report.canonical_tantivy == root.join("tantivy").canonicalize()?,
        "wrong source catalog"
    );
    Ok(report)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "same-executable child; requires parent-owned fixture and nonce"]
async fn catalog_process_child() -> anyhow::Result<()> {
    let root = PathBuf::from(
        std::env::var_os("ENGRAM_S17_ROOT").ok_or_else(|| anyhow::anyhow!("missing root"))?,
    );
    let evidence = PathBuf::from(
        std::env::var_os("ENGRAM_S17_EVIDENCE")
            .ok_or_else(|| anyhow::anyhow!("missing evidence"))?,
    );
    let nonce = std::env::var("ENGRAM_S17_NONCE")?;
    let action = std::env::var("ENGRAM_S17_ACTION")?;
    let report_path = PathBuf::from(
        std::env::var_os("ENGRAM_S17_REPORT").ok_or_else(|| anyhow::anyhow!("missing report"))?,
    );
    anyhow::ensure!(action == "blocked" || action == "seal-gc");
    anyhow::ensure!(!nonce.is_empty() && nonce.len() <= 128);
    let canonical_root = root.canonicalize()?;
    anyhow::ensure!(
        canonical_root == evidence.canonicalize()?,
        "root is outside owned fixture"
    );
    anyhow::ensure!(
        report_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing report parent"))?
            .canonicalize()?
            == canonical_root
    );
    anyhow::ensure!(matches!(
        report_path.file_name().and_then(|p| p.to_str()),
        Some("s17-child-0.json" | "s17-child-1.json" | "s17-child-2.json" | "s17-child-3.json")
    ));
    anyhow::ensure!(
        fs::read_to_string(evidence.join(".engram-s17-owned"))? == nonce,
        "missing fixture ownership"
    );
    let canonical_tantivy = root.join("tantivy").canonicalize()?;
    let report = match engine(&root, &config()).await {
        Err(error) => {
            let message = format!("{error:#}");
            anyhow::ensure!(
                message.contains("source index is already open in another process"),
                "refusal happened outside source catalog admission: {message}"
            );
            Report {
                nonce,
                action,
                outcome: "catalog-busy".into(),
                canonical_tantivy,
                operations: vec![],
                lexical: vec![],
                vectors: vec![],
            }
        }
        Ok(index) => {
            let cancel = CancellationToken::new();
            let mut operations = Vec::new();
            // Even an unexpectedly admitted blocked child exercises the actual
            // mutation path; it can only address this parent's owned fixture.
            index
                .prepare_source_snapshot("project", 2, 3, &[RelPath::new("a.rs")], &[], &cancel)
                .await?;
            operations.push("prepare".into());
            index
                .index_docs(
                    "project",
                    &[document("a.rs", 3, "next-a", "next source")],
                    &cancel,
                )
                .await?;
            operations.push("index".into());
            index.seal_source_snapshot("project", 3)?;
            operations.push("seal".into());
            index.purge_old_generations("project", 3).await?;
            operations.push("purge".into());
            let lexical = rows(index.lexical_search(&query(3, "source"))?);
            let vectors = rows(index.vector_search(&query(3, "source"), &cancel).await?);
            operations.push("read".into());
            anyhow::ensure!(index.count_docs("project")? == 2);
            anyhow::ensure!(index.count_vectors("project").await? == 2);
            drop(index);
            Report {
                nonce,
                action,
                outcome: "sealed-collected".into(),
                canonical_tantivy,
                operations,
                lexical,
                vectors,
            }
        }
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report_path)?;
    file.write_all(&serde_json::to_vec(&report)?)?;
    file.sync_all()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separate_process_cannot_mutate_or_collect_while_engine_or_source_view_is_owned()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().to_path_buf();
    let result = async {
        fs::create_dir(root.join("alias"))?;
        let alias = root.join("alias").join("..");
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        fs::write(root.join(".engram-s17-owned"), &nonce)?;
        let cancel = CancellationToken::new();
        let index = engine(&root, &config()).await?;
        index
            .index_docs(
                "project",
                &[
                    document("a.rs", 1, "old-a", "old source"),
                    document("b.rs", 1, "stable-b", "stable source"),
                ],
                &cancel,
            )
            .await?;
        let old_reader = index.pin_source_generation("project", 1)?;
        let original = rows(index.lexical_search(&query(1, "source"))?);
        let stable_pk = original
            .iter()
            .find(|(id, _)| id == "stable-b")
            .ok_or_else(|| anyhow::anyhow!("missing stable document"))?
            .1
            .clone();
        index
            .prepare_source_snapshot("project", 1, 2, &[RelPath::new("a.rs")], &[], &cancel)
            .await?;
        index
            .index_docs(
                "project",
                &[document("a.rs", 2, "new-a", "new source")],
                &cancel,
            )
            .await?;
        index.seal_source_snapshot("project", 2)?;
        let current = rows(index.lexical_search(&query(2, "source"))?);
        for (sequence, candidate) in [&root, &alias].into_iter().enumerate() {
            let report = run_child(candidate, &root, &nonce, "blocked", sequence)?;
            anyhow::ensure!(
                report.outcome == "catalog-busy" && report.operations.is_empty(),
                "{report:?}"
            );
            anyhow::ensure!(rows(index.lexical_search(&query(2, "source"))?) == current);
            anyhow::ensure!(
                rows(index.vector_search(&query(2, "source"), &cancel).await?) == current
            );
            anyhow::ensure!(rows(index.lexical_search(&query(1, "source"))?) == original);
        }
        drop(index);
        let report = run_child(&alias, &root, &nonce, "blocked", 2)?;
        anyhow::ensure!(
            report.outcome == "catalog-busy" && report.operations.is_empty(),
            "{report:?}"
        );
        drop(old_reader);
        let report = run_child(&alias, &root, &nonce, "seal-gc", 3)?;
        anyhow::ensure!(report.outcome == "sealed-collected", "{report:?}");
        anyhow::ensure!(
            report.operations == ["prepare", "index", "seal", "purge", "read"],
            "{report:?}"
        );
        anyhow::ensure!(
            report
                .lexical
                .iter()
                .map(|r| r.0.as_str())
                .collect::<Vec<_>>()
                == ["next-a", "stable-b"],
            "{report:?}"
        );
        anyhow::ensure!(report.vectors == report.lexical, "{report:?}");
        anyhow::ensure!(
            report
                .lexical
                .iter()
                .find(|r| r.0 == "stable-b")
                .map(|r| &r.1)
                == Some(&stable_pk)
        );
        let reopened = engine(&root, &config()).await?;
        anyhow::ensure!(rows(reopened.lexical_search(&query(3, "source"))?) == report.lexical);
        anyhow::ensure!(
            rows(reopened.vector_search(&query(3, "source"), &cancel).await?) == report.vectors
        );
        anyhow::ensure!(
            reopened.count_docs("project")? == 2 && reopened.count_vectors("project").await? == 2
        );
        anyhow::ensure!(reopened.pin_source_generation("project", 1).is_err());
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}
