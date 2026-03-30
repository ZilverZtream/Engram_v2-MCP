use engram_core::Config;
use engram_server::actors;
use engram_server::state::AppState;
use engram_server::tools;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // IMPORTANT for STDIO MCP servers: do not write to stdout.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cfg = Config::load()?;
    let (state, events_rx) = AppState::new(cfg)?;

    // Cleanup orphaned jobs from previous run.
    let reg = state.registry.clone();
    match tokio::task::spawn_blocking(move || reg.cleanup_orphaned_jobs()).await {
        Ok(Ok(count)) if count > 0 => tracing::info!("Aborted {count} orphaned jobs."),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("Failed to clean up orphaned jobs: {e}"),
        Err(e) => tracing::warn!("Orphaned-job cleanup task panicked: {e}"),
    }

    // CANCEL1: shared shutdown token propagated to all background actors so they
    // exit cooperatively when the process receives a shutdown signal rather than
    // being forcibly killed mid-cycle.
    let shutdown = CancellationToken::new();

    // Background cognitive features.
    tokio::spawn(actors::dreamer::run_dreamer(state.clone(), events_rx, shutdown.clone()));
    tokio::spawn(actors::watcher::run_watcher(
        state.clone(),
        state.events_tx.subscribe(),
        shutdown.clone(),
    ));
    tokio::spawn(actors::gc::run_gc_scheduler(state.clone(), shutdown.clone()));
    tokio::spawn(actors::immune::run_immune_actor(state.clone(), shutdown.clone()));

    // Data integrity sentinel (periodic cross-store consistency checks).
    tokio::spawn(engram_server::services::integrity_service::run_integrity_checker(state.clone(), shutdown.clone()));

    tools::run_stdio(state).await?;
    Ok(())
}
