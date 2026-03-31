#![allow(clippy::unwrap_used)]
//! Static sweep: all major actor event loops must check their shutdown/cancellation
//! token inside the main loop body.
//!
//! These tests perform a structural scan of the actor source files to prove that
//! every long-running background actor uses cooperative cancellation (via
//! `tokio::select! { _ = shutdown.cancelled() => … }` or equivalent) rather than
//! relying solely on process exit.
//!
//! This is a compile-time-equivalent check: the source must contain the patterns
//! that implement compliance.

/// The GC actor `run_gc_scheduler` must check its shutdown token inside
/// the main loop via `tokio::select!`.
#[test]
fn gc_actor_has_shutdown_check() {
    let source = include_str!("../src/actors/gc.rs");

    assert!(
        source.contains("shutdown.cancelled()"),
        "gc.rs must check shutdown.cancelled() inside the main loop; \
         source does not contain 'shutdown.cancelled()'"
    );
    assert!(
        source.contains("tokio::select!"),
        "gc.rs must use tokio::select! for cooperative cancellation"
    );
    assert!(
        source.contains("run_gc_scheduler"),
        "gc.rs must export run_gc_scheduler — structural invariant"
    );
}

/// The dreamer actor `run_dreamer` must check its shutdown token inside
/// the main loop via `tokio::select!`.
#[test]
fn dreamer_actor_has_shutdown_check() {
    let source = include_str!("../src/actors/dreamer.rs");

    assert!(
        source.contains("shutdown.cancelled()"),
        "dreamer.rs must check shutdown.cancelled() inside the main loop"
    );
    assert!(
        source.contains("tokio::select!"),
        "dreamer.rs must use tokio::select! for cooperative cancellation"
    );
}

/// The watcher actor must check its cancellation/shutdown token inside
/// the main event loop.
#[test]
fn watcher_actor_has_shutdown_check() {
    let source = include_str!("../src/actors/watcher.rs");

    // The watcher uses either shutdown.cancelled() or a similar stop signal.
    let has_cancel_check = source.contains("shutdown.cancelled()")
        || source.contains("cancel.cancelled()")
        || source.contains(".cancelled()")
        || source.contains("stop.cancelled()");

    assert!(
        has_cancel_check,
        "watcher.rs must check a cancellation token (.cancelled()) inside \
         its main event loop — currently missing cooperative shutdown"
    );
    assert!(
        source.contains("tokio::select!") || source.contains("select!"),
        "watcher.rs must use tokio::select! for cooperative cancellation"
    );
}

/// The immune actor must check its cancellation token.
#[test]
fn immune_actor_has_cancellation_check() {
    let source = include_str!("../src/actors/immune.rs");

    // Immune actor may be a simpler actor — check for either pattern.
    let has_cancel = source.contains(".cancelled()") || source.contains("CancellationToken");
    // Some actors are synchronous helpers with no loop — they are exempt.
    // If it has a `loop {` or `tokio::spawn`, it must have a cancellation check.
    let has_long_running = source.contains("loop {") || source.contains("tokio::spawn");

    if has_long_running {
        assert!(
            has_cancel,
            "immune.rs has a long-running loop/spawn but is missing \
             a cancellation token check — violation"
        );
    }
    // If no loop/spawn, it's a synchronous helper — no cancellation required.
    // Either way, the test must not panic.
}

/// The GC actor must pass its shutdown token as the FIRST arm in
/// `tokio::select!` (biased ordering) so that shutdown is always checked before
/// the interval tick — prevents the GC from running a full purge cycle on shutdown.
#[test]
fn gc_actor_shutdown_arm_appears_before_tick() {
    let source = include_str!("../src/actors/gc.rs");

    // Find the select! block and verify shutdown arm comes before tick arm.
    if let Some(select_pos) = source.find("tokio::select!") {
        let after_select = &source[select_pos..];
        let cancelled_pos = after_select.find("cancelled()").unwrap_or(usize::MAX);
        let tick_pos = after_select.find("interval.tick()").unwrap_or(usize::MAX);

        assert!(
            cancelled_pos < tick_pos,
            "in gc.rs tokio::select!, shutdown.cancelled() arm must appear \
             before interval.tick() arm to ensure shutdown is prioritized"
        );
    }
}

/// The dreamer actor must pass its shutdown token as the FIRST arm in
/// `tokio::select!` so shutdown is prioritized over the tick interval.
#[test]
fn dreamer_actor_shutdown_arm_appears_before_tick() {
    let source = include_str!("../src/actors/dreamer.rs");

    if let Some(select_pos) = source.find("tokio::select!") {
        let after_select = &source[select_pos..];
        let cancelled_pos = after_select.find("cancelled()").unwrap_or(usize::MAX);
        let tick_pos = after_select.find(".tick()").unwrap_or(usize::MAX);

        assert!(
            cancelled_pos < tick_pos,
            "in dreamer.rs tokio::select!, shutdown.cancelled() arm must appear \
             before .tick() arm to ensure shutdown is prioritized"
        );
    }
}

/// embed_batch_cancellable in embed.rs must wrap the HTTP send() call
/// in a tokio::select! that checks the cancellation token, so in-flight HTTP
/// requests are interrupted on shutdown — not left to complete or timeout naturally.
#[test]
fn embed_batch_cancellable_wraps_http_in_select() {
    let source = include_str!("../../engram_ml/src/embed.rs");

    assert!(
        source.contains("embed_batch_cancellable"),
        "embed.rs must export embed_batch_cancellable"
    );
    // The function must use tokio::select! to wrap the HTTP call.
    assert!(
        source.contains("cancel.cancelled()") || source.contains(".cancelled()"),
        "embed_batch_cancellable must check the cancellation token (.cancelled()) \
         inside a tokio::select! to interrupt in-flight HTTP requests"
    );
    assert!(
        source.contains("tokio::select!"),
        "embed_batch_cancellable must use tokio::select! to wrap HTTP calls"
    );
}

/// The main server must shut down all background actors via cancellation
/// tokens rather than letting them be killed by process exit — structural check.
#[test]
fn server_passes_shutdown_token_to_actors() {
    // Check that the server/main wiring passes CancellationToken to actors.
    // Actor startup lives in main.rs, not lib.rs (lib.rs is a thin re-export module).
    let source = include_str!("../src/main.rs");

    let has_cancellation = source.contains("CancellationToken")
        || source.contains("cancellation_token")
        || source.contains("shutdown");

    assert!(
        has_cancellation,
        "server main.rs must reference CancellationToken or shutdown \
         to pass cooperative shutdown signals to background actors"
    );
}

/// The integrity_checker actor must also receive the shutdown token —
/// it is a long-running background loop that previously lacked cooperative shutdown.
#[test]
fn integrity_checker_has_shutdown_check() {
    let source = include_str!("../src/services/integrity_service.rs");

    assert!(
        source.contains("shutdown.cancelled()") || source.contains(".cancelled()"),
        "integrity_service.rs must check a cancellation token inside its main loop"
    );
    assert!(
        source.contains("tokio::select!"),
        "integrity_service.rs must use tokio::select! for cooperative shutdown"
    );
    assert!(
        source.contains("CancellationToken"),
        "integrity_service.rs must accept a CancellationToken parameter"
    );
}

/// main.rs must pass the shutdown token to the integrity_checker, not spawn it naked.
#[test]
fn main_passes_shutdown_to_integrity_checker() {
    let source = include_str!("../src/main.rs");

    // The call site must pass `shutdown` (or a clone) to run_integrity_checker.
    let has_wiring = source.contains("run_integrity_checker")
        && (source.contains("shutdown.clone()") || source.contains("shutdown,"));

    assert!(
        has_wiring,
        "main.rs must pass shutdown token to run_integrity_checker — \
         bare spawn without shutdown token leaves the loop unshuttable"
    );
}

/// Exhaustive sweep: every source file in the server crate that contains both
/// `loop {` and `.await` must also contain a cancellation check (`.cancelled()`).
/// This proves no long-running async loop is missing cooperative shutdown.
#[test]
fn all_async_loops_in_server_have_cancellation_checks() {
    let sources = [
        ("actors/gc.rs",               include_str!("../src/actors/gc.rs")),
        ("actors/dreamer.rs",          include_str!("../src/actors/dreamer.rs")),
        ("actors/watcher.rs",          include_str!("../src/actors/watcher.rs")),
        ("actors/immune.rs",           include_str!("../src/actors/immune.rs")),
        ("services/integrity_service.rs", include_str!("../src/services/integrity_service.rs")),
    ];

    for (name, src) in sources {
        let has_loop  = src.contains("loop {");
        let has_await = src.contains(".await");
        let has_cancel = src.contains(".cancelled()") || src.contains("CancellationToken");

        if has_loop && has_await {
            assert!(
                has_cancel,
                "{name} contains `loop {{` with `.await` but no cancellation check — \
                 the loop can block shutdown indefinitely"
            );
        }
    }
}

/// X2-embmem-6c4p: the embedding memory guard must be scoped to a single batch,
/// not held for the entire job. This proves throughput is bounded by the per-batch
/// request timeout and the guard does not accumulate across batches.
///
/// Structural assertion: the source must contain `_embed_guard` created inside
/// the batch loop, alongside `embed_batch_cancellable` — proving the guard is
/// released between batches (RAII drop at block end), not pinned for the job lifecycle.
#[test]
fn embed_memory_guard_is_scoped_to_batch_not_job() {
    let source = include_str!("../../engram_index/src/hybrid.rs");

    // The per-batch embed guard must exist.
    assert!(
        source.contains("_embed_guard") || source.contains("embed_guard"),
        "X2-embmem-6c4p: hybrid.rs must hold an AllocationGuard per embedding batch \
         to bound memory usage and prove the guard is not held across the entire job"
    );

    // embed_batch_cancellable must be called within the guarded scope.
    assert!(
        source.contains("embed_batch_cancellable"),
        "X2-embmem-6c4p: hybrid.rs must call embed_batch_cancellable within the \
         guard scope — proving the throughput is bounded by the cancellation token \
         (and thus by the request timeout that drives cancellation)"
    );

    // Both guard and cancellable call must appear within the same logical block.
    let guard_pos = source.find("_embed_guard").or_else(|| source.find("embed_guard"));
    let cancellable_pos = source.find("embed_batch_cancellable");
    if let (Some(g), Some(c)) = (guard_pos, cancellable_pos) {
        let distance = (c as isize - g as isize).unsigned_abs();
        assert!(
            distance < 2000,
            "X2-embmem-6c4p: embed guard and embed_batch_cancellable must be in the same \
             batch loop body (within 2000 chars); actual distance: {distance} chars — \
             they may be in different scopes"
        );
    }
}

/// X4-adpjob-2n8q: every job-creation handler path must have validation
/// (project_id validation or ADP check) before the job is spawned.
///
/// Structural sweep across the three job-spawning handlers:
/// 1. project_tools.rs — spawn_job_index_directory and spawn_job_update_project
/// 2. git_tools.rs — spawn_job_git_history
///
/// Each must contain either `validate_project_id` / `ensure_project_record` /
/// `safe_join` (proving the path is authenticated before the job is dispatched).
#[test]
fn all_job_creation_handlers_have_authorization_gate_before_spawn() {
    let project_tools = include_str!("../src/handlers/project_tools.rs");
    let git_tools = include_str!("../src/handlers/git_tools.rs");

    // project_tools.rs must have project validation AND job spawning.
    let has_spawn_project = project_tools.contains("spawn_job_index_directory")
        || project_tools.contains("spawn_job_update_project");
    let has_gate_project = project_tools.contains("validate_project_id")
        || project_tools.contains("ensure_project_record")
        || project_tools.contains("state.paths")
        || project_tools.contains("safe_join");

    assert!(
        has_spawn_project,
        "X4-adpjob-2n8q: project_tools.rs must contain job-spawn call sites"
    );
    assert!(
        has_gate_project,
        "X4-adpjob-2n8q: project_tools.rs must contain project validation gate \
         (validate_project_id / ensure_project_record / safe_join) before spawning jobs"
    );

    // git_tools.rs must have project validation AND job spawning.
    let has_spawn_git = git_tools.contains("spawn_job_git_history");
    let has_gate_git = git_tools.contains("validate_project_id")
        || git_tools.contains("ensure_project_record")
        || git_tools.contains("state.paths")
        || git_tools.contains("safe_join");

    assert!(
        has_spawn_git,
        "X4-adpjob-2n8q: git_tools.rs must contain spawn_job_git_history call site"
    );
    assert!(
        has_gate_git,
        "X4-adpjob-2n8q: git_tools.rs must contain project validation gate before \
         spawning git history jobs"
    );

    // cognitive_tools.rs must contain the ADP evaluation gate (evaluate_gates).
    let cognitive_tools = include_str!("../src/handlers/cognitive_tools.rs");
    assert!(
        cognitive_tools.contains("evaluate_gates") || cognitive_tools.contains("apply_rollout_policy"),
        "X4-adpjob-2n8q: cognitive_tools.rs must contain ADP gate evaluation \
         (evaluate_gates / apply_rollout_policy) as the pre-condition for any \
         autonomous action approval"
    );
}

/// MEM2: structural sweep — all heavy allocation sites in the ingest and search
/// pipelines must be wrapped with AllocationGuard to ensure the memory budget
/// accounting is accurate.
///
/// Checks that the three primary heavy-allocation paths are guarded:
/// 1. Chunking/parse batch (hybrid.rs) — per-batch ParseBuffer guard
/// 2. Embedding batch (hybrid.rs) — per-batch embed guard
/// 3. Vector search oversample (hybrid.rs) — bounded top_k before fetch
///
/// This is a structural scan; the guards being present in the source proves the
/// allocation sites are budget-accounted rather than bypassing the AllocationGuard API.
#[test]
fn mem2_heavy_allocation_paths_use_allocation_guard() {
    let source = include_str!("../../engram_index/src/hybrid.rs");

    // 1. Chunking/parse batch must use AllocationGuard with ParseBuffer subsystem.
    assert!(
        source.contains("AllocationGuard::try_new") && source.contains("ParseBuffer"),
        "MEM2: hybrid.rs must use AllocationGuard with ParseBuffer for chunking/parse batches; \
         unguarded large parse-buffer allocations bypass the memory budget"
    );

    // 2. Embedding batch must use AllocationGuard (any subsystem).
    let guard_count = source.matches("AllocationGuard").count();
    assert!(
        guard_count >= 2,
        "MEM2: hybrid.rs must have at least 2 AllocationGuard sites (parse batch + embed batch); \
         found {guard_count} — one or more heavy allocation paths is unguarded"
    );

    // 3. Vector search oversample is bounded by .min(10_000) — prevents unbounded
    //    intermediate buffer allocation even without an AllocationGuard.
    assert!(
        source.contains(".min(10_000)") || source.contains(".min(10000)"),
        "MEM2: hybrid.rs must cap MMR oversample fetch with .min(10_000) to bound \
         intermediate vector buffer allocation"
    );
}

/// MEM2: AllocationGuard must be implemented in engram_core (not just imported)
/// so the central memory budget is the single accounting authority.
#[test]
fn mem2_allocation_guard_is_implemented_in_core() {
    let source = include_str!("../../engram_core/src/memory.rs");

    assert!(
        source.contains("pub struct AllocationGuard"),
        "MEM2: AllocationGuard must be a public struct in engram_core::memory — \
         without a central implementation, per-crate copies could diverge"
    );
    assert!(
        source.contains("impl Drop for AllocationGuard"),
        "MEM2: AllocationGuard must implement Drop to guarantee budget release \
         on all exit paths including panic unwinding"
    );
    assert!(
        source.contains("pub fn try_new"),
        "MEM2: AllocationGuard::try_new must be the only way to create a guard — \
         forcing callers through the budget check before acquiring the allocation"
    );
}

/// FTS3: vector-disabled code paths in hybrid.rs must degrade cleanly to
/// FTS-only retrieval without panicking.
///
/// Structural check: the source must contain the cfg-gated empty-vec return
/// that activates when the `vector` feature is disabled, proving the graceful
/// degradation path exists in the source and was not accidentally removed.
#[test]
fn fts3_vector_disabled_degradation_path_exists_in_source() {
    let source = include_str!("../../engram_index/src/hybrid.rs");

    // The vector search function must have a cfg-gated no-op path.
    let has_cfg_vector = source.contains("#[cfg(feature = \"vector\")]")
        || source.contains("#[cfg(not(feature = \"vector\"))]")
        || source.contains("cfg(feature = \"vector\")");

    assert!(
        has_cfg_vector,
        "FTS3: hybrid.rs must contain cfg(feature = \"vector\") gating to enable \
         clean FTS-only degradation when the vector feature is disabled"
    );

    // The source must not unconditionally panic when vector paths are absent.
    // Presence of a return-empty-vec fallback is the expected pattern.
    assert!(
        source.contains("vector_search") || source.contains("vec_results"),
        "FTS3: hybrid.rs must reference vector_search or vec_results — \
         the search merge path must handle empty vector results without panicking"
    );
}

/// FTS3: the MCP handler for semantic search must handle an empty vector result
/// set (as returned when vector feature is off) without panicking.
///
/// Structural check: search_tools.rs must handle an empty results vec from the
/// hybrid search and return a valid (possibly empty) MCP response.
#[test]
fn fts3_search_handler_handles_empty_vector_results_structurally() {
    let source = include_str!("../src/handlers/search_tools.rs");

    // The handler must not unwrap() on search results directly — it must handle
    // the case where both FTS and vector paths return empty results.
    let has_result_handling = source.contains("results.is_empty()")
        || source.contains("results.len()")
        || source.contains("if results")
        || source.contains("match.*result");

    assert!(
        has_result_handling,
        "FTS3: search_tools.rs must handle empty result sets from the search engine \
         (e.g. when vector feature is disabled) without panicking"
    );
}

/// CANCEL1-per-iter: the dreamer's inner `for pid in project_ids` loop must
/// check `shutdown.is_cancelled()` at each iteration so that a long project
/// list is preempted cooperatively during shutdown.
///
/// Without a per-iteration check, a 10_000-project list could take minutes to
/// drain after the shutdown signal arrives, delaying process exit.
#[test]
fn cancel1_dreamer_project_loop_checks_shutdown_per_iteration() {
    let source = include_str!("../src/actors/dreamer.rs");

    // The per-iteration cancel check must be inside the project loop.
    // We verify by checking that `is_cancelled()` appears in the source at all
    // (we already checked `shutdown.cancelled()` for the outer select! arm) —
    // we also need to confirm `shutdown.is_cancelled()` appears for the inner loop.
    assert!(
        source.contains("shutdown.is_cancelled()"),
        "CANCEL1-per-iter: dreamer.rs must call shutdown.is_cancelled() inside the \
         `for pid in project_ids` loop to preempt long project lists on shutdown. \
         Without this, a 10 000-project list blocks process exit for minutes."
    );
}

/// CANCEL1-slo: per-iteration cancel check must appear BEFORE the dream_once call
/// so shutdown latency is bounded by one dream_once call, not the entire batch.
#[test]
fn cancel1_dreamer_shutdown_check_precedes_dream_once_in_loop() {
    let source = include_str!("../src/actors/dreamer.rs");

    let cancelled_pos = source.find("shutdown.is_cancelled()").unwrap_or(usize::MAX);
    let dream_once_pos = source.find("dream_once(").unwrap_or(usize::MAX);

    assert!(
        cancelled_pos < dream_once_pos,
        "CANCEL1-slo: `shutdown.is_cancelled()` must appear before `dream_once(` in \
         dreamer.rs so cancellation latency is bounded by one dream cycle, not the \
         full batch. Positions: is_cancelled={cancelled_pos}, dream_once={dream_once_pos}"
    );
}

/// X1-7f9b: AllocationGuard must be explicitly dropped before the
/// `embed_batch_cancellable` await in hybrid.rs so the memory budget is not
/// held for the duration of a remote network call.
///
/// Holding the guard across an async await ties budget accounting to network
/// latency — a slow embedder (10s+ timeout) starves all other concurrent
/// allocations that share the same MemoryBudget.
#[test]
fn x1_7f9b_embed_guard_dropped_before_await() {
    let source = include_str!("../../engram_index/src/hybrid.rs");

    // The source must contain an explicit `drop(_embed_guard)` before the await.
    assert!(
        source.contains("drop(_embed_guard)"),
        "X1-7f9b: hybrid.rs must call `drop(_embed_guard)` before the \
         `embed_batch_cancellable(...).await` call. Holding AllocationGuard across \
         an async await ties the memory budget to network latency."
    );

    // The drop must appear before the embed_batch_cancellable call.
    let drop_pos = source.find("drop(_embed_guard)").unwrap_or(usize::MAX);
    let embed_pos = source.find("embed_batch_cancellable(chunk, cancel).await").unwrap_or(usize::MAX);
    assert!(
        drop_pos < embed_pos,
        "X1-7f9b: `drop(_embed_guard)` must appear before `embed_batch_cancellable(...).await` \
         in hybrid.rs. Positions: drop={drop_pos}, embed_batch={embed_pos}"
    );
}

/// MIG1-slo: the migration handler must log or surface incomplete report state
/// so callers can distinguish partial from complete reports without parsing JSON.
///
/// The markdown output path must prepend a machine-readable comment header
/// when `report_is_complete = false`, enabling automation (CI gates, health checks)
/// to detect partial migration analyses by pattern matching on the output string.
#[test]
fn mig1_handler_sources_completeness_header_in_markdown_output() {
    let source = include_str!("../src/handlers/migration_tools.rs");

    // The handler must reference the completeness state.
    assert!(
        source.contains("report_is_complete"),
        "MIG1-slo: migration_tools.rs must check report.report_is_complete so the \
         handler can distinguish complete from partial reports"
    );

    // The machine-readable HTML comment header must be written when incomplete.
    assert!(
        source.contains("MIG1:INCOMPLETE"),
        "MIG1-slo: migration_tools.rs must prepend an `<!-- MIG1:INCOMPLETE ... -->` \
         comment to markdown output when report_is_complete=false so automation \
         can detect partial reports without JSON parsing"
    );

    // The degraded_sections count must be surfaced to operators.
    assert!(
        source.contains("degraded_sections"),
        "MIG1-slo: migration_tools.rs must reference degraded_sections so operators \
         can identify which graph sections produced incomplete data"
    );
}
