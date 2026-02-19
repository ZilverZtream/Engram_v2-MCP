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
    tokio::task::spawn_blocking(move || {
        if let Ok(count) = reg.cleanup_orphaned_jobs()
            && count > 0
        {
            tracing::info!("Aborted {count} orphaned jobs.");
        }
    })
    .await
    .ok();

    // Background cognitive features.
    tokio::spawn(actors::dreamer::run_dreamer(state.clone(), events_rx));
    tokio::spawn(actors::watcher::run_watcher(
        state.clone(),
        state.events_tx.subscribe(),
    ));
    tokio::spawn(actors::gc::run_gc_scheduler(state.clone()));

    tools::run_stdio(state).await?;
    Ok(())
}
