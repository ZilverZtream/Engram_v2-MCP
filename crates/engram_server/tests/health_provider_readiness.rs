//! Configuration is not proof that an embedding service can answer requests.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use std::{net::TcpListener, path::PathBuf};

struct Fixture {
    server: Engram,
    listener: TcpListener,
    owned: tempfile::TempDir,
}

impl Fixture {
    fn new(backend: &str) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("hr-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir(&root)?;
        // The endpoint has not served any response. A configured URL alone
        // cannot establish remote availability, and health must not invoke it.
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: backend.into(),
            ollama_url: Some(format!("http://{}", listener.local_addr()?)),
            ollama_embed_dim: Some(4),
            llm_backend: "none".into(),
            max_concurrent_jobs: 1,
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id: "owned-health-readiness".into(),
            project_name: "Owned health interpretation".into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
        Ok(Self {
            server: Engram::new(state),
            listener,
            owned,
        })
    }

    async fn health(&self) -> Result<String> {
        let result = self
            .server
            .handle_project_health(engram_server::models::ProjectIdRequest {
                project_id: "owned-health-readiness".into(),
            })
            .await?;
        Ok(result.content[0].as_text().unwrap().text.clone())
    }

    fn close(self) -> Result<()> {
        drop(self.server);
        let unused = matches!(self.listener.accept(), Err(ref error)
            if error.kind() == std::io::ErrorKind::WouldBlock);
        drop(self.listener);
        self.owned.close()?;
        ensure!(unused, "Health contacted the configured embedding endpoint");
        Ok(())
    }
}

#[cfg(feature = "vector")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_semantic_service_is_not_reported_ready_without_operational_evidence()
-> Result<()> {
    let fixture = Fixture::new("ollama")?;
    let result = async {
        let health = fixture.health().await?;
        ensure!(health.contains("semantic_search: true"), "{health}");
        ensure!(!health.contains("retrieval_readiness: READY"), "{health}");
        ensure!(
            health.contains("semantic_provider_availability: UNVERIFIED_NOT_PROBED"),
            "{health}"
        );
        ensure!(
            health.contains("Actual retrieval replies report provider outcomes"),
            "{health}"
        );
        ensure!(health.contains("active_generation:") && health.contains("tantivy_docs_total:"));
        Ok(())
    }
    .await;
    let cleanup = fixture.close();
    result.and(cleanup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_semantic_retrieval_retains_its_integrity_and_degraded_scope() -> Result<()> {
    let fixture = Fixture::new("fts_only")?;
    let result = async {
        let health = fixture.health().await?;
        ensure!(health.contains("semantic_search: off"), "{health}");
        ensure!(
            health.contains("semantic_provider_availability: DISABLED"),
            "{health}"
        );
        ensure!(health.contains("retrieval_readiness: DEGRADED"), "{health}");
        ensure!(
            health.contains("Health: OK covers source-index integrity only"),
            "{health}"
        );
        ensure!(health.contains("active_generation:") && health.contains("tantivy_docs_total:"));
        Ok(())
    }
    .await;
    let cleanup = fixture.close();
    result.and(cleanup)
}
