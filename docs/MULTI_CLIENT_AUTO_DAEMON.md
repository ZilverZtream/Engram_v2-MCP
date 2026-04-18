# Multi-Client Auto-Daemon — Dev Spec

**Status:** Approved, ready for implementation
**Owner:** (assign)
**Target release:** v0.7 (behind flag) → v0.8 (default on)
**Related docs:** `ARCHITECTURE.md`, `DATA_MODEL.md`, `GENERATION_MODEL.md`

---

## 1. Problem statement

Engram's storage stack is **single-writer per file** across every layer:

| Layer | Technology | Concurrency rule |
|---|---|---|
| Registry, Graph, DocStore, Checkpoints, Migration progress | Redb | One exclusive writer per `.redb` file; OS advisory lock enforces it |
| Full-text index | Tantivy | One `IndexWriter` per directory at a time |
| Vector index | LanceDB | Single-writer assumption on commit |

MCP (Model Context Protocol) is a **per-client stdio-spawn** protocol. Every agent that adds Engram to its config spawns its own `engram_server` process. Each process calls `AppState::new(cfg)` which tries to open every storage layer for writing.

**Observed failure mode**

```
Claude Desktop      ──spawn──▶  engram_server #1  ──open──▶  ~/.engram-data  ← holds lock
Claude Code         ──spawn──▶  engram_server #2  ──open──▶  ~/.engram-data  ← fails
CI pre-commit hook  ──spawn──▶  engram_server #3  ──open──▶  ~/.engram-data  ← fails
```

Process #2 and #3 fail at `Database::create()` with a cryptic "resource temporarily unavailable" or "file locked" error. The MCP handshake itself may succeed; every subsequent tool call fails with I/O errors. End users see "Engram is broken" and stop using it.

**Adoption impact.** Real-world scenarios that this blocks today:

1. Claude Desktop for chat + Claude Code in a terminal — same project, both want the graph.
2. Developer uses Engram locally + CI runs `pre_commit_review` on PRs — concurrent.
3. Two Claude Code terminals in different git worktrees of the same repository.
4. Agent running long-lived autonomous task + developer opens a second Claude Desktop window.

Without a fix, Engram is effectively a single-terminal tool — which contradicts the flagship-for-agents positioning.

---

## 2. Goals & non-goals

### Goals

- **G1.** Multiple MCP clients on the same machine can use Engram simultaneously against the same data directory.
- **G2.** **Zero-config change for users.** Existing `claude_desktop_config.json` / `.mcp.json` entries keep working without modification.
- **G3.** Single source of truth. All clients see a consistent view of the graph and search indexes.
- **G4.** No long-lived background process by default. An idle instance exits gracefully after a configurable timeout.
- **G5.** Clean crash recovery. A `kill -9`'d primary never leaves the system unusable — the next client to start recovers automatically.
- **G6.** Cross-platform. Linux, macOS, Windows — including Windows's quirky named-pipe semantics.
- **G7.** No network exposure. IPC stays on the local machine via Unix domain sockets or named pipes.
- **G8.** Observability parity. `get_metrics` exposes multi-client state (active connections, primary PID, idle seconds).

### Non-goals

- **NG1.** Multi-machine / networked Engram. Out of scope; teams that need that run a classic client-server deployment.
- **NG2.** High-availability failover. If the primary dies while a client is mid-request, that request fails; client retries with the new primary. No transparent retry.
- **NG3.** Sharing data via NFS / SMB / Dropbox. Advisory file locks are unreliable on networked filesystems; we detect and refuse rather than pretend it works.
- **NG4.** Agent-isolated views. Every client sees the same project graph. Per-client sandboxing is a future consideration, not part of this work.

---

## 3. High-level architecture

Every `engram_server` binary invocation decides at startup whether to be a **primary** or a **proxy** based on whether a data-directory lock is already held.

```
┌──────────────────────────────────────────────────────────────────────┐
│                         engram_server process                        │
│                                                                      │
│  1. Parse config + data_dir                                          │
│  2. Try to acquire exclusive lock on <data_dir>/.engram.lock         │
│                                                                      │
│  ┌──────────────┐           or           ┌─────────────────────┐     │
│  │  Got lock?   │  ──YES──▶  PRIMARY     │  Lock is held?       │     │
│  │              │                         │  Read <data_dir>/    │     │
│  └──────┬───────┘                         │  .engram.sock path   │     │
│         │                                 └──────────┬──────────┘     │
│         │                                            │                 │
│         ▼                                            ▼                 │
│  ─────────────────────────                  ─────────────────────      │
│  PRIMARY MODE                                PROXY MODE                │
│   - Open all storage layers                   - Connect to socket      │
│   - Construct AppState                        - Forward stdin ▶︎ sock │
│   - Spawn socket listener                     - Forward sock ▶︎ stdout│
│   - Serve this client's stdio                 - Exit when stdio or    │
│   - Serve N socket peers                        socket closes         │
│   - Exit after IDLE_TIMEOUT with no           - Spend zero time in    │
│     connected clients                           Engram business logic │
└──────────────────────────────────────────────────────────────────────┘
```

The primary owns the storage layers and processes every tool call. Proxies are byte-copiers between stdio and the local socket. The MCP client never knows whether its `engram_server` is a primary or a proxy — the JSON-RPC it sends and the responses it receives are byte-identical.

---

## 4. Lifecycle

### 4.1 Startup (every invocation)

```
1. Parse config (ENGRAM_CONFIG_PATH).
2. Resolve `data_dir`. Canonicalize.
3. Validate `data_dir` is on a local filesystem (see §7).
4. lock_path = <data_dir>/.engram.lock
5. sock_path = <data_dir>/.engram.sock     (Unix)
   pipe_name = \\.\pipe\engram-<sha256(data_dir)[..16]>   (Windows)
6. Attempt exclusive lock acquisition on lock_path (non-blocking).
   - Success → go to §4.2 (primary)
   - WouldBlock → go to §4.3 (proxy)
   - Other error → fatal, exit with clear message
```

### 4.2 Primary startup

```
1. Truncate and write to lock file:
     pid=<os_pid>
     started_at_ms=<unix_ts_ms>
     protocol_version=1
     socket=<sock_path or pipe_name>
2. Build AppState (existing path — opens Redb, Tantivy, LanceDB).
3. Spawn `socket_listener` task:
     - Unix: bind <sock_path>, unlink-on-bind dance for stale sockets
     - Windows: create named pipe with ACL restricting to current user SID
4. Spawn `idle_watchdog` task (§4.5).
5. Enter stdio MCP loop for THIS process's own client (the one that invoked us).
6. On every inbound socket connection, spawn a new `mcp_session` task sharing AppState.
```

### 4.3 Proxy startup

```
1. Read lock file, extract socket path + protocol_version.
2. If protocol_version != our_version → exit with "primary is running an
   incompatible version; upgrade or restart the primary".
3. Connect to socket. If ConnectionRefused or NotFound:
      → socket is stale (primary crashed). Try to take the lock ourselves:
         - If we get it → become primary (§4.2). Delete stale socket first.
         - If we still don't (race lost to another proxy) → retry from step 1
           with short backoff. Give up after 3s with a clear error.
4. Forward stdin ▶︎ socket bytewise.
5. Forward socket ▶︎ stdout bytewise.
6. Exit when stdin closes (client disconnect) or socket closes (primary
   shutdown / crash). Exit cleanly in either case.
```

### 4.4 Client session handling (primary side)

Every socket connection arriving at the primary is treated as a new MCP session:

- Spawn `handle_mcp_session(AppState.clone(), socket)`.
- Inside the task: wrap the socket in `tokio::io::BufReader` / `BufWriter`, hand it to `Engram::new(state).serve(transport)` just like stdio.
- The inbound client sees the full tool surface. Tool handlers don't need to know they're running via socket — `AppState` is identical.

**Implication for per-project locks.** Multiple concurrent sessions can race on the same project's writes. This is already handled by `AppState.project_update_locks` (per-project `Arc<Mutex>`), so we get serialisation for free. Reads can go parallel — Redb supports multi-reader.

### 4.5 Idle timeout

```
- Active client count = 1 (own stdio session) + N (socket sessions).
- When count drops to 0, start a timer = IDLE_TIMEOUT (default 300s, configurable).
- Each new connection resets the timer.
- On timer expiry: stop socket listener, finish any outstanding graceful
  work, release lock, unlink socket file, exit(0).
```

Default 5 min is short enough that a forgotten instance doesn't squat the lock, long enough that a Claude Code session closing and reopening within the same task doesn't incur cold-start cost twice.

### 4.6 Crash recovery

Three crash modes to handle:

| Crash | Detection | Recovery |
|---|---|---|
| Primary SIGKILL'd | OS releases advisory lock + socket file descriptor | Next client's `connect()` returns `ConnectionRefused`. Proxy detects stale socket, deletes it, acquires lock, becomes new primary. |
| Primary panics but doesn't cleanup | OS releases lock; socket file may linger | Same path as SIGKILL. The new primary's `bind()` will fail until it unlinks the stale socket file — hence the "unlink-on-bind" dance. |
| Proxy crashes mid-request | Primary sees socket read error | Primary logs "peer disconnected", drops session, decrements active count. No data corruption because writes are transactional. |

### 4.7 Graceful shutdown

Signal handling (SIGTERM / SIGINT on Unix, Ctrl-C on Windows):

1. Stop accepting new socket connections.
2. Close all active session socket writers (clients see EOF on their next read).
3. Wait up to 10s for tasks to complete.
4. Release lock, unlink socket file, exit.

Proxies receiving EOF on the socket exit cleanly with a message: "Engram primary shut down — exit and restart your MCP client to reconnect."

---

## 5. Locking strategy

### 5.1 Lock file

- **Path:** `<data_dir>/.engram.lock`
- **Contents:** plain-text key-value, one pair per line:
  ```
  pid=12345
  started_at_ms=1713200000000
  protocol_version=1
  socket=/home/user/.engram-data/.engram.sock
  ```
- **Acquisition:** `fs2::FileExt::try_lock_exclusive()` — non-blocking advisory lock.
- **Held for:** the entire lifetime of the primary process.
- **Released by:** OS on process exit (even SIGKILL).

### 5.2 Why advisory (not mandatory) locks

Windows has mandatory locking; Linux and macOS only offer advisory (`flock`/`fcntl`). Using advisory across all platforms means behaviour is uniform. The only loophole is a rogue process that ignores the lock — not a realistic threat in a single-user environment.

### 5.3 Filesystem validation

Advisory locks are unreliable on NFS < v4, older SMB, and most FUSE filesystems (Dropbox, OneDrive, iCloud Drive). On startup:

1. Stat `data_dir`.
2. On Linux, read `/proc/mounts` and check the FS type for the mount containing `data_dir`. If in the blocklist (`nfs`, `nfs4`, `cifs`, `fuse.dropbox`, `fuse.gvfs-fuse-daemon`), refuse to enter multi-client mode.
3. On macOS / Windows, use `statfs` / `GetVolumeInformation` to detect network filesystems similarly.
4. When refused, emit a clear error: "Engram multi-client mode requires a local filesystem. `<data_dir>` is on <fs_type> which does not support reliable advisory locks. Move the data directory to a local path or set `multi_client: false` in your config to run in single-client mode."

---

## 6. IPC protocol

### 6.1 Transport

- **Unix:** `tokio::net::UnixStream` on a Unix domain socket bound at `<data_dir>/.engram.sock`.
- **Windows:** `tokio::net::windows::named_pipe::NamedPipeServer` / `NamedPipeClient` on `\\.\pipe\engram-<sha256(data_dir)[..16]>`. The SHA prefix avoids collisions when a user has multiple Engram data directories.

### 6.2 Framing

The MCP protocol over stdio uses **JSON-RPC 2.0 with LSP-style Content-Length headers**:

```
Content-Length: 142\r\n
\r\n
{"jsonrpc":"2.0","id":1,"method":"tools/call", …}
```

The auto-daemon does **not re-frame**. The proxy streams bytes in both directions; the primary parses JSON-RPC exactly as it would from stdio. This has three big advantages:

1. Zero re-serialisation overhead.
2. No new protocol to version — any future MCP change is transparent.
3. `rmcp` already handles the stdio transport; we construct the same transport type over the socket and hand it to `Engram::new(state).serve(transport)`.

### 6.3 Authentication

On Unix, bind the socket with mode `0600`. On Windows, create the named pipe with a security descriptor allowing only the current user's SID. This matches how `gh auth`, `docker.sock`, and `/tmp/.X11-unix` handle local IPC. No password / token layer.

### 6.4 Version negotiation

A proxy's first action after connecting:

1. Read the primary's `protocol_version` from the lock file (already done at startup).
2. If mismatch, exit before forwarding any bytes.

This means version negotiation happens **out of band** via the lock file, not in the stream. The stream itself is byte-transparent MCP JSON-RPC. When we bump `protocol_version` in a future release, older proxies exit with a clear error telling the user to restart their MCP client.

---

## 7. Concurrency on the primary

### 7.1 Reads

Redb supports unlimited concurrent read transactions. Tantivy allows multiple concurrent `Searcher` handles. LanceDB supports concurrent reads. **No additional synchronisation needed for read-only tool calls.**

### 7.2 Writes

All writes go through `AppState`. Existing mechanisms already serialise:

- **Per-project indexing:** `AppState.project_update_locks: HashMap<String, Arc<Mutex<()>>>` — two concurrent `update_project` calls on the same project serialise.
- **Graph writes:** `GraphStore::upsert_nodes` / `upsert_edges` take an exclusive Redb write transaction internally.
- **Registry writes:** single Redb table, same.
- **Tantivy writes:** `IndexWriter` is exclusive-by-acquisition (current design already respects this).

The move to multi-client does not add new synchronisation points. It just lets more threads try to acquire the existing ones.

### 7.3 Backpressure

When many proxies fire concurrent `update_project` calls, the existing `project_update_locks` serialises them. Clients experiencing backpressure see their tool calls block — no failures, just slower responses. This is acceptable for the v0.7 release; if it becomes a problem we can add a request-rate metric and surface it via `get_metrics`.

---

## 8. Observability

Add to the existing `get_metrics` tool output:

```
multi_client:
  mode: "primary" | "proxy" | "single"
  primary_pid: 12345           # visible from both primary and proxy
  socket_path: "/…/.engram.sock"
  active_sessions: 3           # stdio + sockets
  idle_seconds: 12
  total_sessions_served: 142
  session_rejections: 0        # version-mismatch refusals etc.
  last_primary_start_ms: 1713200000000
```

And a new `tracing::Span` named `mcp_session` with the session's socket address / `stdio` tag, so log analysis can attribute tool calls to specific proxies.

---

## 9. Configuration

### 9.1 YAML

New field in `engram_mcp.yaml`:

```yaml
# Multi-client mode. When true, the first engram_server process to
# start becomes the primary and subsequent spawns proxy to it.
# When false (legacy), every process opens its own storage.
multi_client: true                    # default: true in v0.8+, false during v0.7 rollout

# How long an idle primary stays up before auto-exiting.
multi_client_idle_timeout_secs: 300   # default 300 (5 min)

# Override the IPC socket / pipe path. Default derived from data_dir.
# Useful only for tests.
multi_client_socket_path: null
```

### 9.2 Environment variables

Override YAML at launch time:

- `ENGRAM_MULTI_CLIENT=1` / `=0` → force multi-client on/off.
- `ENGRAM_MULTI_CLIENT_IDLE_SECS=60` → override idle timeout.

### 9.3 CLI flag (during rollout)

`engram_server --multi-client` / `--no-multi-client`. Wins over YAML and env. Removed in v0.9 once default is stable.

---

## 10. Failure modes (enumerated)

| # | Scenario | Behaviour |
|---|---|---|
| F1 | Lock acquisition returns `WouldBlock` → already a primary | Become proxy. Connect to socket. |
| F2 | Lock file exists but stale (primary crashed) | Our `try_lock_exclusive` succeeds because the OS already released the lock. We become primary and rewrite the lock file. |
| F3 | Socket file exists but no primary | `connect()` returns `ConnectionRefused` / `NotFound`. We delete the stale socket and retry lock acquisition. |
| F4 | Primary panics mid-request | Active sessions see EOF. Proxies forward EOF to stdout; MCP client sees transport error. Next spawn takes over. |
| F5 | Proxy loses connection while primary is alive | Log, drop the session, decrement count. No data impact. |
| F6 | Primary and proxy protocol version mismatch | Proxy reads lock file's `protocol_version`, sees mismatch, exits with a user-facing error before any tool call runs. |
| F7 | Two processes race to become primary after a crash | Only one `try_lock_exclusive` wins. The loser sees `WouldBlock`, becomes a proxy. |
| F8 | User sets `data_dir` on a networked filesystem | Startup detects the FS type and refuses multi-client mode with a specific error. User can either move the data dir or set `multi_client: false`. |
| F9 | User runs Engram as two different OS users against same data dir | UDS / named-pipe permissions prevent cross-user connection. Second user's process fails to connect, exits with permission error. (Correct behaviour — storage files likely don't permit cross-user access either.) |
| F10 | Idle timer fires while a session is still active | Only fires when count = 0. By construction cannot race against an active session. |
| F11 | Signal arrives during shutdown | Treat as immediate exit. Lock + socket cleaned up by OS. |
| F12 | The socket path is too long (Unix socket path limit = 108 bytes on Linux) | Detect at startup; fall back to `/tmp/engram-<sha256(data_dir)[..16]>.sock` with a log line explaining. |

---

## 11. Security considerations

- **Local IPC only.** UDS / named pipes never leave the machine.
- **File permissions.** Lock file, socket file, and data dir all `0600` (owner-only) on Unix. On Windows, Security Descriptor restricts to current user SID.
- **No shared-memory surface.** Every session has its own `Engram` instance in the primary; the shared surface is `AppState`, which is already the production sharing boundary today.
- **Denial of service.** A misbehaving proxy could open many sessions. Cap: max 32 concurrent sessions (configurable); beyond that the primary refuses new connections with a clear error.

---

## 12. Testing strategy

### 12.1 Unit tests

- `LockFile::try_acquire` — success, already-held, stale-but-locked, permission-denied.
- `parse_lock_file` — well-formed, missing fields, extra fields, encoding errors.
- `derive_socket_path` / `derive_pipe_name` — Unix socket length fallback, Windows pipe name hash stability.
- `is_local_filesystem` — matrix of FS types.

### 12.2 Integration tests

- **Two-client happy path** — spawn two `engram_server` processes sharing one `data_dir`. Both run `list_projects` concurrently. Assert both succeed and return identical output.
- **Primary crash recovery** — spawn primary, spawn two proxies, SIGKILL the primary. Assert both proxies exit cleanly; spawn a third process and assert it becomes the new primary.
- **Idle shutdown** — spawn primary, connect one proxy, disconnect, wait `IDLE_TIMEOUT + 1s`, assert primary has exited and lock is released.
- **Graceful shutdown** — SIGTERM the primary. Connected proxies see EOF. Lock released. No stale socket.
- **Concurrent writes on same project** — N proxies call `update_project` for the same project. Assert the per-project mutex serialises; no corruption.
- **Concurrent writes across projects** — N proxies call `update_project` for N different projects. Assert parallelism.
- **Version mismatch** — write a lock file with `protocol_version=999`. Start a new process. Assert it exits with the user-facing version error.
- **Stale socket cleanup** — create a socket file with no listener, start `engram_server`. Assert it becomes primary and deletes the stale socket.
- **Socket length fallback (Linux)** — configure `data_dir` such that the socket path exceeds 108 bytes. Assert fallback to `/tmp/`.

### 12.3 Cross-platform CI

GitHub Actions matrix: `ubuntu-latest`, `macos-latest`, `windows-latest`. Full integration suite on each.

### 12.4 Manual end-to-end tests

Not automated but validated before release:

- Claude Desktop + Claude Code simultaneously against same data dir.
- Claude Desktop running + Engram GitHub Action `pre_commit_review` on a PR.
- Two Claude Code terminals in different git worktrees of the same repo.
- Kill the primary while a slow `update_project` is running in another client → assert the client gets a clear error, next spawn recovers.

---

## 13. Implementation plan

### Phase A — Core daemon (3 days)

- Module structure: `engram_server/src/multi_client/` with `lock.rs`, `socket.rs`, `primary.rs`, `proxy.rs`, `mod.rs`.
- `LockFile` type: acquire, write metadata, release on drop.
- `SocketListener` abstraction with Unix / Windows impls.
- `dispatch()` function: lock → primary-or-proxy decision → run.
- Wire into `main.rs`: call `dispatch(state)` at startup instead of directly running the stdio loop.
- Unit tests for all the above.

### Phase B — Primary session multiplexing (2 days)

- Spawn per-connection `mcp_session` tasks on the primary.
- Confirm `AppState` clone-into-session pattern works (it already does — AppState is Clone).
- Per-project mutex interaction tested under concurrent writes.
- Idle-watchdog + graceful-shutdown wiring.

### Phase C — Proxy pass-through (1 day)

- Stdio ↔ socket bidirectional byte copy using `tokio::io::copy_bidirectional`.
- Version check at connect time.
- Stale-socket / lost-primary recovery loop.

### Phase D — Observability + config (1 day)

- `get_metrics` extension.
- Config parsing (YAML + env + CLI).
- Startup log lines: "primary" / "proxy" mode, PID, socket path.

### Phase E — Tests + docs (2 days)

- Full integration test suite.
- `docs/MULTI_CLIENT_AUTO_DAEMON.md` (this doc) stays as the design reference.
- Update `README.md` with the multi-client section.
- Update `docs/ARCHITECTURE.md` to describe the new dispatch boundary.

**Total: ~9 focused days.** Tracking slippage risk at +3 days for Windows named-pipe quirks.

### Phased rollout

| Release | Default | Behaviour |
|---|---|---|
| v0.7 | `multi_client: false` | Opt-in via flag / YAML / env. Users who want it set it. Real-world validation period. |
| v0.7.x | same | Bug fixes from early adopters. |
| v0.8 | `multi_client: true` | Default on. `single_client: true` escape hatch for anyone hitting an unforeseen edge. |
| v0.9 | same | Remove the escape hatch. |

---

## 14. The "don't regress" checklist

Before declaring Phase A done, hand-verify:

- [ ] Existing single-client flow unchanged when `multi_client: false` (default during v0.7).
- [ ] All existing integration tests still pass.
- [ ] `cargo build --release` and `cargo test --workspace` green on Linux, macOS, Windows.
- [ ] Startup time with multi_client on ≤ startup time with it off + 50ms.
- [ ] Memory footprint of a proxy process ≤ 10 MB (vs ~200 MB for a primary) — a proxy doesn't open storage.
- [ ] `strings` on the binary does not leak data_dir paths or PIDs.
- [ ] No new dependencies with transitive GPL / AGPL licences (check `cargo-deny`).

---

## 15. Open questions

**Q1. Idle timeout default.** 5 min feels right for interactive use but might be too aggressive for a CI system that triggers every 10 min. Options: (a) configurable, leave default at 5 min; (b) auto-detect "is this a CI environment" and extend to 30 min; (c) expose a tool call `keep_alive` clients can invoke. Proposal: (a) for v0.7. Reconsider if real-world use justifies (b) or (c).

**Q2. Can the primary process's own MCP client disconnect while peers remain?** Yes — proxies keep the primary alive via the active-session count. The primary's own stdio session is just one of N sessions; when THAT client disconnects, the primary keeps running as long as peers are connected. Proposal: implement this way; document as feature.

**Q3. Should proxies retry transparently if the primary dies mid-request?** No for v0.7. MCP clients already handle transport errors with their own retry logic. Surface the error, let the client retry. A future version could add transparent retry but it complicates semantics (idempotency).

**Q4. Per-user isolation on shared hosts.** Two users on the same machine against the same data dir would fail at the UDS permission layer. That matches expectations — shared Engram data across users is already complicated by Redb file permissions. Proposal: document as a non-goal.

**Q5. Do we need a `--kill-daemon` CLI subcommand for debugging?** Possibly useful. Proposal: add `engram_server --shutdown` that sends a graceful shutdown signal to the running primary (by reading `pid=` from the lock file). Cheap to add in Phase D.

**Q6. Should proxies cache metadata locally to avoid round-trips for fast read-only queries?** No for v0.7. Caching correctness in the face of cross-client writes is non-trivial. Revisit only if latency measurements show it matters.

---

## 16. Done definition

- All 12 failure modes in §10 either covered by automated tests or explicitly ruled out as non-goals.
- v0.7 ships with `multi_client: false` default, working opt-in path.
- Claude Desktop + Claude Code can run simultaneously against the same data dir for ≥ 8 hours in manual testing without hangs, corruption, or errors.
- Primary restart after SIGKILL takes ≤ 2 seconds measured end-to-end from a proxy client's perspective.
- Total added workspace LOC ≤ 2500 (target: ~1700).
- No change to the MCP client's config file; `engram_server` binary is drop-in-compatible.

---

## 17. References

- rmcp crate — MCP protocol implementation already in use: https://github.com/modelcontextprotocol/rust-sdk
- `fs2` crate — cross-platform advisory file locks
- Tokio's named-pipe support on Windows: `tokio::net::windows::named_pipe`
- Redb locking semantics: https://redb.org
- Background prior art: `rustup`'s update-daemon pattern, `docker`'s docker.sock, `gh auth`'s credential socket, `mise` / `asdf` tool-version daemons.
