use engram_core::Config;
use engram_server::actors;
use engram_server::logging;
use engram_server::multi_client;
use engram_server::state::AppState;
use engram_server::tools;
use tokio_util::sync::CancellationToken;

/// Which role this process was launched as. Clients are what MCP hosts
/// spawn; the daemon + launcher roles are internal (spawned by clients).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessRole {
    /// Normal MCP server process (stdio session with the host).
    Client,
    /// Detached shared primary: serves IPC sessions only, no stdio.
    Daemon,
    /// One-shot intermediary that spawns the daemon detached and exits,
    /// breaking the parent-process chain so host-side tree-kills of the
    /// client cannot reach the daemon.
    DaemonLauncher,
}

fn parse_role() -> ProcessRole {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--daemon" => return ProcessRole::Daemon,
            "--daemon-launcher" => return ProcessRole::DaemonLauncher,
            _ => {}
        }
    }
    ProcessRole::Client
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let role = parse_role();

    // Config first, tracing second: the log-file destination lives in the
    // config. Anything Config::load() itself traces is dropped — acceptable,
    // its hard errors still propagate via the Result and reach stderr.
    let mut cfg = Config::load()?;

    // IMPORTANT for STDIO MCP servers: never write logs to stdout. Tracing
    // goes to stderr plus (when configured) a shared-append log file — the
    // file MUST be opened in-process; a shell `2>>` redirect takes an
    // exclusive handle on Windows and kills every second MCP session.
    let log_path = {
        let resolved = logging::resolve_log_path(cfg.log_file.as_deref());
        if resolved.is_none() && role == ProcessRole::Daemon {
            // A detached daemon has no console: without a file its logs
            // vanish entirely. Default to a file next to the data dir.
            Some(cfg.data_dir.join("engram-daemon.log"))
        } else {
            resolved
        }
    };
    logging::init_tracing(log_path.as_deref());

    // P0-2: the default embedding backend is a non-semantic trigram projection.
    // Say so once at startup instead of letting operators assume real
    // semantic search is active.
    if matches!(cfg.embedding_backend.as_str(), "" | "local" | "candle") {
        tracing::warn!(
            backend = %if cfg.embedding_backend.is_empty() { "(default)" } else { cfg.embedding_backend.as_str() },
            "embedding backend is the non-semantic trigram-projection stub — \
             vector search will reflect character overlap, not meaning. Set \
             embedding_backend=ollama|openai in engram_mcp.yaml for true semantic search."
        );
    }

    // Multi-client mode can be forced on/off via env var or CLI flag,
    // overriding the YAML. This lets a user flip modes without
    // editing config — useful during the v0.7 rollout when the
    // default is still `false`.
    apply_multi_client_overrides(&mut cfg);

    match role {
        ProcessRole::DaemonLauncher => {
            return multi_client::run_daemon_launcher()
                .inspect_err(multi_client::log_fatal_daemon_error);
        }
        // The daemon is detached with a null stderr, so returning this Err
        // would print the cause into a discarded handle. Trace it first so
        // the failure reaches `log_file` — otherwise a daemon that dies
        // before binding its IPC endpoint is completely silent.
        ProcessRole::Daemon => {
            return multi_client::run_daemon(cfg)
                .await
                .inspect_err(multi_client::log_fatal_daemon_error);
        }
        ProcessRole::Client => {}
    }

    if cfg.multi_client {
        // Delegate to the auto-daemon dispatcher. It handles
        // primary-vs-proxy role selection and owns the whole
        // lifecycle (including background actors in primary mode).
        return multi_client::dispatch(cfg).await;
    }

    // Legacy single-client path — byte-identical to the previous
    // main() so existing deployments that have `multi_client: false`
    // (or haven't set it at all) behave exactly as before.
    let (state, events_rx) = AppState::new(cfg)?;

    let reg = state.registry.clone();
    match tokio::task::spawn_blocking(move || reg.cleanup_orphaned_jobs()).await {
        Ok(Ok(count)) if count > 0 => tracing::info!("Aborted {count} orphaned jobs."),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("Failed to clean up orphaned jobs: {e}"),
        Err(e) => tracing::warn!("Orphaned-job cleanup task panicked: {e}"),
    }

    let shutdown = CancellationToken::new();

    tokio::spawn(actors::dreamer::run_dreamer(
        state.clone(),
        events_rx,
        shutdown.clone(),
    ));
    tokio::spawn(actors::watcher::run_watcher(
        state.clone(),
        state.events_tx.subscribe(),
        shutdown.clone(),
    ));
    tokio::spawn(actors::gc::run_gc_scheduler(
        state.clone(),
        shutdown.clone(),
    ));
    tokio::spawn(actors::warmup::run_warmup(state.clone(), shutdown.clone()));
    tokio::spawn(actors::immune::run_immune_actor(
        state.clone(),
        shutdown.clone(),
    ));

    tokio::spawn(
        engram_server::services::integrity_service::run_integrity_checker(
            state.clone(),
            shutdown.clone(),
        ),
    );

    tools::run_stdio(state).await?;
    Ok(())
}

/// Apply `--multi-client` / `--no-multi-client` CLI flags and the
/// `ENGRAM_MULTI_CLIENT` env var on top of whatever the YAML
/// configured. Precedence (highest wins): CLI > env > YAML > default.
fn apply_multi_client_overrides(cfg: &mut Config) {
    // Env var is the middle priority.
    if let Ok(v) = std::env::var("ENGRAM_MULTI_CLIENT") {
        match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => cfg.multi_client = true,
            "0" | "false" | "no" | "off" => cfg.multi_client = false,
            _ => tracing::warn!(
                value = %v,
                "ENGRAM_MULTI_CLIENT set to unrecognised value — ignoring"
            ),
        }
    }
    // CLI flag is the highest priority. Scan argv for the simple
    // tokens `--multi-client` and `--no-multi-client` — no clap
    // dependency because we only need these two switches.
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--multi-client" => cfg.multi_client = true,
            "--no-multi-client" => cfg.multi_client = false,
            "--no-daemon" => cfg.multi_client_daemon = false,
            _ => {}
        }
    }
    if let Ok(v) = std::env::var("ENGRAM_MULTI_CLIENT_IDLE_SECS") {
        if let Ok(secs) = v.trim().parse::<u64>() {
            cfg.multi_client_idle_timeout_secs = secs.max(10);
        }
    }
    if let Ok(v) = std::env::var("ENGRAM_MULTI_CLIENT_DAEMON") {
        match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => cfg.multi_client_daemon = true,
            "0" | "false" | "no" | "off" => cfg.multi_client_daemon = false,
            _ => tracing::warn!(
                value = %v,
                "ENGRAM_MULTI_CLIENT_DAEMON set to unrecognised value — ignoring"
            ),
        }
    }
}
