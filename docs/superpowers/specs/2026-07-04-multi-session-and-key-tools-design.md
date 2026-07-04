# Multi-Session Hardening + Key-Tool Upgrades — Design

Date: 2026-07-04
Status: approved-by-default (autonomous session; user directive: "do an audit and then start improving Engram" + fix multi-session)

## Part 1 — Multi-session failure (root cause found)

### Symptoms
MCP connects in one Claude Code session; every other session's engram MCP "fails to start".

### Evidence
1. `multi_client: true` is deployed and the live process (pid 39320) runs in
   **primary mode** on `\\.\pipe\engram-3dd483b313a2dd43` (engram.log).
2. **No "proxy mode" line has ever been logged** — no second process ever ran.
3. `engram_logged.bat` is `engram_server.exe 2>> %LOCALAPPDATA%\engram\engram.log`.
   cmd.exe opens redirect targets **without FILE_SHARE_WRITE**. Reproduced:
   a second `cmd /c ... 2>> same.log` fails instantly with
   "The process cannot access the file because it is being used by another
   process" while the first holds it. The second session's server never launches.
4. Sandbox E2E test (two real processes, temp data_dir, deployed binary):
   A=primary, B=proxy via named pipe, both answered initialize + tools/list
   (133 tools). **The Rust multi-client path works.**

### Fix design
1. **Server-side log file** (removes the shell redirect forever):
   - `Config.log_file: Option<PathBuf>` + `ENGRAM_LOG_FILE` env override.
   - Open with append via Rust std (Windows share_read|share_write|share_delete
     by default → concurrent appenders are safe; FILE_APPEND_DATA keeps lines
     atomic enough for logs).
   - Startup rotation: if file > 64 MB, best-effort rename to `<name>.1`.
   - tracing subscriber writes to stderr AND the file (ANSI off in file).
2. **Detached daemon** (fixes "primary dies with session 1, orphaning proxies"):
   - New hidden flag `--daemon`: acquire `.engram.lock`; if lost → exit(0)
     (another daemon won). Runs AppState + actors + pipe/socket listener,
     **no stdio session**. Idle watchdog exits after
     `multi_client_idle_timeout_secs` with zero sessions.
   - Client path (`dispatch`): try pipe connect (fast path) → if no listener,
     spawn the daemon **detached** and retry connect until
     `multi_client_connect_timeout_secs` (default 120), then proxy stdio↔pipe.
   - Windows detach: double-spawn (client spawns `--daemon-launcher`, which
     spawns `--daemon` with DETACHED_PROCESS|CREATE_NEW_PROCESS_GROUP and
     exits) so `taskkill /T` on the client can't reach the daemon.
     Unix detach: `process_group(0)` (+ the same double-spawn for symmetry).
   - Fallback: `multi_client_daemon: bool` (default true). On spawn failure or
     `false`, keep today's in-process-primary behavior (still fixed by #1).
   - Metadata gains `version` (CARGO_PKG_VERSION); proxy logs a WARN on
     mismatch (stale daemon after redeploy).
3. **Deployment**: rewrite `engram_logged.bat` to plain exec (no redirect);
   add `log_file` to deployed YAML; rotate the existing 146 MB log; rebuild,
   redeploy, restart the live primary; verify with the sandbox E2E script and
   a second live session.

### Background-actor calm-down (found during diagnosis; production impact)
- Dreamer idle pass iterates ALL registered projects (~30) every idle window,
  opening tantivy+lancedb engines through a 5-slot LRU → permanent eviction
  storm every ~20 s, 146 MB log. Fix: idle pass only dreams projects with
  events since the last pass (dirty-set from the existing broadcast events);
  skip projects whose `directory` no longer exists (warn once per process).
- Immune actor: check `directory.exists()` FIRST; open/instantiate the search
  engine only when the git scan actually produced anti-patterns (it currently
  opens engines for all projects, then fails on missing repos).
- Registry hygiene at deploy: delete the ~28 stale eval projects pointing at
  deleted `%TEMP%\engram_eval_wt\pr*` worktrees.

## Part 2 — Key-tool upgrades (from the hot-path audit)

Ranked audit findings (agent-verified, file:line in audit report). Implementing
top items this session:

1. **`get_method_edit_context` token bomb (Blocker)** — defaults
   `max_callers: 50 → 3`, `include_caller_bodies: true → false` (callers render
   as signature + file:line unless bodies requested). It is the mandated
   pre-edit tool.
2. **Search snippet-first (Major)** — `include_content` default `true → false`;
   snippets (500 chars, truncation-flagged) remain; content via
   `include_content: true` or `get_chunk`.
3. **Access-layer staleness guard (Major, correctness)** — before reading a
   method body from disk by graph line numbers, compare the file's disk mtime
   against the last-index time; on drift emit a prominent STALE warning line in
   the response (and still return the body). Add the standard freshness footer
   to access-layer responses.
4. **Fuzzy fallback on method-lookup miss (Major)** — on no-match, return
   nearest method names (case-insensitive substring + trigram-ish ranking)
   instead of "ensure the project is indexed".
5. **`get_method_info` cap (Major)** — >10 matches renders a disambiguation
   table only (no full detail blocks); ≤10 renders details.
6. **Search pagination (Major)** — `offset` param on search_memory.

Deferred (documented, not this session): node_id/doc_id unification across the
access layer, watcher auto-enable, tool-surface pruning, dead-knob removal.

## Testing
- Unit tests per change (defaults, rotation, dirty-set, fuzzy suggestions).
- Sandbox E2E (Python, real processes): (a) two sessions concurrently OK,
  (b) daemon survives client-1 exit and serves client-2, (c) idle exit.
- `cargo fmt --all` + `cargo check --all-targets` + targeted test suites
  (`engram_server`, `engram_index`); full suite is OOM-prone → targeted.

## Non-goals
Networked Engram, HA failover, per-session data dirs, protocol bump.
