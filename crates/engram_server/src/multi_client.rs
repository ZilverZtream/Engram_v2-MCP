//! Multi-client auto-daemon — see `docs/MULTI_CLIENT_AUTO_DAEMON.md`.
//!
//! This module owns the "am I primary or proxy?" decision at startup.
//! When `cfg.multi_client == true`, `main.rs` delegates to
//! [`dispatch`] instead of opening storage directly.
//!
//! Goals:
//! - Zero user-facing config change: same MCP config, same binary.
//! - Never block two clients from running simultaneously against the
//!   same `data_dir`.
//! - No new networking — IPC is local only (Unix domain socket now;
//!   Windows named pipe is a v0.8 follow-up).
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
}

impl LockMetadata {
    fn serialise(&self) -> String {
        format!(
            "pid={}\nstarted_at_ms={}\nprotocol_version={}\nsocket={}\n",
            self.pid, self.started_at_ms, self.protocol_version, self.socket_path
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut pid = None;
        let mut started_at_ms = None;
        let mut protocol_version = None;
        let mut socket_path = None;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "pid" => pid = v.trim().parse().ok(),
                "started_at_ms" => started_at_ms = v.trim().parse().ok(),
                "protocol_version" => protocol_version = v.trim().parse().ok(),
                "socket" => socket_path = Some(v.trim().to_string()),
                _ => {}
            }
        }
        Some(LockMetadata {
            pid: pid?,
            started_at_ms: started_at_ms?,
            protocol_version: protocol_version?,
            socket_path: socket_path?,
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
pub fn write_primary_metadata(
    metadata_path: &Path,
    meta: &LockMetadata,
) -> anyhow::Result<()> {
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

/// Top-level multi-client entry point. Called from `main.rs` when
/// `cfg.multi_client` is true. Returns when the current process should
/// exit (either gracefully or because the MCP session closed).
///
/// When `cfg.multi_client` is `false`, `main.rs` continues to use the
/// legacy single-client code path — this function is not called at
/// all and the module imposes zero overhead.
pub async fn dispatch(cfg: Config) -> anyhow::Result<()> {
    let data_dir = cfg.data_dir.clone();
    // `data_dir` must exist before we try to put a lock file in it.
    std::fs::create_dir_all(&data_dir)?;
    let lock_path = derive_lock_path(&data_dir);
    let socket_path = match &cfg.multi_client_socket_path {
        Some(custom) => PathBuf::from(custom),
        None => derive_socket_path(&data_dir),
    };

    let metadata_path = derive_metadata_path(&data_dir);
    match LockHandle::try_acquire(&lock_path)? {
        Some(handle) => run_primary(cfg, handle, metadata_path, socket_path).await,
        None => run_proxy(metadata_path, socket_path).await,
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

async fn run_primary(
    cfg: Config,
    lock_handle: LockHandle,
    metadata_path: PathBuf,
    socket_path: PathBuf,
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
    };
    write_primary_metadata(&metadata_path, &meta)?;

    // Build AppState. This opens every storage layer — Redb, Tantivy,
    // LanceDB, checkpoints, etc. Exactly the same init path the
    // single-client mode would run.
    let (state, events_rx) = crate::state::AppState::new(cfg.clone())?;

    // Cleanup orphaned jobs — same as legacy main().
    {
        let reg = state.registry.clone();
        if let Ok(Ok(count)) = tokio::task::spawn_blocking(move || reg.cleanup_orphaned_jobs()).await
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

    // Serve the stdio session for THIS process's own MCP client.
    // Counted as an active session so the watchdog doesn't kill us
    // while the caller is mid-request.
    ps.active_sessions.fetch_add(1, Ordering::SeqCst);
    let stdio_result = crate::tools::run_stdio(state.clone()).await;
    ps.active_sessions.fetch_sub(1, Ordering::SeqCst);

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
                anyhow::anyhow!("failed to remove stale socket {}: {e}", socket_path.display())
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
    #[cfg(not(unix))]
    {
        let _ = (socket_path, state, active, shutdown);
        anyhow::bail!(
            "multi-client mode on non-Unix platforms is a v0.8 follow-up; \
             set `multi_client: false` in engram_mcp.yaml to run the legacy single-client path"
        );
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
    let transport = (read, write);
    let engram = crate::tools::Engram::new(state);
    let service = match engram.serve(transport).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("socket session handshake failed: {e}");
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
    #[cfg(not(unix))]
    {
        let _ = sock_path;
        anyhow::bail!(
            "multi-client mode on non-Unix platforms is a v0.8 follow-up; \
             set `multi_client: false` in engram_mcp.yaml to run the legacy single-client path"
        );
    }
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

    #[test]
    fn lock_metadata_roundtrip() {
        let m = LockMetadata {
            pid: 12345,
            started_at_ms: 1713200000000,
            protocol_version: PROTOCOL_VERSION,
            socket_path: "/tmp/engram-abc.sock".into(),
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
        let text =
            "pid = 7\nstarted_at_ms=100\nprotocol_version=1\nfuture_field=ignored\nsocket=/x/y.sock\n";
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
            |c| if c < MAX_CONCURRENT_SESSIONS { Some(c + 1) } else { None },
        );
        assert!(res.is_ok());
        assert_eq!(active.load(std::sync::atomic::Ordering::SeqCst), MAX_CONCURRENT_SESSIONS);
        // At cap — next attempt must fail without mutating.
        let res = active.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |c| if c < MAX_CONCURRENT_SESSIONS { Some(c + 1) } else { None },
        );
        assert!(res.is_err());
        assert_eq!(active.load(std::sync::atomic::Ordering::SeqCst), MAX_CONCURRENT_SESSIONS);
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
        let p = derive_socket_path(Path::new("/data"));
        #[cfg(unix)]
        assert_eq!(p, PathBuf::from("/data/.engram.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn derive_socket_path_falls_back_to_tmp_when_too_long() {
        // Build a data-dir path that blows past the 104-byte limit.
        let long = "a".repeat(200);
        let long_dir = PathBuf::from(format!("/tmp/{long}"));
        let p = derive_socket_path(&long_dir);
        assert!(
            p.starts_with("/tmp/"),
            "expected /tmp fallback, got {p:?}"
        );
        assert!(p.to_string_lossy().contains("engram-"));
    }
}
