//! Multi-client auto-daemon — see `docs/MULTI_CLIENT_AUTO_DAEMON.md`.
//!
//! This module owns process-role selection at startup. When
//! `cfg.multi_client == true`, `main.rs` delegates to [`dispatch`]
//! instead of opening storage directly.
//!
//! Default architecture (v0.8, `multi_client_daemon: true`): every MCP
//! host session spawns a thin CLIENT that proxies its stdio to one
//! shared DETACHED daemon over local IPC (Windows named pipe / Unix
//! domain socket). The daemon owns all storage, survives any single
//! session closing, and idle-exits when the last client disconnects.
//! Fallback (`multi_client_daemon: false` or daemon spawn failure):
//! lock election — the winning process becomes an in-process primary
//! serving both its own stdio and the IPC listener; losers proxy.
//!
//! Goals:
//! - Zero user-facing config change: same MCP config, same binary.
//! - Never block two clients from running simultaneously against the
//!   same `data_dir`.
//! - Closing the first session must not sever the other sessions.
//! - No new networking — IPC is local only.
//!
//! Non-goals: networked Engram, HA failover, transparent cross-primary
//! retry. See the design doc for rationale.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_core::Config;
use fs2::FileExt as _;

/// Wire-protocol version. Written into the lock file so that a proxy
/// can detect a primary running a mismatched build BEFORE forwarding
/// any bytes. Bump when the IPC framing / MCP dialect changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// The filename engram uses under `data_dir` for the advisory /
/// (on Windows: mandatory) lock file that decides primary vs proxy.
/// This file's CONTENTS are never read — it exists only so `fs2`'s
/// `try_lock_exclusive` has something to take an OS-level lock on.
pub const LOCK_FILENAME: &str = ".engram.lock";

/// The filename engram uses under `data_dir` for the primary's
/// metadata (PID, socket path, protocol version). A separate file
/// from the lock file because on Windows the locked file is
/// mandatorily locked — readers (proxies) cannot open it while a
/// primary holds it. Keeping the metadata in a sibling file means
/// proxies can read freely without racing the lock.
pub const METADATA_FILENAME: &str = ".engram.primary";

/// The filename engram uses under `data_dir` for the Unix domain
/// socket that proxies connect to. On Windows we fall back to a
/// named pipe with a deterministic name (see `derive_pipe_name`).
pub const SOCKET_FILENAME: &str = ".engram.sock";

/// Hard cap on concurrent socket sessions a primary will accept.
/// Protects against runaway clients / DoS from a misbehaving agent.
pub const MAX_CONCURRENT_SESSIONS: usize = 32;

// ─── Lock file ──────────────────────────────────────────────────────────────

/// Metadata written into the advisory lock file by a primary. Proxies
/// read this blob to discover the IPC endpoint and verify protocol
/// compatibility before exchanging any bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockMetadata {
    pub pid: u32,
    pub started_at_ms: u64,
    pub protocol_version: u32,
    pub socket_path: String,
    /// Crate version of the primary's binary. Informational: proxies WARN
    /// (never refuse) on mismatch so a stale daemon left over from before a
    /// redeploy is visible in the logs. Empty when the primary predates this
    /// field.
    pub version: String,
}

impl LockMetadata {
    fn serialise(&self) -> String {
        format!(
            "pid={}\nstarted_at_ms={}\nprotocol_version={}\nsocket={}\nversion={}\n",
            self.pid, self.started_at_ms, self.protocol_version, self.socket_path, self.version
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut pid = None;
        let mut started_at_ms = None;
        let mut protocol_version = None;
        let mut socket_path = None;
        let mut version = String::new();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "pid" => pid = v.trim().parse().ok(),
                "started_at_ms" => started_at_ms = v.trim().parse().ok(),
                "protocol_version" => protocol_version = v.trim().parse().ok(),
                "socket" => socket_path = Some(v.trim().to_string()),
                "version" => version = v.trim().to_string(),
                _ => {}
            }
        }
        Some(LockMetadata {
            pid: pid?,
            started_at_ms: started_at_ms?,
            protocol_version: protocol_version?,
            socket_path: socket_path?,
            version,
        })
    }
}

/// RAII wrapper around a held exclusive OS-level file lock. The lock
/// is released when this handle is dropped (or when the process
/// exits — the OS reclaims advisory + mandatory locks on exit). The
/// primary holds this for its entire lifetime.
///
/// Important: the file the lock is held on is **content-free**. It
/// exists solely to give `fs2::FileExt::try_lock_exclusive` a handle
/// to lock. The primary's metadata (PID, socket path, protocol
/// version) lives in a **separate** file (`METADATA_FILENAME`) so
/// proxies can read it freely. This matters on Windows where a held
/// lock makes the file unreadable by other processes.
pub struct LockHandle {
    // Kept alive for the lifetime of the primary — dropping the File
    // releases the lock. `_file` rather than `file` because we never
    // read or write through the handle after locking.
    _file: File,
    lock_path: PathBuf,
}

impl LockHandle {
    /// Attempt to acquire the lock non-blockingly. Returns:
    /// - `Ok(Some(handle))` if we got it (become primary).
    /// - `Ok(None)` if another process holds it (become proxy).
    /// - `Err(_)` for unexpected IO errors (permission denied, etc).
    pub fn try_acquire(path: &Path) -> anyhow::Result<Option<Self>> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self {
                _file: file,
                lock_path: path.to_path_buf(),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => {
                // `fs2` surfaces "would block" as a synthetic error
                // whose kind isn't always WouldBlock across platforms.
                // Fall back to a substring check to avoid false
                // negatives.
                let msg = e.to_string().to_ascii_lowercase();
                if msg.contains("would block")
                    || msg.contains("resource temporarily")
                    || msg.contains("locked")
                {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.lock_path
    }
}

/// Write the primary's metadata to the sibling `.engram.primary`
/// file. The lock file and metadata file are distinct so proxies
/// can read metadata without fighting the OS lock.
pub fn write_primary_metadata(metadata_path: &Path, meta: &LockMetadata) -> anyhow::Result<()> {
    // Atomic rename pattern: write to `<path>.tmp`, rename onto
    // target. Never leaves a half-written metadata file behind even
    // if the process is killed mid-write.
    let tmp_path = metadata_path.with_extension("tmp");
    {
        let mut tmp = File::create(&tmp_path)?;
        tmp.write_all(meta.serialise().as_bytes())?;
        tmp.sync_all()?;
    }
    std::fs::rename(&tmp_path, metadata_path)?;
    Ok(())
}

/// Read + parse the primary's metadata file. Proxies call this to
/// discover the socket path and verify protocol compatibility
/// before sending a byte.
pub fn read_primary_metadata(metadata_path: &Path) -> anyhow::Result<LockMetadata> {
    let text = std::fs::read_to_string(metadata_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to read primary metadata file {}: {e}",
            metadata_path.display()
        )
    })?;
    LockMetadata::parse(&text).ok_or_else(|| {
        anyhow::anyhow!(
            "primary metadata file {} is malformed or empty",
            metadata_path.display()
        )
    })
}

// ─── Path derivation ────────────────────────────────────────────────────────

/// Derive the advisory-lock file path for a given data directory.
pub fn derive_lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOCK_FILENAME)
}

/// Derive the primary-metadata file path for a given data directory.
pub fn derive_metadata_path(data_dir: &Path) -> PathBuf {
    data_dir.join(METADATA_FILENAME)
}

/// Derive the IPC socket path for a given data directory.
///
/// On Unix, the canonical location is `<data_dir>/.engram.sock`. If
/// that path would exceed the platform's `sun_path` limit (108 bytes
/// on Linux, 104 on macOS — we use a conservative 104), we fall back
/// to `/tmp/engram-<hash>.sock` where `<hash>` is a stable
/// blake3-derived tag of the data directory.
///
/// On Windows, a named pipe with a deterministic name is used
/// instead; see `derive_pipe_name`.
pub fn derive_socket_path(data_dir: &Path) -> PathBuf {
    let canonical = data_dir.join(SOCKET_FILENAME);
    #[cfg(unix)]
    {
        if canonical.as_os_str().len() <= 104 {
            return canonical;
        }
        // Fallback to a short path under /tmp. Derive the tag from the
        // data_dir so two projects don't collide.
        let tag = short_tag(data_dir);
        PathBuf::from(format!("/tmp/engram-{tag}.sock"))
    }
    #[cfg(not(unix))]
    {
        canonical
    }
}

/// Stable 16-hex-char tag derived from a filesystem path. Used for
/// short socket / pipe names where the full path would be too long or
/// too noisy.
pub fn short_tag(path: &Path) -> String {
    let s = path.to_string_lossy();
    let hash = blake3::hash(s.as_bytes()).to_hex();
    hash[..16].to_string()
}

/// Windows named-pipe name for a data directory.
#[cfg(windows)]
pub fn derive_pipe_name(data_dir: &Path) -> String {
    format!(r"\\.\pipe\engram-{}", short_tag(data_dir))
}

// ─── Dispatch ───────────────────────────────────────────────────────────────

/// Role the current process has in multi-client mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Primary,
    Proxy,
}

/// Resolve the IPC endpoint path for a config: explicit override wins,
/// otherwise derive from `data_dir` (named pipe on Windows, Unix socket
/// elsewhere).
pub fn resolve_socket_path(cfg: &Config) -> PathBuf {
    match &cfg.multi_client_socket_path {
        Some(custom) => PathBuf::from(custom),
        None => {
            // On Windows the IPC channel is a named pipe, not a
            // filesystem socket. Carry its name through the same
            // socket_path field (metadata + listener + proxy all read it).
            #[cfg(windows)]
            {
                PathBuf::from(derive_pipe_name(&cfg.data_dir))
            }
            #[cfg(not(windows))]
            {
                derive_socket_path(&cfg.data_dir)
            }
        }
    }
}

/// Top-level multi-client entry point. Called from `main.rs` when
/// `cfg.multi_client` is true. Returns when the current process should
/// exit (either gracefully or because the MCP session closed).
///
/// Default flow (`multi_client_daemon: true`): this process NEVER opens
/// storage. It connects to the shared detached daemon (spawning one if
/// none is listening) and proxies its stdio over local IPC. The daemon
/// outlives any individual MCP session, so closing the first Claude Code
/// window no longer severs every other window's Engram connection.
///
/// Fallback flow (`multi_client_daemon: false`, or the daemon binary
/// cannot be spawned): the pre-daemon behavior — lock election, winner
/// becomes an in-process primary, losers proxy to it.
pub async fn dispatch(cfg: Config) -> anyhow::Result<()> {
    let data_dir = cfg.data_dir.clone();
    // `data_dir` must exist before we try to put a lock file in it.
    std::fs::create_dir_all(&data_dir)?;
    let socket_path = resolve_socket_path(&cfg);
    let metadata_path = derive_metadata_path(&data_dir);

    if cfg.multi_client_daemon {
        match run_client(&cfg, &metadata_path, &socket_path).await {
            ClientOutcome::Served(result) => return result,
            ClientOutcome::SpawnUnavailable(e) => {
                tracing::warn!(
                    "could not spawn detached daemon ({e:#}); \
                     falling back to in-process primary election"
                );
            }
        }
    }

    let lock_path = derive_lock_path(&data_dir);
    match LockHandle::try_acquire(&lock_path)? {
        Some(handle) => run_primary(cfg, handle, metadata_path, socket_path).await,
        None => run_proxy(metadata_path, socket_path).await,
    }
}

/// What happened when this process tried to act as a thin client of the
/// shared daemon.
enum ClientOutcome {
    /// A session ran (successfully or not) — the process is done.
    Served(anyhow::Result<()>),
    /// The daemon could not even be spawned — caller should fall back to
    /// in-process primary election so the user still gets a working server.
    SpawnUnavailable(anyhow::Error),
}

/// Warn (once) when the daemon we connected to was built from a different
/// crate version than this client — a stale daemon lingering after a
/// redeploy keeps serving old behavior until it idle-exits or is killed.
fn warn_on_version_mismatch(meta: &LockMetadata) {
    use std::sync::atomic::AtomicBool;
    static WARNED: AtomicBool = AtomicBool::new(false);
    let mine = env!("CARGO_PKG_VERSION");
    if !meta.version.is_empty() && meta.version != mine && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            daemon_pid = meta.pid,
            daemon_version = %meta.version,
            client_version = %mine,
            "connected to a daemon built from a different engram version — \
             kill the daemon process (or wait for its idle exit) to pick up the new binary"
        );
    }
}

/// Client mode: connect to the shared daemon's IPC endpoint, spawning the
/// daemon (detached) if nothing is listening, then forward stdio bytes.
async fn run_client(cfg: &Config, metadata_path: &Path, socket_path: &Path) -> ClientOutcome {
    let timeout = std::time::Duration::from_secs(cfg.multi_client_connect_timeout_secs.max(5));
    let deadline = std::time::Instant::now() + timeout;
    let mut spawned = false;
    loop {
        // Prefer the endpoint advertised by the live primary's metadata —
        // it covers custom socket paths and is written before the slow
        // AppState init, so it appears early in the daemon's startup.
        let effective = match read_primary_metadata(metadata_path) {
            Ok(meta) => {
                warn_on_version_mismatch(&meta);
                if meta.socket_path.is_empty() {
                    socket_path.to_path_buf()
                } else {
                    PathBuf::from(&meta.socket_path)
                }
            }
            Err(_) => socket_path.to_path_buf(),
        };

        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ClientOptions;
            let pipe_name = effective.to_string_lossy().into_owned();
            match ClientOptions::new().open(&pipe_name) {
                Ok(client) => {
                    tracing::info!(
                        pipe = %pipe_name,
                        "engram_server: connected to shared daemon"
                    );
                    return ClientOutcome::Served(forward_stdio_to_pipe(client).await);
                }
                // ERROR_PIPE_BUSY (231): a listener exists but all pipe
                // instances are momentarily taken — retry, never spawn.
                Err(e) if e.raw_os_error() == Some(231) => {}
                // ERROR_FILE_NOT_FOUND (2): nobody is listening (no daemon
                // yet, or one is mid-startup before the listener binds).
                Err(e) if e.raw_os_error() == Some(2) => {
                    if !spawned {
                        if let Err(spawn_err) = spawn_daemon_detached() {
                            return ClientOutcome::SpawnUnavailable(spawn_err);
                        }
                        spawned = true;
                    }
                }
                Err(e) => return ClientOutcome::Served(Err(e.into())),
            }
        }
        #[cfg(unix)]
        {
            match tokio::net::UnixStream::connect(&effective).await {
                Ok(sock) => {
                    tracing::info!(
                        socket = %effective.display(),
                        "engram_server: connected to shared daemon"
                    );
                    return ClientOutcome::Served(forward_stdio_to_socket(sock).await);
                }
                Err(e) if is_missing_or_refused(&e) => {
                    // A refused connect on a leftover socket file from a
                    // crashed daemon blocks rebinding — clear it before the
                    // daemon we spawn tries to listen.
                    if e.kind() == std::io::ErrorKind::ConnectionRefused {
                        let _ = std::fs::remove_file(&effective);
                    }
                    if !spawned {
                        if let Err(spawn_err) = spawn_daemon_detached() {
                            return ClientOutcome::SpawnUnavailable(spawn_err);
                        }
                        spawned = true;
                    }
                }
                Err(e) => return ClientOutcome::Served(Err(e.into())),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (effective, &mut spawned);
            return ClientOutcome::Served(Err(anyhow::anyhow!(
                "multi-client daemon mode is unsupported on this platform; \
                 set `multi_client_daemon: false` in engram_mcp.yaml"
            )));
        }

        if std::time::Instant::now() >= deadline {
            return ClientOutcome::Served(Err(anyhow::anyhow!(
                "engram_server: no daemon accepted a connection on {} within {}s. \
                 On very large stores the daemon's startup can exceed this — raise \
                 `multi_client_connect_timeout_secs` in engram_mcp.yaml. If the \
                 problem persists, check the daemon log (`log_file`, default \
                 <data_dir>/engram-daemon.log) and restart your MCP client.",
                effective.display(),
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// Windows: strip HANDLE_FLAG_INHERIT from this process's std handles so
/// a spawned child cannot inherit them. Without this, the daemon ends up
/// holding a copy of the MCP host's stdio PIPE handles (host → client →
/// launcher → daemon all inherit), and the host's `read()` on the client's
/// stdout NEVER returns EOF — even after the client exits — because the
/// long-lived daemon still holds the pipe's write end. Observed in
/// production: the MCP host hung forever after the client timed out.
#[cfg(windows)]
fn unset_std_handle_inheritance() {
    use std::os::windows::io::AsRawHandle as _;
    const HANDLE_FLAG_INHERIT: u32 = 0x1;
    unsafe extern "system" {
        fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    unsafe {
        let _ = SetHandleInformation(std::io::stdin().as_raw_handle(), HANDLE_FLAG_INHERIT, 0);
        let _ = SetHandleInformation(std::io::stdout().as_raw_handle(), HANDLE_FLAG_INHERIT, 0);
        let _ = SetHandleInformation(std::io::stderr().as_raw_handle(), HANDLE_FLAG_INHERIT, 0);
    }
}

/// Spawn the shared daemon so that it survives this client's exit.
///
/// Windows: spawn a short-lived `--daemon-launcher` intermediary which
/// spawns the real `--daemon` process DETACHED and exits immediately.
/// The extra hop breaks the parent chain, so a host-side tree-kill of
/// this client (taskkill /T) cannot reach the daemon.
///
/// Unix: spawn `--daemon` directly into its own process group; group
/// signals aimed at the client can't touch it, and it reparents to init
/// when this process dies.
fn spawn_daemon_detached() -> anyhow::Result<()> {
    #[cfg(windows)]
    unset_std_handle_inheritance();
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot resolve current executable: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.arg("--daemon-launcher");
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.arg("--daemon");
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn engram daemon: {e}"))?;
    Ok(())
}

/// `--daemon-launcher` entry point: spawn the real daemon fully detached
/// and exit. Runs synchronously — this process lives for milliseconds.
pub fn run_daemon_launcher() -> anyhow::Result<()> {
    #[cfg(windows)]
    unset_std_handle_inheritance();
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot resolve current executable: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("launcher failed to spawn daemon: {e}"))?;
    Ok(())
}

/// `--daemon` entry point: become the shared primary if the lock is free,
/// otherwise exit quietly (another daemon won a startup race). Serves IPC
/// sessions only — no stdio session — and exits after
/// `multi_client_idle_timeout_secs` with zero connected clients.
/// Trace a fatal daemon-startup failure so it survives the detached
/// process's null stderr.
///
/// The daemon is spawned with `stderr = Stdio::null()`, so an `Err` returned
/// from `main` is formatted to a discarded handle. Without this, a daemon
/// that dies before binding its IPC endpoint leaves no trace at all: the log
/// shows "daemon starting" and nothing more, while every client blocks until
/// `multi_client_connect_timeout_secs` expires. Tracing the error routes it
/// to the configured `log_file`, which is the only sink a detached daemon has.
pub fn log_fatal_daemon_error(err: &anyhow::Error) {
    tracing::error!(
        error = %format!("{err:#}"),
        "engram_server: daemon failed to start — no IPC endpoint was created, \
         so every client will block until multi_client_connect_timeout_secs \
         expires. Fix the cause above and restart your MCP client."
    );
}

pub async fn run_daemon(cfg: Config) -> anyhow::Result<()> {
    let data_dir = cfg.data_dir.clone();
    std::fs::create_dir_all(&data_dir)?;
    let lock_path = derive_lock_path(&data_dir);
    let socket_path = resolve_socket_path(&cfg);
    let metadata_path = derive_metadata_path(&data_dir);
    match LockHandle::try_acquire(&lock_path)? {
        Some(handle) => {
            tracing::info!(
                pid = std::process::id(),
                version = env!("CARGO_PKG_VERSION"),
                "engram_server: daemon starting (won primary lock)"
            );
            run_primary_core(cfg, handle, metadata_path, socket_path, false).await
        }
        None => {
            tracing::info!(
                "engram_server: daemon exiting — another primary already holds the lock"
            );
            Ok(())
        }
    }
}

// ─── Primary ────────────────────────────────────────────────────────────────

/// State a primary process carries across its lifetime. Everything
/// the MCP session handler needs is here so session tasks can be
/// spawned freely.
struct PrimaryState {
    /// Count of currently-active MCP sessions (stdio + sockets). Used
    /// by the idle watchdog to decide when to exit.
    active_sessions: Arc<AtomicUsize>,
    /// Shutdown signal fired when idle timeout expires or a SIGTERM
    /// arrives.
    shutdown: tokio_util::sync::CancellationToken,
}

/// Legacy in-process primary: serves its own stdio session AND the IPC
/// listener. Used when daemon spawning is disabled or unavailable.
async fn run_primary(
    cfg: Config,
    lock_handle: LockHandle,
    metadata_path: PathBuf,
    socket_path: PathBuf,
) -> anyhow::Result<()> {
    run_primary_core(cfg, lock_handle, metadata_path, socket_path, true).await
}

async fn run_primary_core(
    cfg: Config,
    lock_handle: LockHandle,
    metadata_path: PathBuf,
    socket_path: PathBuf,
    serve_stdio: bool,
) -> anyhow::Result<()> {
    // Write primary metadata into the sibling metadata file so
    // proxies can find us without fighting the OS-level lock.
    let meta = LockMetadata {
        pid: std::process::id(),
        started_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        protocol_version: PROTOCOL_VERSION,
        socket_path: socket_path.to_string_lossy().into_owned(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    write_primary_metadata(&metadata_path, &meta)?;

    // Build AppState. This opens every storage layer — Redb, Tantivy,
    // LanceDB, checkpoints, etc. Exactly the same init path the
    // single-client mode would run.
    let (state, events_rx) = crate::state::AppState::new(cfg.clone())?;

    // Cleanup orphaned jobs — same as legacy main().
    {
        let reg = state.registry.clone();
        if let Ok(Ok(count)) =
            tokio::task::spawn_blocking(move || reg.cleanup_orphaned_jobs()).await
        {
            if count > 0 {
                tracing::info!("Aborted {count} orphaned jobs.");
            }
        }
    }

    let shutdown = tokio_util::sync::CancellationToken::new();

    // Background actors — same as legacy main().
    tokio::spawn(crate::actors::dreamer::run_dreamer(
        state.clone(),
        events_rx,
        shutdown.clone(),
    ));
    tokio::spawn(crate::actors::watcher::run_watcher(
        state.clone(),
        state.events_tx.subscribe(),
        shutdown.clone(),
    ));
    tokio::spawn(crate::actors::gc::run_gc_scheduler(
        state.clone(),
        shutdown.clone(),
    ));
    tokio::spawn(crate::actors::immune::run_immune_actor(
        state.clone(),
        shutdown.clone(),
    ));
    tokio::spawn(crate::services::integrity_service::run_integrity_checker(
        state.clone(),
        shutdown.clone(),
    ));

    let ps = PrimaryState {
        active_sessions: Arc::new(AtomicUsize::new(0)),
        shutdown: shutdown.clone(),
    };

    // Start the socket listener. Socket file is removed on startup
    // if stale (prior primary crashed without cleanup).
    remove_stale_socket(&socket_path)?;
    let listener_handle = spawn_socket_listener(
        socket_path.clone(),
        state.clone(),
        ps.active_sessions.clone(),
        ps.shutdown.clone(),
    )
    .await?;

    // Idle watchdog — exits when active_sessions stays at 0 for too long.
    let idle_timeout = std::time::Duration::from_secs(cfg.multi_client_idle_timeout_secs.max(10));
    tokio::spawn(run_idle_watchdog(
        ps.active_sessions.clone(),
        ps.shutdown.clone(),
        idle_timeout,
    ));

    tracing::info!(
        primary_pid = meta.pid,
        socket = %socket_path.display(),
        idle_timeout_secs = cfg.multi_client_idle_timeout_secs,
        "engram_server: primary mode"
    );

    // Spawn a SIGTERM/Ctrl-C handler so graceful shutdown is
    // possible from outside. When fired, it signals the same
    // shutdown token the idle watchdog uses, so the cleanup path
    // below runs identically.
    {
        let shutdown_signal = ps.shutdown.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("multi-client primary: received shutdown signal");
                shutdown_signal.cancel();
            }
        });
    }

    // Serve the stdio session for THIS process's own MCP client (legacy
    // in-process primary only). Counted as an active session so the
    // watchdog doesn't kill us while the caller is mid-request. A daemon
    // has no MCP client of its own — it serves IPC sessions exclusively
    // and lives by the idle watchdog alone.
    let stdio_result = if serve_stdio {
        ps.active_sessions.fetch_add(1, Ordering::SeqCst);
        let r = crate::tools::run_stdio(state.clone()).await;
        ps.active_sessions.fetch_sub(1, Ordering::SeqCst);
        r
    } else {
        Ok(())
    };

    // Wait until shutdown is actually signalled — either the idle
    // watchdog fires (active_sessions stayed 0 long enough) or a
    // signal handler fires. We MUST NOT timeout here: if peers are
    // still connected, this blocks as it should.
    ps.shutdown.cancelled().await;
    tracing::info!("multi-client primary: shutting down, cleaning up");

    // Now drain in-flight tasks with a 10s grace window. After that
    // we cleanup regardless — better to lose a straggler request
    // than to hang the process.
    let grace = std::time::Duration::from_secs(10);
    listener_handle.abort();
    let _ = tokio::time::timeout(grace, listener_handle).await;

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&metadata_path);
    // Drop the lock handle LAST so the next primary doesn't race
    // acquisition before we finish cleaning up the socket + metadata.
    drop(lock_handle);
    stdio_result
}

fn remove_stale_socket(socket_path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        if socket_path.exists() {
            // A real listener holds an exclusive lock indirectly via
            // the lock file — if we got this far we own the lock, so
            // whatever socket is there is leftover from a crashed
            // primary. Safe to remove.
            std::fs::remove_file(socket_path).map_err(|e| {
                anyhow::anyhow!(
                    "failed to remove stale socket {}: {e}",
                    socket_path.display()
                )
            })?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = socket_path;
    }
    Ok(())
}

async fn spawn_socket_listener(
    socket_path: PathBuf,
    state: crate::state::AppState,
    active: Arc<AtomicUsize>,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    #[cfg(unix)]
    {
        let listener = tokio::net::UnixListener::bind(&socket_path)?;
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((sock, _addr)) => {
                                // Atomic reserve-or-reject: use
                                // fetch_update to both cap at
                                // MAX_CONCURRENT_SESSIONS AND
                                // increment before the task spawns.
                                // This closes the window where the
                                // watchdog could observe idle=0
                                // between accept() and the session
                                // task actually running.
                                let reserved = active.fetch_update(
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                    |c| if c < MAX_CONCURRENT_SESSIONS { Some(c + 1) } else { None },
                                );
                                if reserved.is_err() {
                                    tracing::warn!(
                                        cap = MAX_CONCURRENT_SESSIONS,
                                        "rejecting socket connection — concurrent-session cap hit"
                                    );
                                    drop(sock);
                                    continue;
                                }
                                let state_c = state.clone();
                                let active_c = active.clone();
                                let shutdown_c = shutdown.clone();
                                tokio::spawn(async move {
                                    serve_socket_session(sock, state_c, shutdown_c).await;
                                    active_c.fetch_sub(1, Ordering::SeqCst);
                                });
                            }
                            Err(e) => {
                                tracing::warn!("accept error on multi-client socket: {e}");
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }
        });
        Ok(handle)
    }
    #[cfg(windows)]
    {
        // TODO-38: named-pipe transport. Named pipes differ from Unix
        // sockets — a server instance accepts exactly one client via
        // `connect()`, so we create the next instance after each
        // connection to keep accepting. The lock file already guarantees
        // a single primary, so `first_pipe_instance(true)` on the first
        // create just asserts that invariant.
        use tokio::net::windows::named_pipe::ServerOptions;
        let pipe_name = socket_path.to_string_lossy().into_owned();
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .map_err(|e| anyhow::anyhow!("failed to create named pipe {pipe_name}: {e}"))?;
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    conn = server.connect() => {
                        match conn {
                            Ok(()) => {
                                // `server` is now bound to this client; spin
                                // up the next instance to accept the following
                                // one (without first_pipe_instance — an
                                // instance already exists).
                                let connected = server;
                                server = match ServerOptions::new().create(&pipe_name) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        tracing::error!(
                                            "failed to create next named-pipe instance: {e}"
                                        );
                                        // Still serve the client we have.
                                        let state_c = state.clone();
                                        let active_c = active.clone();
                                        let shutdown_c = shutdown.clone();
                                        if active
                                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
                                                (c < MAX_CONCURRENT_SESSIONS).then_some(c + 1)
                                            })
                                            .is_ok()
                                        {
                                            tokio::spawn(async move {
                                                serve_pipe_session(connected, state_c, shutdown_c)
                                                    .await;
                                                active_c.fetch_sub(1, Ordering::SeqCst);
                                            });
                                        }
                                        break;
                                    }
                                };
                                let reserved = active.fetch_update(
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                    |c| if c < MAX_CONCURRENT_SESSIONS { Some(c + 1) } else { None },
                                );
                                if reserved.is_err() {
                                    tracing::warn!(
                                        cap = MAX_CONCURRENT_SESSIONS,
                                        "rejecting named-pipe connection — concurrent-session cap hit"
                                    );
                                    drop(connected);
                                    continue;
                                }
                                let state_c = state.clone();
                                let active_c = active.clone();
                                let shutdown_c = shutdown.clone();
                                tokio::spawn(async move {
                                    serve_pipe_session(connected, state_c, shutdown_c).await;
                                    active_c.fetch_sub(1, Ordering::SeqCst);
                                });
                            }
                            Err(e) => {
                                tracing::warn!("named-pipe connect error: {e}");
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }
        });
        Ok(handle)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (socket_path, state, active, shutdown);
        anyhow::bail!(
            "multi-client mode is unsupported on this platform; \
             set `multi_client: false` in engram_mcp.yaml to run the legacy single-client path"
        );
    }
}

// ─── Handshake capture ───────────────────────────────────────────────────────

/// How many bytes of a session's opening traffic to keep for diagnostics.
/// Enough for an `initialize` request plus a little slack; a healthy session
/// stops growing the buffer once it is full.
const HANDSHAKE_CAPTURE_BYTES: usize = 8192;

/// AsyncRead wrapper that keeps a copy of the first bytes a session sends.
///
/// rmcp turns an undeserialisable message into a SILENT stream close: its
/// codec error is logged without the offending line at anything below
/// `debug`, `receive()` returns `None`, and the server then reports only
/// "connection closed: initialized request". Twenty of those appeared in the
/// log across five days with no way to tell what the client had sent. The
/// capture makes the failure self-describing.
pub(crate) struct CapturingReader<R> {
    inner: R,
    captured: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl<R> CapturingReader<R> {
    pub(crate) fn new(inner: R, captured: Arc<std::sync::Mutex<Vec<u8>>>) -> Self {
        Self { inner, captured }
    }
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CapturingReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &poll {
            let new = &buf.filled()[before..];
            if !new.is_empty()
                && let Ok(mut c) = self.captured.lock()
                && c.len() < HANDSHAKE_CAPTURE_BYTES
            {
                let room = HANDSHAKE_CAPTURE_BYTES - c.len();
                c.extend_from_slice(&new[..room.min(new.len())]);
            }
        }
        poll
    }
}

/// Render captured handshake bytes for a log line: first line only, control
/// characters escaped, bounded length.
pub(crate) fn describe_capture(captured: &Arc<std::sync::Mutex<Vec<u8>>>) -> String {
    const MAX_RENDERED: usize = 2048;
    let Ok(bytes) = captured.lock() else {
        return String::new();
    };
    if bytes.is_empty() {
        return " (client sent no bytes before the connection closed)".to_string();
    }
    let text = String::from_utf8_lossy(&bytes);
    let first_line = text.split(['\n', '\r']).find(|l| !l.trim().is_empty());
    let Some(line) = first_line else {
        return format!(" (client sent {} bytes, all blank)", bytes.len());
    };
    let mut rendered: String = line
        .chars()
        .take(MAX_RENDERED)
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect();
    if line.chars().count() > MAX_RENDERED {
        rendered.push_str("…[truncated]");
    }
    format!(" — first message from client: {rendered}")
}

#[cfg(windows)]
async fn serve_pipe_session(
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    state: crate::state::AppState,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use rmcp::ServiceExt as _;
    // NamedPipeServer is a duplex AsyncRead+AsyncWrite; split into halves
    // and hand to rmcp's transport adapter — same shape as the stdio /
    // Unix-socket paths.
    let (read, write) = tokio::io::split(pipe);
    let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = (CapturingReader::new(read, captured.clone()), write);
    let engram = crate::tools::Engram::new(state);
    let service = match engram.serve(transport).await {
        Ok(s) => s,
        Err(e) => {
            // Include what the client actually sent — rmcp reports only
            // "connection closed" when it cannot deserialise a message.
            tracing::warn!(
                "named-pipe session handshake failed: {e}{}",
                describe_capture(&captured)
            );
            return;
        }
    };
    tokio::select! {
        // On cancel, the waiting() future (which owns/borrows `service`) is
        // dropped, which tears the rmcp session down — no explicit drop.
        _ = shutdown.cancelled() => {}
        result = service.waiting() => {
            if let Err(e) = result {
                tracing::warn!("named-pipe session ended with error: {e}");
            }
        }
    }
}

#[cfg(unix)]
async fn serve_socket_session(
    sock: tokio::net::UnixStream,
    state: crate::state::AppState,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use rmcp::ServiceExt as _;
    // Split the duplex UnixStream into (read, write) halves and hand
    // them to rmcp's (AsyncRead, AsyncWrite) transport adapter — same
    // shape as stdio.
    let (read, write) = sock.into_split();
    let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = (CapturingReader::new(read, captured.clone()), write);
    let engram = crate::tools::Engram::new(state);
    let service = match engram.serve(transport).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "socket session handshake failed: {e}{}",
                describe_capture(&captured)
            );
            return;
        }
    };
    tokio::select! {
        _ = shutdown.cancelled() => {
            // Graceful shutdown — close the session.
            // rmcp's service handles the tear-down when dropped.
            drop(service);
        }
        result = service.waiting() => {
            if let Err(e) = result {
                tracing::warn!("socket session ended with error: {e}");
            }
        }
    }
}

async fn run_idle_watchdog(
    active: Arc<AtomicUsize>,
    shutdown: tokio_util::sync::CancellationToken,
    idle_timeout: std::time::Duration,
) {
    let poll = std::time::Duration::from_secs(1);
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(poll) => {}
        }
        let count = active.load(Ordering::SeqCst);
        if count == 0 {
            let started = idle_since.get_or_insert_with(std::time::Instant::now);
            if started.elapsed() >= idle_timeout {
                tracing::info!(
                    idle_timeout_secs = idle_timeout.as_secs(),
                    "multi-client primary: idle timeout reached, shutting down"
                );
                shutdown.cancel();
                return;
            }
        } else {
            idle_since = None;
        }
    }
}

// ─── Proxy ──────────────────────────────────────────────────────────────────

async fn run_proxy(metadata_path: PathBuf, socket_path: PathBuf) -> anyhow::Result<()> {
    // A primary holds the OS lock on the lock file. Read its
    // metadata from the sibling `.engram.primary` file (always
    // readable — no mandatory lock contention on Windows).
    //
    // Startup race: a peer can lose the try_lock_exclusive race
    // milliseconds before the winner writes its metadata file.
    // Retry reads for up to a second so we don't fail just because
    // we got here faster than the winner could finish stage-2
    // startup.
    let mut meta_result = read_primary_metadata(&metadata_path);
    if meta_result.is_err() {
        let mut attempts = 0;
        while meta_result.is_err() && attempts < 20 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            meta_result = read_primary_metadata(&metadata_path);
            attempts += 1;
        }
    }
    let meta = match meta_result {
        Ok(m) => m,
        Err(e) => anyhow::bail!(
            "engram_server: could not start as proxy — primary metadata at {} is \
             unreadable ({e}). A primary may have crashed mid-startup. Restart your \
             MCP client to retry.",
            metadata_path.display()
        ),
    };
    if meta.protocol_version != PROTOCOL_VERSION {
        anyhow::bail!(
            "engram_server: proxy refused — primary (pid {pid}) is running protocol v{their}, \
             this binary speaks v{mine}. Restart the primary with the current binary.",
            pid = meta.pid,
            their = meta.protocol_version,
            mine = PROTOCOL_VERSION
        );
    }

    // The stored socket path wins over the derived one. Lets tests
    // override the path cleanly.
    let sock_path = if meta.socket_path.is_empty() {
        socket_path
    } else {
        PathBuf::from(&meta.socket_path)
    };

    #[cfg(unix)]
    {
        let sock = match tokio::net::UnixStream::connect(&sock_path).await {
            Ok(s) => s,
            Err(e) if is_missing_or_refused(&e) => {
                // Primary crashed after writing the lock file — the
                // OS released the advisory lock, so the next caller
                // (us or a racing peer) can take over. Delete the
                // stale socket + bail with a clear retry hint.
                let _ = std::fs::remove_file(&sock_path);
                anyhow::bail!(
                    "engram_server: primary (pid {pid}) appears to have crashed — stale socket \
                     at {sp} was cleared. Restart your MCP client to spawn a fresh primary.",
                    pid = meta.pid,
                    sp = sock_path.display()
                );
            }
            Err(e) => return Err(e.into()),
        };

        tracing::info!(
            primary_pid = meta.pid,
            socket = %sock_path.display(),
            "engram_server: proxy mode"
        );
        forward_stdio_to_socket(sock).await
    }
    #[cfg(windows)]
    {
        // TODO-38: connect to the primary's named pipe. ERROR_PIPE_BUSY
        // (231) means all instances are momentarily taken — retry briefly.
        // ERROR_FILE_NOT_FOUND (2) means no primary is listening (it
        // crashed after writing the lock); surface a clear retry hint.
        use tokio::net::windows::named_pipe::ClientOptions;
        let pipe_name = sock_path.to_string_lossy().into_owned();
        // The primary writes its metadata BEFORE the (potentially slow)
        // AppState init that precedes binding the pipe — so for a large
        // index there is a startup window where the metadata exists but the
        // listener isn't up yet. Both ERROR_PIPE_BUSY (231, all instances
        // momentarily taken) and ERROR_FILE_NOT_FOUND (2, primary still
        // starting) are transient: retry until the deadline, then treat a
        // persistently-absent pipe as a crashed primary.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let client = loop {
            match ClientOptions::new().open(&pipe_name) {
                Ok(c) => break c,
                Err(e) if matches!(e.raw_os_error(), Some(231) | Some(2)) => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "engram_server: primary (pid {pid}) is not listening on named                              pipe {pipe_name} after 30s — it may have crashed. Restart your                              MCP client to spawn a fresh primary.",
                            pid = meta.pid
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => return Err(e.into()),
            }
        };
        tracing::info!(
            primary_pid = meta.pid,
            pipe = %pipe_name,
            "engram_server: proxy mode (named pipe)"
        );
        forward_stdio_to_pipe(client).await
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = sock_path;
        anyhow::bail!(
            "multi-client mode is unsupported on this platform; \
             set `multi_client: false` in engram_mcp.yaml to run the legacy single-client path"
        );
    }
}

#[cfg(windows)]
async fn forward_stdio_to_pipe(
    pipe: tokio::net::windows::named_pipe::NamedPipeClient,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    // Byte-transparent passthrough between our stdio and the primary's
    // named pipe — mirror of forward_stdio_to_socket.
    let (mut pipe_read, mut pipe_write) = tokio::io::split(pipe);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = async {
        let res = tokio::io::copy(&mut stdin, &mut pipe_write).await;
        pipe_write.shutdown().await.ok();
        res.map(|_| ()).map_err(anyhow::Error::from)
    };
    let down = async {
        tokio::io::copy(&mut pipe_read, &mut stdout)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    };
    tokio::select! {
        r = up => { r.ok(); }
        r = down => { r.ok(); }
    }
    Ok(())
}

#[cfg(unix)]
fn is_missing_or_refused(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
    )
}

#[cfg(unix)]
async fn forward_stdio_to_socket(sock: tokio::net::UnixStream) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    // Byte-transparent passthrough between our stdio and the
    // primary's socket. `tokio::io::copy` handles the read/write
    // loop and EOF semantics; we run both directions concurrently
    // via `tokio::try_join!`, so when either direction completes
    // (EOF on stdin, EOF on the socket, or an error), the other
    // direction is cancelled and the proxy exits.
    //
    // No re-framing: bytes move through verbatim, so any MCP JSON-RPC
    // payload is forwarded as-is. This also means any protocol
    // change to MCP in the future is transparent to the auto-daemon.
    let (mut sock_read, mut sock_write) = sock.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let up = async {
        let res = tokio::io::copy(&mut stdin, &mut sock_write).await;
        // Half-close the write side so the primary sees EOF and can
        // wind down the MCP session cleanly.
        sock_write.shutdown().await.ok();
        res.map(|_| ()).map_err(anyhow::Error::from)
    };
    let down = async {
        tokio::io::copy(&mut sock_read, &mut stdout)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    };
    tokio::select! {
        r = up => { r.ok(); }
        r = down => { r.ok(); }
    }
    Ok(())
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture must record what the client sent, so a handshake failure
    /// says WHICH message could not be parsed instead of just "connection
    /// closed".
    #[tokio::test]
    async fn capturing_reader_records_the_first_message() {
        use tokio::io::AsyncReadExt as _;

        let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let payload = br#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#;
        let mut reader = CapturingReader::new(&payload[..], captured.clone());

        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).await.unwrap();

        assert_eq!(
            sink, payload,
            "the wrapper must pass bytes through verbatim"
        );
        let rendered = describe_capture(&captured);
        assert!(
            rendered.contains(r#""method":"initialize""#),
            "capture must surface the message: {rendered}"
        );
    }

    /// A healthy long-lived session must not accumulate its whole traffic in
    /// the diagnostic buffer.
    #[tokio::test]
    async fn capturing_reader_is_bounded() {
        use tokio::io::AsyncReadExt as _;

        let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let payload = vec![b'x'; HANDSHAKE_CAPTURE_BYTES * 4];
        let mut reader = CapturingReader::new(&payload[..], captured.clone());

        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).await.unwrap();

        assert_eq!(sink.len(), payload.len(), "pass-through must be complete");
        assert_eq!(
            captured.lock().unwrap().len(),
            HANDSHAKE_CAPTURE_BYTES,
            "the capture buffer must stop at its cap"
        );
    }

    /// A client that connects and closes without sending anything must be
    /// described as such, not as an unparseable message.
    #[test]
    fn describe_capture_reports_an_empty_handshake() {
        let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rendered = describe_capture(&captured);
        assert!(rendered.contains("no bytes"), "got: {rendered}");
    }

    /// Control characters must not be able to forge log lines, and only the
    /// first message is rendered.
    #[test]
    fn describe_capture_escapes_control_characters() {
        let raw = b"{\"a\":\"b\x07\"}\nsecond line".to_vec();
        let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(raw));
        let rendered = describe_capture(&captured);
        assert!(
            !rendered.contains('\u{7}'),
            "bell must be escaped: {rendered:?}"
        );
        assert!(
            !rendered.contains("second line"),
            "only the first line is rendered: {rendered:?}"
        );
    }

    // TODO-38: the named-pipe transport must round-trip bytes the same way
    // the Unix-socket path does. Exercises ServerOptions/ClientOptions +
    // tokio::io::split exactly as serve_pipe_session / forward_stdio_to_pipe.
    #[cfg(windows)]
    #[tokio::test]
    async fn named_pipe_transport_round_trips() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

        // pid-unique pipe name via the production deriver (no literals).
        let tag_path = std::path::PathBuf::from(format!("npt-{}", std::process::id()));
        let pipe_name = derive_pipe_name(&tag_path);
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .expect("create named pipe server");

        let srv = tokio::spawn(async move {
            server.connect().await.expect("server connect");
            let (mut r, mut w) = tokio::io::split(server);
            let mut buf = [0u8; 4];
            r.read_exact(&mut buf).await.expect("server read");
            for b in buf.iter_mut() {
                b.make_ascii_uppercase();
            }
            w.write_all(&buf).await.expect("server write");
            w.flush().await.ok();
        });

        // Client connect with ERROR_PIPE_BUSY (231) retry, mirroring run_proxy.
        let client = loop {
            match ClientOptions::new().open(&pipe_name) {
                Ok(c) => break c,
                Err(e) if e.raw_os_error() == Some(231) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => panic!("client open failed: {e}"),
            }
        };
        let (mut cr, mut cw) = tokio::io::split(client);
        cw.write_all(b"ping").await.expect("client write");
        cw.flush().await.ok();
        let mut got = [0u8; 4];
        cr.read_exact(&mut got).await.expect("client read");
        assert_eq!(&got, b"PING", "round-trip through the named pipe");
        srv.await.expect("server task");
    }

    #[test]
    fn lock_metadata_roundtrip() {
        let m = LockMetadata {
            pid: 12345,
            started_at_ms: 1713200000000,
            protocol_version: PROTOCOL_VERSION,
            socket_path: "/tmp/engram-abc.sock".into(),
            version: "9.9.9".into(),
        };
        let text = m.serialise();
        let parsed = LockMetadata::parse(&text).expect("must parse");
        assert_eq!(parsed, m);
    }

    #[test]
    fn lock_metadata_parse_rejects_missing_field() {
        // Missing `socket` → None.
        let text = "pid=1\nstarted_at_ms=2\nprotocol_version=1\n";
        assert!(LockMetadata::parse(text).is_none());
    }

    #[test]
    fn lock_metadata_parse_tolerates_extra_fields_and_ws() {
        let text = "pid = 7\nstarted_at_ms=100\nprotocol_version=1\nfuture_field=ignored\nsocket=/x/y.sock\n";
        let parsed = LockMetadata::parse(text).expect("must parse");
        assert_eq!(parsed.pid, 7);
        assert_eq!(parsed.socket_path, "/x/y.sock");
    }

    #[test]
    fn try_acquire_succeeds_on_fresh_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("engram.lock");
        let h = LockHandle::try_acquire(&path).unwrap();
        assert!(h.is_some(), "fresh file should be lockable");
    }

    #[test]
    fn try_acquire_returns_none_when_already_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("engram.lock");
        let h1 = LockHandle::try_acquire(&path).unwrap().expect("first lock");
        let h2 = LockHandle::try_acquire(&path).unwrap();
        assert!(h2.is_none(), "second attempt must see WouldBlock");
        drop(h1);
        // Once dropped, a fresh attempt must succeed.
        let h3 = LockHandle::try_acquire(&path).unwrap();
        assert!(h3.is_some(), "lock should be available after drop");
    }

    #[test]
    fn write_and_read_metadata_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock_path = tmp.path().join("engram.lock");
        let meta_path = tmp.path().join("engram.primary");
        let _h = LockHandle::try_acquire(&lock_path).unwrap().expect("lock");
        let meta = LockMetadata {
            pid: 999,
            started_at_ms: 42,
            protocol_version: PROTOCOL_VERSION,
            socket_path: "/tmp/s.sock".into(),
            version: String::new(),
        };
        write_primary_metadata(&meta_path, &meta).unwrap();
        // Metadata is readable by a "proxy" even while the primary
        // holds the lock — the two files are independent.
        let read = read_primary_metadata(&meta_path).unwrap();
        assert_eq!(read, meta);
    }

    #[test]
    fn metadata_can_be_read_while_lock_is_held() {
        // The whole point of splitting lock + metadata into two
        // files: on Windows, the locked file becomes mandatorily
        // inaccessible to other openers, so if metadata lived
        // inside the lock file, proxies couldn't read it. Guard
        // that invariant explicitly.
        let tmp = tempfile::TempDir::new().unwrap();
        let lock_path = tmp.path().join("engram.lock");
        let meta_path = tmp.path().join("engram.primary");
        let _h = LockHandle::try_acquire(&lock_path).unwrap().expect("lock");
        let meta = LockMetadata {
            pid: 1,
            started_at_ms: 2,
            protocol_version: PROTOCOL_VERSION,
            socket_path: "x".into(),
            version: String::new(),
        };
        write_primary_metadata(&meta_path, &meta).unwrap();
        // Simulating a proxy — try to read while the lock is held.
        let read = read_primary_metadata(&meta_path);
        assert!(
            read.is_ok(),
            "proxy-side metadata read must not be blocked by lock: {read:?}"
        );
    }

    #[test]
    fn derive_metadata_path_adds_filename() {
        let p = derive_metadata_path(Path::new("/data"));
        assert_eq!(p, PathBuf::from("/data/.engram.primary"));
    }

    #[test]
    fn session_counter_cap_via_fetch_update() {
        // Regression guard for the "active_sessions race" fix —
        // fetch_update must atomically reject increments above the
        // cap and return Err when the cap is hit.
        let active = std::sync::atomic::AtomicUsize::new(MAX_CONCURRENT_SESSIONS - 1);
        let res = active.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |c| {
                if c < MAX_CONCURRENT_SESSIONS {
                    Some(c + 1)
                } else {
                    None
                }
            },
        );
        assert!(res.is_ok());
        assert_eq!(
            active.load(std::sync::atomic::Ordering::SeqCst),
            MAX_CONCURRENT_SESSIONS
        );
        // At cap — next attempt must fail without mutating.
        let res = active.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |c| {
                if c < MAX_CONCURRENT_SESSIONS {
                    Some(c + 1)
                } else {
                    None
                }
            },
        );
        assert!(res.is_err());
        assert_eq!(
            active.load(std::sync::atomic::Ordering::SeqCst),
            MAX_CONCURRENT_SESSIONS
        );
    }

    #[tokio::test]
    async fn read_primary_metadata_tolerates_startup_race() {
        // Simulates the startup race: a proxy loses the lock race
        // and immediately tries to read metadata that the primary
        // hasn't written yet. We model this by writing the metadata
        // file after a short delay, then verifying a retry-reading
        // proxy eventually sees it.
        let tmp = tempfile::TempDir::new().unwrap();
        let meta_path = tmp.path().join(".engram.primary");
        let meta_path_clone = meta_path.clone();
        // Simulate the primary finishing startup after 100ms.
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            write_primary_metadata(
                &meta_path_clone,
                &LockMetadata {
                    pid: 1,
                    started_at_ms: 2,
                    protocol_version: PROTOCOL_VERSION,
                    socket_path: "/x".into(),
                    version: String::new(),
                },
            )
            .unwrap();
        });
        // Proxy-side retry loop modelled the same way as run_proxy.
        let mut meta_result = read_primary_metadata(&meta_path);
        let mut attempts = 0;
        while meta_result.is_err() && attempts < 20 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            meta_result = read_primary_metadata(&meta_path);
            attempts += 1;
        }
        assert!(
            meta_result.is_ok(),
            "proxy must eventually see freshly-written metadata within retry budget"
        );
        writer.await.unwrap();
    }

    #[test]
    fn short_tag_is_deterministic_and_16_chars() {
        let a = short_tag(Path::new("/home/user/.engram-data"));
        let b = short_tag(Path::new("/home/user/.engram-data"));
        let c = short_tag(Path::new("/home/user/other"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn derive_lock_path_adds_filename() {
        let p = derive_lock_path(Path::new("/data"));
        assert_eq!(p, PathBuf::from("/data/.engram.lock"));
    }

    #[test]
    fn derive_socket_path_uses_data_dir_when_short() {
        // Underscore: the assert is unix-only, so `p` is otherwise unused
        // on Windows builds.
        let _p = derive_socket_path(Path::new("/data"));
        #[cfg(unix)]
        assert_eq!(_p, PathBuf::from("/data/.engram.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn derive_socket_path_falls_back_to_tmp_when_too_long() {
        // Build a data-dir path that blows past the 104-byte limit.
        let long = "a".repeat(200);
        let long_dir = PathBuf::from(format!("/tmp/{long}"));
        let p = derive_socket_path(&long_dir);
        assert!(p.starts_with("/tmp/"), "expected /tmp fallback, got {p:?}");
        assert!(p.to_string_lossy().contains("engram-"));
    }

    // ── fatal daemon-startup error visibility ─────────────────────────────

    /// A detached daemon runs with `stderr = Stdio::null()`, so an `Err`
    /// bubbling out of `main` is written to a discarded handle and the
    /// operator sees only a client that blocks until its connect timeout.
    /// The failure must be traced instead, so it lands in `log_file`.
    #[test]
    fn fatal_daemon_error_is_traced_not_swallowed() {
        use std::io;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().expect("lock").extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let sink = Arc::new(Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(Buf(sink.clone()))
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(sub, || {
            let err =
                anyhow::anyhow!("config error: cannot canonicalize allowed root \"F:\\\\OciusX\"");
            log_fatal_daemon_error(&err);
        });

        let out = String::from_utf8(sink.lock().expect("lock").clone()).expect("utf8");
        assert!(
            out.contains("cannot canonicalize allowed root"),
            "the underlying cause must appear in the log, got: {out}"
        );
        assert!(
            out.contains("ERROR"),
            "must be logged at ERROR level, got: {out}"
        );
    }
}
