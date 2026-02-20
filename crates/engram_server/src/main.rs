use engram_core::Config;
use engram_server::actors;
use engram_server::state::AppState;
use engram_server::tools;

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

    // Background cognitive features.
    tokio::spawn(actors::dreamer::run_dreamer(state.clone(), events_rx));
    tokio::spawn(actors::watcher::run_watcher(
        state.clone(),
        state.events_tx.subscribe(),
    ));
    tokio::spawn(actors::gc::run_gc_scheduler(state.clone()));
    tokio::spawn(actors::immune::run_immune_actor(state.clone()));

    tools::run_stdio(state).await?;
    Ok(())
}
