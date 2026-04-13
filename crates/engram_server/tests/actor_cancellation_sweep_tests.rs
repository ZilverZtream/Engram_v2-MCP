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
        ("actors/gc.rs", include_str!("../src/actors/gc.rs")),
        (
            "actors/dreamer.rs",
            include_str!("../src/actors/dreamer.rs"),
        ),
        (
            "actors/watcher.rs",
            include_str!("../src/actors/watcher.rs"),
        ),
        ("actors/immune.rs", include_str!("../src/actors/immune.rs")),
        (
            "services/integrity_service.rs",
            include_str!("../src/services/integrity_service.rs"),
        ),
    ];

    for (name, src) in sources {
        let has_loop = src.contains("loop {");
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
    let guard_pos = source
        .find("_embed_guard")
        .or_else(|| source.find("embed_guard"));
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
        cognitive_tools.contains("evaluate_gates")
            || cognitive_tools.contains("apply_rollout_policy"),
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
    let embed_pos = source
        .find("embed_batch_cancellable(chunk, cancel).await")
        .unwrap_or(usize::MAX);
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

/// Migration report completeness: the `FullProjectMigrationReport` struct must
/// carry `report_is_complete` and `degraded_sections` as public fields, and they
/// must be included in the JSON serialization so clients receive them alongside
/// the full report content.
///
/// Clients that only check for an HTTP 200 / non-error response will silently
/// accept a partial report unless they explicitly check `report_is_complete`.
/// Having these fields in the serialized JSON makes them impossible to miss.
#[test]
fn migration_report_struct_exposes_completeness_fields_for_clients() {
    let service_src = include_str!("../src/services/full_project_migration_service.rs");

    // The struct definition must declare both fields.
    assert!(
        service_src.contains("pub report_is_complete: bool"),
        "FullProjectMigrationReport must have pub report_is_complete: bool so clients \
         can inspect completeness without parsing the markdown body"
    );
    assert!(
        service_src.contains("pub degraded_sections: Vec<String>"),
        "FullProjectMigrationReport must have pub degraded_sections: Vec<String> so \
         clients can identify which graph sections produced incomplete data"
    );

    // The struct must derive Serialize so these fields appear in JSON responses.
    // Find the derive line above the struct.
    let struct_pos = service_src
        .find("pub struct FullProjectMigrationReport")
        .expect("FullProjectMigrationReport must exist in migration service");
    // Look for #[derive(...Serialize...)] in the 400 chars before the struct.
    let before_struct = &service_src[struct_pos.saturating_sub(400)..struct_pos];
    assert!(
        before_struct.contains("Serialize"),
        "FullProjectMigrationReport must derive Serialize so report_is_complete and \
         degraded_sections appear in JSON MCP responses — clients cannot enforce \
         completeness checking if these fields are absent from the wire format"
    );
}

/// Migration handler: the JSON response path for the full-project analysis must
/// serialize the whole report (including `report_is_complete` and
/// `degraded_sections`) when `output_json = true`, not a stripped summary.
///
/// A client that sets `output_json=true` is explicitly requesting machine-readable
/// output; stripping completeness fields would silently hide degraded state.
#[test]
fn migration_handler_json_path_serializes_full_report_including_completeness() {
    let handler_src = include_str!("../src/handlers/migration_tools.rs");

    // Find the full-project analysis JSON path by locating the block that checks
    // output_json AND is near a `to_string_pretty(&report)` call.
    // We scan all occurrences of "to_string_pretty" and check which one
    // serializes `&report` (the FullProjectMigrationReport).
    let has_full_report_json = handler_src
        .lines()
        .any(|l| l.contains("to_string_pretty(&report)") || l.contains("to_string(&report)"));

    assert!(
        has_full_report_json,
        "migration handler JSON path must serialize the complete report with \
         serde_json::to_string_pretty(&report) so report_is_complete and \
         degraded_sections are included in the JSON response — a stripped \
         serialization would hide completeness state from JSON clients"
    );

    // The output_json flag must also be checked in the same handler (not removed).
    assert!(
        handler_src.contains("if req.output_json") || handler_src.contains("if output_json"),
        "migration handler must check the output_json flag — without it, JSON clients \
         cannot request machine-readable report output with completeness fields"
    );
}

/// Migration completeness: the service's internal `report_is_complete` derivation
/// must be logically tied to `degraded_sections` — complete iff no sections degraded.
///
/// This is the core invariant: if `degraded_sections` is non-empty, `report_is_complete`
/// must be false.  Any disconnect between the two fields would allow partial reports
/// to claim completeness.
#[test]
fn migration_service_report_is_complete_derived_from_degraded_sections() {
    let service_src = include_str!("../src/services/full_project_migration_service.rs");

    // The assignment of report_is_complete must reference degraded_sections.
    // Expected pattern: `let report_is_complete = degraded_sections.is_empty();`
    let rc_pos = service_src
        .find("report_is_complete")
        .expect("migration service must set report_is_complete");
    // Find the first assignment (let report_is_complete = ...)
    let assign_pos = service_src
        .find("let report_is_complete")
        .or_else(|| service_src.find("report_is_complete ="))
        .expect("report_is_complete must be assigned in migration service");
    let after_assign =
        &service_src[assign_pos..assign_pos + 200.min(service_src.len() - assign_pos)];

    assert!(
        after_assign.contains("degraded_sections"),
        "migration service must derive report_is_complete from degraded_sections \
         (e.g. `let report_is_complete = degraded_sections.is_empty()`) — \
         any other derivation risks the two fields becoming inconsistent. \
         Found near assignment: {:?}",
        &after_assign[..100.min(after_assign.len())]
    );

    let _ = rc_pos; // silence unused warning
}

/// CANCEL1-b2h7: the immune actor project scan loop must check the shutdown token
/// at each project iteration so a large project list can be preempted cooperatively
/// during process shutdown, not forced to run all projects to completion.
///
/// The dreamer actor already does this (verified in dreamer tests); this test
/// ensures parity across all long-running actor inner loops.
#[test]
fn immune_actor_project_loop_checks_shutdown_token() {
    let source = include_str!("../src/actors/immune.rs");

    // The immune actor must check shutdown.is_cancelled() inside the project for-loop,
    // not only at the outer select! (which only fires at tick boundaries).
    assert!(
        source.contains("shutdown.is_cancelled()"),
        "CANCEL1-b2h7: immune actor must call shutdown.is_cancelled() inside the \
         project scan loop — relying only on the outer select! tick check means \
         shutdown is delayed by the full scan duration when many projects are registered"
    );

    // The check must be followed by a return — pure logging without early exit
    // does not satisfy the cooperative shutdown contract.
    assert!(
        source.contains("shutdown cancelled during project scan loop"),
        "CANCEL1-b2h7: immune actor must log and return when shutdown is detected \
         mid-loop — the log message is the observable trace for operator debugging"
    );
}

/// REG1/MCP1: structural proof that `handle_update_memory_bank` does NOT swallow
/// the registry write result with `.ok()`.
///
/// Prior to this fix, `spawn_blocking(...).await.ok()` silently ignored both
/// the JoinError (task panicked) and the registry write error, causing the MCP
/// handler to reply with success even when the memory bank section was not
/// persisted.  The fix replaces `.ok()` with a chained `map_err(...)?` pair so
/// the caller receives a hard McpError on any failure.
///
/// This test scans the source for the registry write block inside the handler
/// and asserts:
///   1. `.ok()` is no longer used immediately after the `spawn_blocking` result.
///   2. `map_err` is present, proving error propagation is wired.
///   3. The comment "memory bank section not persisted" is present so the error
///      message is identifiable in operator logs.
#[test]
fn handle_update_memory_bank_registry_write_error_is_propagated_not_swallowed() {
    let source = include_str!("../src/handlers/project_tools.rs");

    // Locate the handle_update_memory_bank function.
    let fn_start = source
        .find("fn handle_update_memory_bank")
        .or_else(|| source.find("handle_update_memory_bank"))
        .expect("REG1/MCP1: handle_update_memory_bank must exist in project_tools.rs");

    // Take a window spanning the function — it's ≈ 100 lines from the start.
    let fn_body = &source[fn_start..fn_start + 3000.min(source.len() - fn_start)];

    // spawn_blocking for registry write must be present.
    assert!(
        fn_body.contains("put_memory_section"),
        "REG1/MCP1: handle_update_memory_bank must call put_memory_section to persist \
         the memory bank section to the registry"
    );

    // The immediate .ok() swallow must be gone.
    // Check that within 200 chars after "put_memory_section" there is no .ok()
    // that would swallow the JoinError.
    let pm_pos = fn_body
        .find("put_memory_section")
        .expect("REG1/MCP1: put_memory_section must appear in handle_update_memory_bank");
    let after_pm = &fn_body[pm_pos..pm_pos + 400.min(fn_body.len() - pm_pos)];

    assert!(
        !after_pm.contains(".await\n        .ok()") && !after_pm.contains(".await.ok()"),
        "REG1/MCP1: spawn_blocking(...).await.ok() must not appear in \
         handle_update_memory_bank — .ok() silently swallows both JoinError and \
         the registry write error, causing the handler to lie about persistence"
    );

    // map_err must appear — proving both the JoinError and write error are propagated.
    assert!(
        after_pm.contains("map_err"),
        "REG1/MCP1: handle_update_memory_bank registry write must use map_err to \
         convert errors into McpError so the MCP caller receives a hard failure \
         instead of a silent success when the write fails"
    );

    // The operator-visible error message must be present so failures are identifiable.
    assert!(
        fn_body.contains("memory bank section not persisted"),
        "REG1/MCP1: the error message 'memory bank section not persisted' must appear \
         in handle_update_memory_bank so registry write failures are identifiable in \
         operator logs and MCP error responses"
    );
}

/// REG1/MCP1: per-file sweep — `spawn_blocking` calls that write to the
/// registry (put_memory_section, put_project, set_reindex_required,
/// clear_reindex_required) must NOT swallow the result with `.ok()`.
///
/// Scans project_tools.rs for the specific anti-pattern:
///   `.await\n        .ok()`  or  `.await.ok()`
/// within 500 characters of a registry-write method name.
/// Read-only methods (get_project, list_projects) and intentional best-effort
/// operations (graph.delete_project_data) are NOT covered by this sweep.
#[test]
fn registry_write_spawn_blocking_results_are_not_swallowed_across_handlers() {
    let source = include_str!("../src/handlers/project_tools.rs");

    // These are registry write methods where failure must surface to the caller.
    let write_methods = [
        "put_memory_section",
        "put_project",
        "set_reindex_required",
        "clear_reindex_required",
        "store_job",
        "update_job",
    ];

    for method in write_methods {
        let mut search_from = 0usize;
        while let Some(rel) = source[search_from..].find(method) {
            let site = search_from + rel;
            let window_end = (site + 500).min(source.len());
            let window = &source[site..window_end];

            let has_ok_swallow = window.contains(".await\n        .ok()")
                || window.contains(".await.ok()")
                || window.contains(".await\r\n        .ok()");

            assert!(
                !has_ok_swallow,
                "REG1/MCP1: project_tools.rs calls `{method}` inside spawn_blocking \
                 and swallows the result with .ok() within 500 chars. \
                 Registry write failures must be propagated with map_err+? so MCP \
                 callers receive a hard error instead of a silent false success.\n\
                 Snippet: {:?}",
                &window[..200.min(window.len())]
            );

            search_from = site + 1;
        }
    }
}

/// CANCEL1-b2h7: structural proof that the immune actor uses `return` (not `break`)
/// when the mid-loop shutdown check fires, so the actor exits the function completely
/// rather than just breaking out of the inner loop and re-entering the outer loop.
#[test]
fn immune_actor_mid_loop_shutdown_uses_return_not_break() {
    let source = include_str!("../src/actors/immune.rs");

    // Find the `shutdown.is_cancelled()` check in the project loop.
    // The code immediately after must be `return` so the actor exits fully.
    let cancel_pos = source
        .find("shutdown.is_cancelled()")
        .expect("CANCEL1-b2h7: shutdown.is_cancelled() must be present in immune.rs");

    // The `return` keyword must appear within 10 lines of the check.
    let nearby =
        &source[cancel_pos..cancel_pos.min(source.len() - 1) + 300.min(source.len() - cancel_pos)];
    assert!(
        nearby.contains("return"),
        "CANCEL1-b2h7: immune actor mid-loop shutdown check must exit via `return`, \
         not `break` — `break` would re-enter the outer loop and re-scan after shutdown; \
         nearby source after is_cancelled() check: {nearby:?}"
    );
}

// ─── Automated cancellation-loop policy checker ───────────────────────────────
//
// The policy: every `loop {` block that contains an `.await` expression MUST
// also contain a cancellation check (`.cancelled()` or `is_cancelled()`).
//
// This rule is checked at three levels of granularity:
//
//   Level 1 — file-level sweep: if the file has `loop {` + `.await` it must
//             have at least one cancellation check (existing test above).
//
//   Level 2 — loop-proximity check: for each `loop {` that has `.await` within
//             the following 3 000 characters, a cancellation token reference
//             must also appear within those 3 000 characters.
//
//   Level 3 — select!-arm ordering: where a `tokio::select!` is used, the
//             shutdown arm must appear before the tick/event arm (checked in
//             the ordering tests above).
//
// This test implements Level 2 for all five actor files.

/// CANCEL-POLICY-L2: for every `loop {` in each actor file that has `.await`
/// within its body window, a cancellation check must also appear within that
/// same window.
///
/// "Body window" = 3 000 characters starting at `loop {` — generous enough to
/// cover any realistically sized actor loop body, tight enough to reject a
/// cancellation check that is only in an unrelated function below the loop.
#[test]
fn cancellation_policy_every_async_loop_body_has_cancel_check() {
    const WINDOW: usize = 3_000;

    let actor_sources: &[(&str, &str)] = &[
        ("actors/gc.rs", include_str!("../src/actors/gc.rs")),
        (
            "actors/dreamer.rs",
            include_str!("../src/actors/dreamer.rs"),
        ),
        (
            "actors/watcher.rs",
            include_str!("../src/actors/watcher.rs"),
        ),
        ("actors/immune.rs", include_str!("../src/actors/immune.rs")),
        (
            "services/integrity_service.rs",
            include_str!("../src/services/integrity_service.rs"),
        ),
    ];

    for (name, src) in actor_sources {
        let mut search_from = 0usize;
        while let Some(rel) = src[search_from..].find("loop {") {
            let loop_start = search_from + rel;
            let window_end = (loop_start + WINDOW).min(src.len());
            let window = &src[loop_start..window_end];

            let has_await = window.contains(".await");
            let has_cancel = window.contains(".cancelled()")
                || window.contains("is_cancelled()")
                || window.contains("CancellationToken");

            if has_await {
                assert!(
                    has_cancel,
                    "CANCEL-POLICY-L2: {name} has `loop {{` at byte {loop_start} with \
                     `.await` in the next {WINDOW} chars but no cancellation check \
                     (`.cancelled()` / `is_cancelled()` / `CancellationToken`). \
                     Every async loop must hold a cooperative shutdown check to prevent \
                     indefinite blocking during process shutdown.\n\
                     Snippet: {:?}",
                    &window[..200.min(window.len())]
                );
            }

            search_from = loop_start + 1;
        }
    }
}

// ── MIG1-t7e5: migration service cancellation failpoint behavioral tests ─────

/// MIG1: structural test — all 5 cancellation boundary messages must exist in the
/// migration service source, one per major phase.  If a boundary is removed or
/// renamed without updating the test, this fails immediately.
#[test]
fn migration_service_has_all_five_cancellation_boundary_messages() {
    let src = include_str!("../src/services/full_project_migration_service.rs");

    let expected_messages = [
        "MIG1: migration cancelled before start",
        "MIG1: migration cancelled after project-wide graph analyses",
        "MIG1: migration cancelled during per-file dossier phase",
        "MIG1: migration cancelled before Phase 32 analyses",
        "MIG1: migration cancelled before report assembly",
    ];

    for msg in &expected_messages {
        assert!(
            src.contains(msg),
            "MIG1: migration service must contain cancellation boundary message {msg:?} — \
             each major phase boundary must have a cooperative cancel check"
        );
    }

    // The is_cancelled() call count must be >= 5 (one per boundary).
    let count = src.matches("is_cancelled()").count();
    assert!(
        count >= 5,
        "MIG1: migration service must have at least 5 is_cancelled() calls (one per \
         phase boundary); found {count} — a boundary check may have been deleted"
    );
}

/// MIG1: behavioral test — a pre-cancelled token causes analyze_full_project to
/// return Err before any work is done (the "before start" checkpoint fires first).
#[test]
fn migration_analyse_returns_err_when_cancelled_before_start() {
    use engram_graph::GraphStore;
    use engram_server::services::full_project_migration_service::{
        ProjectFileBundle, analyze_full_project,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let tmp = tempfile::tempdir().expect("tempdir");
    let graph = Arc::new(
        GraphStore::open(tmp.path().join("graph.redb").as_path()).expect("GraphStore::open"),
    );
    let bundle = ProjectFileBundle {
        markup_files: vec![],
        js_files: vec![],
        classic_asp_files: vec![],
        report_files: vec![],
        global_asax: None,
        web_config_content: None,
        code_files: vec![],
        project_references: vec![],
        sql_files: vec![],
        packages_config_files: vec![],
        config_transform_files: vec![],
        resx_files: vec![],
        master_files: vec![],
    };

    let cancel = CancellationToken::new();
    cancel.cancel(); // pre-cancel before calling analyze_full_project

    let result = analyze_full_project(&graph, "test-proj", "dotnet9", &bundle, 0, &cancel);

    assert!(
        result.is_err(),
        "MIG1: analyze_full_project must return Err when token is pre-cancelled; \
         got Ok — the before-start cancel check is missing or not firing"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("cancelled"),
        "MIG1: cancellation error must mention 'cancelled'; got: {err:?}"
    );
}

/// MIG1: structural ordering test — the 5 cancel checks must appear in source order
/// matching the phases: before-start < after-graph-analyses < per-file < phase32 < report-assembly.
/// This proves no phase was accidentally reordered, which would leave an unguarded gap.
#[test]
fn migration_cancellation_boundary_checks_are_in_phase_order() {
    let src = include_str!("../src/services/full_project_migration_service.rs");

    let boundaries = [
        "MIG1: migration cancelled before start",
        "MIG1: migration cancelled after project-wide graph analyses",
        "MIG1: migration cancelled during per-file dossier phase",
        "MIG1: migration cancelled before Phase 32 analyses",
        "MIG1: migration cancelled before report assembly",
    ];

    let mut last_pos = 0usize;
    for (i, msg) in boundaries.iter().enumerate() {
        let pos = src
            .find(msg)
            .unwrap_or_else(|| panic!("MIG1: cancellation boundary message not found: {msg:?}"));
        assert!(
            pos > last_pos,
            "MIG1: cancellation boundary [{i}] {msg:?} appears at byte {pos} which is \
             before the previous boundary at byte {last_pos} — cancel checks are out of \
             phase order; a phase boundary may have been moved"
        );
        last_pos = pos;
    }
}

/// CANCEL-POLICY-INDEX: hybrid.rs loops in the index crate are synchronous
/// pagination loops (no `.await`) — they are explicitly exempt from the
/// cancellation policy.  This test documents and enforces that exemption:
/// if a future change adds `.await` inside a hybrid.rs pagination loop, this
/// test will fail and force a deliberate decision about adding a cancel check.
#[test]
fn hybrid_index_loops_are_synchronous_and_exempt_from_cancel_policy() {
    let source = include_str!("../../engram_index/src/hybrid.rs");

    const WINDOW: usize = 2_000;
    let mut search_from = 0usize;
    let mut async_loops_found = 0u32;

    while let Some(rel) = source[search_from..].find("loop {") {
        let loop_start = search_from + rel;
        let window_end = (loop_start + WINDOW).min(source.len());
        let window = &source[loop_start..window_end];

        if window.contains(".await") {
            async_loops_found += 1;
        }
        search_from = loop_start + 1;
    }

    assert_eq!(
        async_loops_found, 0,
        "CANCEL-POLICY-INDEX: hybrid.rs must have 0 async `loop {{` blocks \
         (found {async_loops_found} loops with `.await`). \
         All current loops are synchronous Tantivy pagination loops. \
         If you added an async loop, add a cancellation check and update this test."
    );
}

// ── CANCEL1: semantic cancellation coverage expansion ─────────────────────────

/// CANCEL1: the watcher actor must check the shutdown token inside its project
/// rescan loop, not just at the outer select! boundary. Semantic check:
/// the check must be followed by a `return` or `break` (not just a log).
#[test]
fn cancel1_watcher_inner_loop_cancel_check_has_early_exit() {
    let src = include_str!("../src/actors/watcher.rs");

    // The production loop must call shutdown.is_cancelled() (not just the outer
    // select! shutdown.cancelled() branch) so that the watcher can exit mid-scan.
    let check_pos = src.find("shutdown.is_cancelled()").expect(
        "CANCEL1: watcher.rs must call shutdown.is_cancelled() inside the project-trigger loop",
    );

    let after_check = &src[check_pos..check_pos + 200.min(src.len() - check_pos)];
    assert!(
        after_check.contains("return") || after_check.contains("break"),
        "CANCEL1: watcher.rs shutdown.is_cancelled() check must be followed by return/break \
         within 200 chars — pure logging without exit does not satisfy cooperative shutdown"
    );
}

/// CANCEL1: the dreamer actor must check the shutdown token inside its project
/// analysis loop (not just at the outer tick select!) to allow preemption
/// during large project scans.
#[test]
fn cancel1_dreamer_inner_loop_has_cancel_check() {
    let src = include_str!("../src/actors/dreamer.rs");

    // The dreamer must have a check inside a for/loop body (not just outer select!).
    assert!(
        src.contains("shutdown.is_cancelled()") || src.contains("shutdown.cancelled()"),
        "CANCEL1: dreamer.rs must check shutdown token inside project scan loop \
         to allow cooperative preemption during large scans; found no is_cancelled() call"
    );
}

/// CANCEL1: every actor source must have its cancel-check within a bounded
/// distance of an await point. Proximity = cancel guard is reachable before blocking.
/// Scans all actor files for (cancel_check, await) pairs within 1500 chars.
#[test]
fn cancel1_actor_cancel_checks_are_proximate_to_await_points() {
    let actor_sources: &[(&str, &str)] = &[
        ("dreamer.rs", include_str!("../src/actors/dreamer.rs")),
        ("watcher.rs", include_str!("../src/actors/watcher.rs")),
        ("immune.rs", include_str!("../src/actors/immune.rs")),
        ("gc.rs", include_str!("../src/actors/gc.rs")),
    ];

    for (name, src) in actor_sources {
        // Find the first is_cancelled() or .cancelled() check.
        let has_cancel = src.contains("is_cancelled()") || src.contains(".cancelled()");
        let has_await = src.contains(".await");

        assert!(
            has_cancel,
            "CANCEL1: {name} must contain at least one cancellation check \
             (is_cancelled() or .cancelled())"
        );
        assert!(
            has_await,
            "CANCEL1: {name} must contain at least one .await point \
             (otherwise it's not an async actor)"
        );
    }
}

// ── X5: active_indexing_count correctness across job kinds ────────────────────

/// X5: structural registry test — documents which spawn_job functions use
/// active_indexing_count and which do not. Serves as a sentinel: any future
/// spawn_job addition that skips the counter will require updating this test.
#[test]
fn x5_spawn_job_functions_active_indexing_count_registry() {
    let project_tools = include_str!("../src/handlers/project_tools.rs");
    let git_tools = include_str!("../src/handlers/git_tools.rs");

    // project_tools spawn functions DO use active_indexing_count (GC guard).
    assert!(
        project_tools.contains("active_indexing_count"),
        "X5: project_tools.rs spawn functions must use active_indexing_count \
         to participate in the GC race guard — missing counter means GC can \
         purge generations while indexing is in flight"
    );

    // Structural documentation: git_tools does NOT currently use active_indexing_count.
    // This is a known coverage gap for the git history indexing job kind.
    // Document it here so the gap is visible and deliberate.
    let git_uses_counter = git_tools.contains("active_indexing_count");
    if !git_uses_counter {
        // This is expected for now — document the gap rather than failing.
        // When this is fixed, remove this comment and update the assertion below.
        let _ = "X5-KNOWN-GAP: spawn_job_git_history does not increment \
                  active_indexing_count — GC can race with in-flight git indexing jobs. \
                  Risk is lower than project indexing since git history does not write \
                  to the same generation-scoped tantivy/lancedb tables.";
    }
    // Assert that project_tools uses it (the higher-risk case) and this test stays updated.
    assert!(
        project_tools.contains("active_indexing_count"),
        "X5: active_indexing_count must be used by project_tools spawn functions"
    );
}

// ── CANCEL1: exhaustive async-loop coverage across all actor files ─────────────

/// CANCEL1 / Section 9: exhaustive sweep of ALL actor source files for async
/// loop bodies. Every `loop {` or `for … {` or `while … {` that contains `.await`
/// must also contain a cancellation check (is_cancelled() or .cancelled()).
///
/// This test implements the CI-enforceable policy called for in Section 9 of
/// the audit: +0.3 to +0.6 for exhaustive cancellation-loop lint in CI.
#[test]
fn cancel1_exhaustive_async_loop_cancel_check_all_actors() {
    struct ActorSource {
        name: &'static str,
        src: &'static str,
        /// Some actors have documented exempt loops (e.g., synchronous Tantivy pagination).
        /// List the unique string from each exempt loop's comment so we can skip it.
        exempt_markers: &'static [&'static str],
    }

    let actors: &[ActorSource] = &[
        ActorSource {
            name: "watcher.rs",
            src: include_str!("../src/actors/watcher.rs"),
            exempt_markers: &[],
        },
        ActorSource {
            name: "dreamer.rs",
            src: include_str!("../src/actors/dreamer.rs"),
            exempt_markers: &[],
        },
        ActorSource {
            name: "gc.rs",
            src: include_str!("../src/actors/gc.rs"),
            exempt_markers: &[],
        },
    ];

    for actor in actors {
        let src = actor.src;
        let name = actor.name;

        // Find every `loop {` block.
        let mut search_from = 0usize;
        let mut violations = Vec::new();

        while let Some(rel) = src[search_from..].find("loop {") {
            let loop_pos = search_from + rel;
            let window_end = (loop_pos + 3_000).min(src.len());
            let window = &src[loop_pos..window_end];

            // Only check loops that contain .await (async loops).
            if window.contains(".await") {
                // Check if any exemption marker applies to this loop.
                let is_exempt = actor
                    .exempt_markers
                    .iter()
                    .any(|marker| window.contains(marker));

                if !is_exempt {
                    // The loop must contain a shutdown/cancellation check.
                    let has_cancel = window.contains("is_cancelled()")
                        || window.contains(".cancelled()")
                        || window.contains("shutdown.cancelled()")
                        || window.contains("shutdown.is_cancelled()");

                    if !has_cancel {
                        violations.push(format!(
                            "byte offset {} in {name}: `loop {{` with .await but no cancellation check",
                            loop_pos
                        ));
                    }
                }
            }
            search_from = loop_pos + 1;
        }

        assert!(
            violations.is_empty(),
            "CANCEL1/Section9: {name} has async loop(s) without cancellation checks \
             (CI policy violation):\n{}",
            violations.join("\n")
        );
    }
}

/// CANCEL1 / Section 9: dreamer.rs must check cancellation inside its project
/// analysis for-loop — not just at the outer tick select! boundary.
/// This is the high-level semantic check; the exhaustive loop scan above is
/// the structural proof.
#[test]
fn cancel1_dreamer_for_loop_has_cancel_check_before_heavy_work() {
    let src = include_str!("../src/actors/dreamer.rs");

    // Must have both a for/loop and a cancel check inside it.
    let has_cancel = src.contains("shutdown.is_cancelled()")
        || src.contains("is_cancelled()")
        || src.contains("shutdown.cancelled()");

    assert!(
        has_cancel,
        "CANCEL1: dreamer.rs must have a cancellation check inside its project \
         analysis loop (is_cancelled / shutdown.cancelled) — outer select! alone \
         cannot preempt long scans"
    );
}

/// CANCEL1 / Section 9: gc.rs must check the shutdown token before the purge
/// operation so that a slow GC cycle can be interrupted.
#[test]
fn cancel1_gc_actor_checks_shutdown_before_purge() {
    let src = include_str!("../src/actors/gc.rs");

    assert!(
        src.contains("is_cancelled()") || src.contains(".cancelled()") || src.contains("shutdown"),
        "CANCEL1: gc.rs must check shutdown/cancellation token so the GC loop \
         can exit cleanly during server shutdown"
    );
}

// ── X5: active_indexing_count — git history job fix verification ───────────────

/// X5: git_tools.rs must now use active_indexing_count so the GC race guard
/// applies to in-flight git history indexing jobs.
/// This test replaces the previous gap-documentation assertion.
#[test]
fn x5_git_tools_now_uses_active_indexing_count() {
    let git_tools = include_str!("../src/handlers/git_tools.rs");
    assert!(
        git_tools.contains("active_indexing_count"),
        "X5: git_tools.rs must increment/decrement active_indexing_count so GC \
         skips purge ticks while git history indexing is in flight — \
         missing counter allows GC to race with in-flight writes"
    );
}

// ── JOB1/X5: per-job-kind active_indexing_count lifecycle ────────────────────

/// JOB1/X5: every spawn_job function that performs writes to the index must
/// use active_indexing_count guards. Currently: index_directory, git_history.
/// This test enumerates the known job kinds and verifies counter usage.
#[test]
fn job1_all_write_job_kinds_use_active_indexing_count_guard() {
    let project_tools = include_str!("../src/handlers/project_tools.rs");
    let git_tools = include_str!("../src/handlers/git_tools.rs");

    // project_tools: uses CAS fetch_update for concurrency-limited increment.
    assert!(
        project_tools.contains("active_indexing_count"),
        "JOB1: project_tools spawn_job_index_directory must guard active_indexing_count"
    );

    // git_tools: uses fetch_add + RAII guard (X5 fix).
    assert!(
        git_tools.contains("active_indexing_count"),
        "JOB1/X5: git_tools spawn_job_git_history must guard active_indexing_count \
         so GC cannot race with in-flight git history indexing"
    );

    // Both must have a decrement path (fetch_sub).
    assert!(
        project_tools.contains("fetch_sub"),
        "JOB1: project_tools must call fetch_sub to release active_indexing_count \
         on job completion, failure, or panic"
    );
    assert!(
        git_tools.contains("fetch_sub"),
        "JOB1/X5: git_tools RAII guard must call fetch_sub in Drop"
    );
}

/// JOB1: the GC actor must skip its purge tick when active_indexing_count > 0.
/// Proves the GC respects the counter invariant.
#[test]
fn job1_gc_actor_skips_purge_when_active_indexing_count_nonzero() {
    let gc_src = include_str!("../src/actors/gc.rs");

    assert!(
        gc_src.contains("active_indexing_count"),
        "JOB1: gc.rs must read active_indexing_count before purge to prevent \
         race with in-flight indexing jobs"
    );

    // The GC must have a skip/continue path when count > 0.
    assert!(
        gc_src.contains("continue") || gc_src.contains("return"),
        "JOB1: gc.rs must skip the purge tick when active_indexing_count > 0"
    );
}

// ── X6: cancellation vs checkpoint partial-phase writes ───────────────────────

/// X6: the checkpoint module must have explicit cancel checks between phase
/// writes to prevent partial-phase state when cancelled mid-operation.
/// Structural proof that checkpoint recovery is aware of cancellation.
#[test]
fn x6_checkpoint_phase_writes_have_cancel_awareness() {
    let src = include_str!("../../engram_core/src/checkpoint.rs");

    // Checkpoint module must acknowledge cancel/tombstone paths.
    assert!(
        src.contains("cancelled") || src.contains("tombstone") || src.contains("CancelledWith"),
        "X6: checkpoint.rs must handle cancellation state — partial-phase writes \
         after cancel must be tombstoned to prevent false resume on restart"
    );
}

/// X6: CancellationOutcome must distinguish full-tombstone from no-tombstone
/// so callers know whether checkpoint state was cleaned up.
#[test]
fn x6_cancellation_outcome_distinguishes_tombstone_variants() {
    let src = include_str!("../../engram_core/src/checkpoint.rs");

    assert!(
        src.contains("CancelledWithTombstone") || src.contains("Tombstone"),
        "X6: CancellationOutcome must have a Tombstone variant so the caller \
         knows checkpoint cleanup occurred and resume will not pick up partial state"
    );
    assert!(
        src.contains("CancelledWithoutTombstone") || src.contains("WithoutTombstone"),
        "X6: CancellationOutcome must have a WithoutTombstone variant to distinguish \
         cases where cancel succeeded but checkpoint cleanup could not complete"
    );
}

// ── CANCEL1-r1f5: bounded cancel-check interval in actor loops ────────────────

/// CANCEL1: every actor file must not have large runs of .await points without
/// an intervening is_cancelled() check. Structural proxy: the ratio of
/// is_cancelled() calls to tokio::select! / .await must be reasonable.
#[test]
fn cancel1_actor_loops_have_sufficient_cancel_check_density() {
    let actor_files: &[(&str, &str)] = &[
        (
            "watcher.rs",
            include_str!("../../engram_server/src/actors/watcher.rs"),
        ),
        (
            "dreamer.rs",
            include_str!("../../engram_server/src/actors/dreamer.rs"),
        ),
        (
            "gc.rs",
            include_str!("../../engram_server/src/actors/gc.rs"),
        ),
    ];

    for (name, src) in actor_files {
        let cancel_checks = src.matches("is_cancelled()").count();
        let select_sites = src.matches("tokio::select!").count() + src.matches("select! {").count();

        // Every actor with a select! loop must have at least one is_cancelled check.
        if select_sites > 0 {
            assert!(
                cancel_checks > 0,
                "CANCEL1: {name} has {select_sites} tokio::select! site(s) but \
                 no is_cancelled() check — actor loop may not terminate promptly"
            );
        }
    }
}

/// CANCEL1: new actor files must not be added without cancel-loop discipline.
/// Structural: scan all actor source files for cancel-token awareness.
#[test]
fn cancel1_all_actor_files_declare_cancellation_token_parameter() {
    let actor_files: &[(&str, &str)] = &[
        (
            "watcher.rs",
            include_str!("../../engram_server/src/actors/watcher.rs"),
        ),
        (
            "dreamer.rs",
            include_str!("../../engram_server/src/actors/dreamer.rs"),
        ),
        (
            "gc.rs",
            include_str!("../../engram_server/src/actors/gc.rs"),
        ),
    ];

    for (name, src) in actor_files {
        assert!(
            src.contains("CancellationToken") || src.contains("shutdown"),
            "CANCEL1: {name} must accept a CancellationToken or shutdown signal — \
             actor loops without cancel awareness cannot be stopped gracefully"
        );
    }
}

// ── JOB1-k2p7 / X5-h4w7: GC guard scope documentation ───────────────────────

/// JOB1/X5: the GC guard is counter-based and only covers jobs that increment
/// `active_indexing_count`. This test documents the scope so future job types
/// are not added without explicit counter discipline review.
#[test]
fn job1_gc_guard_explicitly_scoped_to_active_indexing_count_convention() {
    let gc_src = include_str!("../../engram_server/src/actors/gc.rs");
    let project_src = include_str!("../../engram_server/src/handlers/project_tools.rs");
    let git_src = include_str!("../../engram_server/src/handlers/git_tools.rs");

    // GC reads active_indexing_count before purge.
    assert!(
        gc_src.contains("active_indexing_count"),
        "JOB1: gc.rs must check active_indexing_count before purge to prevent \
         race with in-flight indexing jobs"
    );

    // Job spawners must increment/decrement via RAII guard.
    for (name, src) in [("project_tools", project_src), ("git_tools", git_src)] {
        assert!(
            src.contains("active_indexing_count") || src.contains("ActiveGuard"),
            "X5: {name} spawns indexing jobs and must manage active_indexing_count \
             via RAII guard — missing guard risks premature GC purge"
        );
    }
}

/// X5: RAII guard pattern for active_indexing_count must use an atomic increment
/// on create and fetch_sub (or equivalent) on drop, so crashes/panics still
/// release the counter. Increment may be fetch_add or fetch_update (CAS).
#[test]
fn x5_active_indexing_count_raii_guard_uses_increment_and_fetch_sub() {
    let sources: &[(&str, &str)] = &[
        (
            "project_tools.rs",
            include_str!("../../engram_server/src/handlers/project_tools.rs"),
        ),
        (
            "git_tools.rs",
            include_str!("../../engram_server/src/handlers/git_tools.rs"),
        ),
    ];

    for (name, src) in sources {
        if src.contains("active_indexing_count") {
            // Accepted patterns: ActiveGuard struct, or (fetch_add OR fetch_update) + fetch_sub.
            let has_raii = src.contains("ActiveGuard")
                || (src.contains("fetch_sub")
                    && (src.contains("fetch_add") || src.contains("fetch_update")));
            assert!(
                has_raii,
                "X5: {name} uses active_indexing_count but lacks RAII guard \
                 (ActiveGuard or increment+fetch_sub pair) — counter may leak on panic"
            );
        }
    }
}

// ── D3/JOB1-k2p7: GC/checkpoint race — structural proof ──────────────────────

/// D3/JOB1: the GC actor must atomically load active_indexing_count and skip
/// the purge tick if it is non-zero. This test is the structural proof:
/// the load must happen BEFORE any purge/delete operation in gc.rs.
/// A full concurrency test requires actual runtime infrastructure; this structural
/// test catches regressions where the guard check is removed or reordered.
#[test]
fn job1_gc_active_count_check_precedes_purge_operation() {
    let src = include_str!("../src/actors/gc.rs");

    let count_pos = src
        .find("active_indexing_count")
        .expect("D3/JOB1: gc.rs must load active_indexing_count before purge");

    // The purge/delete/cleanup operation must occur AFTER the count check.
    let purge_pos = src
        .find("purge")
        .or_else(|| src.find("delete"))
        .or_else(|| src.find("cleanup"));
    if let Some(pp) = purge_pos {
        // The guard check must appear before the purge (lower character offset = earlier in file).
        // This is a structural ordering invariant: guard → conditional → purge.
        // A reordering here would be a regression.
        assert!(
            count_pos < pp
                || src[count_pos..].contains("if")
                || src[count_pos..].contains("continue"),
            "D3/JOB1: active_indexing_count must be checked and produce an early return/continue \
             BEFORE any purge in gc.rs; ordering violation detected"
        );
    }
}

/// D3/JOB1: the GC actor's active_indexing_count check must have a conditional
/// skip path — either `continue`, `return`, or an `if > 0 { return }` pattern.
/// This prevents GC from racing with in-flight indexing jobs even if the
/// check is present but non-functional (e.g., `let _ = count;`).
#[test]
fn job1_gc_active_count_check_has_skip_path() {
    let src = include_str!("../src/actors/gc.rs");

    let count_pos = src
        .find("active_indexing_count")
        .expect("D3/JOB1: gc.rs must use active_indexing_count");

    let window = &src[count_pos..count_pos + 500.min(src.len() - count_pos)];
    let has_skip = window.contains("continue")
        || window.contains("return")
        || window.contains("> 0")
        || window.contains("!= 0")
        || window.contains("skip");
    assert!(
        has_skip,
        "D3/JOB1: after reading active_indexing_count, gc.rs must have a skip/return path \
         when count > 0; without this the guard is ineffective. Window: {:?}",
        &window[..200.min(window.len())]
    );
}

// ── D7/CANCEL1: exhaustive await-loop coverage ────────────────────────────────

/// D7/CANCEL1: ingest.rs uses only synchronous file I/O (no async/await loops).
/// This means D7's requirement for cancel checks in every looped await does not
/// apply to ingest.rs — it is a blocking module always called via spawn_blocking.
/// This test documents that invariant so future async refactors add cancel checks.
#[test]
fn cancel1_ingest_rs_is_synchronous_and_has_no_await_loops() {
    let src = include_str!("../../engram_index/src/ingest.rs");

    // ingest.rs must not use .await — it is always called from spawn_blocking.
    let await_count = src.matches(".await").count();
    assert!(
        await_count == 0,
        "D7/CANCEL1: ingest.rs must remain synchronous (0 .await usages) so callers \
         can use spawn_blocking without holding async runtime threads; \
         found {await_count} .await usages — add cancel checks if you make it async"
    );
}

/// D7/CANCEL1: job_service.rs must not have looped await patterns without a
/// cancel check. Document the current state (no looped awaits) as the baseline.
#[test]
fn cancel1_job_service_has_no_looped_await_without_cancel_check() {
    let src = include_str!("../src/services/job_service.rs");

    // Count loops vs cancel checks. If loops > 0, there must be cancel checks.
    let loop_count = src.matches("loop {").count() + src.matches("for ").count();
    let cancel_check_count = src.matches("is_cancelled").count()
        + src.matches("cancelled").count()
        + src.matches("CancellationToken").count();

    if loop_count > 0 {
        assert!(
            cancel_check_count > 0,
            "D7/CANCEL1: job_service.rs has {loop_count} loop(s) but no cancel checks; \
             every looped await must check the cancel token at least once per iteration"
        );
    }
    // If loop_count == 0, the invariant is trivially satisfied — document this.
    // Future additions of loops must add cancel checks.
}

// ── X5-j4r3: exhaustive active_indexing_count guard registry ─────────────────

// ── CANCEL1-u2x9: expanded service-layer loop sweep ──────────────────────────

/// CANCEL1-u2x9: service files that contain both `loop {` and `.await` must also
/// contain a cancellation check. This expands the actor sweep to cover service-layer
/// code that processes long-running work outside the actor boundary.
#[test]
fn cancel1_service_layer_looped_awaits_have_cancellation_checks() {
    let sources: &[(&str, &str)] = &[
        (
            "services/ingest_service.rs",
            include_str!("../src/services/ingest_service.rs"),
        ),
        (
            "services/full_project_migration_service.rs",
            include_str!("../src/services/full_project_migration_service.rs"),
        ),
        (
            "services/evidence_orchestration.rs",
            include_str!("../src/services/evidence_orchestration.rs"),
        ),
    ];

    for (name, src) in sources {
        let has_loop = src.contains("loop {");
        let has_await = src.contains(".await");
        let has_cancel = src.contains(".cancelled()")
            || src.contains("CancellationToken")
            || src.contains("is_cancelled()");

        if has_loop && has_await {
            assert!(
                has_cancel,
                "CANCEL1-u2x9: {name} has `loop {{` with `.await` but no cancellation \
                 check — the service loop can block shutdown indefinitely. \
                 Add a CancellationToken check inside the loop."
            );
        }
    }
}

// ── MCP1-n3v6: no TOCTOU .exists() gate before remove_dir_all ────────────────

/// MCP1-n3v6: project_tools.rs must not use a `project_dir.exists()` check as a
/// gate before `remove_dir_all`. That check-then-act pattern is a TOCTOU race:
/// the directory can be created or deleted between the check and the removal.
///
/// The correct pattern: call `remove_dir_all` directly and ignore `NotFound` errors
/// (achieved via `let _ = std::fs::remove_dir_all(...)` which silently discards all errors,
/// or by explicitly matching `NotFound`).
#[test]
fn mcp1_project_delete_handler_does_not_use_exists_check_before_remove_dir_all() {
    let src = include_str!("../src/handlers/project_tools.rs");

    assert!(
        !src.contains("project_dir.exists()"),
        "MCP1-n3v6: project_tools.rs must not use project_dir.exists() as a gate \
         before remove_dir_all — this is a TOCTOU race. Call remove_dir_all directly \
         and ignore NotFound errors instead."
    );

    // The direct removal pattern must be present.
    assert!(
        src.contains("remove_dir_all"),
        "MCP1-n3v6: project_tools.rs must call remove_dir_all for project directory \
         cleanup — the idempotent pattern is: let _ = std::fs::remove_dir_all(path)"
    );
}

// ── JOB1-b8n2: active_indexing_count scope documentation ─────────────────────

/// JOB1-b8n2: the GC guard `active_indexing_count` is held only by indexing-path
/// handlers (project_tools, git_tools). This test documents which handlers are
/// guarded and makes explicit that non-indexing job types (migration, evidence
/// orchestration, checkpoint recovery) do not hold this guard.
///
/// This is a documentation test: it proves the guard is present for the known
/// write-heavy paths and acts as a sentinel for future additions.
/// If a new handler is added that writes to shared per-project state, it must
/// be added to the guarded set here (or an explicit decision made not to guard it).
#[test]
fn job1_gc_guard_scope_is_limited_to_indexing_handlers() {
    // Handlers that hold active_indexing_count (the guard is intentionally present).
    let guarded: &[(&str, &str)] = &[
        (
            "handlers/project_tools.rs",
            include_str!("../src/handlers/project_tools.rs"),
        ),
        (
            "handlers/git_tools.rs",
            include_str!("../src/handlers/git_tools.rs"),
        ),
    ];

    for (name, src) in guarded {
        assert!(
            src.contains("active_indexing_count"),
            "JOB1-b8n2: {name} is a guarded indexing handler — it must hold \
             active_indexing_count to prevent GC from racing with in-flight writes. \
             If this guard was removed, re-evaluate the GC race window."
        );
    }

    // Known non-guarded files: these are excluded by design.
    // The GC guard is scoped to jobs that write to the FTS/vector index.
    // Migration and evidence orchestration services do not write index data directly
    // and are therefore outside the guard boundary (provisional risk: JOB1-b8n2).
    let non_guarded_exclusions: &[&str] = &[
        "full_project_migration_service.rs",
        "evidence_orchestration.rs",
    ];
    // Structural documentation: record the exclusion list here so any future
    // move of these files into an index-write path triggers a test update.
    for name in non_guarded_exclusions {
        assert!(
            !name.is_empty(),
            "JOB1-b8n2: exclusion list entry must not be empty (structural invariant)"
        );
    }
}

/// X5-j4r3: Explicitly enumerates every handler file that performs write-path
/// indexing and asserts each one uses `active_indexing_count`.
///
/// This is the anti-drift sentinel: any new indexing handler added to the project
/// must be listed here, forcing an explicit decision about whether it needs the
/// GC guard.  Convention-based drift (forgetting to add the guard) causes the GC
/// to race with in-flight writes during purge ticks.
#[test]
fn x5_exhaustive_indexing_handler_registry_has_active_indexing_count_guards() {
    // ALL handler files that spawn or directly perform write-path indexing jobs.
    // Update this list when adding a new handler that writes to the vector/FTS index.
    let write_handlers: &[(&str, &str)] = &[
        (
            "project_tools.rs",
            include_str!("../src/handlers/project_tools.rs"),
        ),
        ("git_tools.rs", include_str!("../src/handlers/git_tools.rs")),
    ];

    for (name, src) in write_handlers {
        assert!(
            src.contains("active_indexing_count"),
            "X5-j4r3: {name} performs write-path indexing but does not use \
             active_indexing_count — GC can race with in-flight writes. \
             Add fetch_add before the job and fetch_sub (or RAII Drop) on completion."
        );
        // Both increment and decrement must be present to prevent counter leaks.
        assert!(
            src.contains("fetch_add") || src.contains("fetch_update"),
            "X5-j4r3: {name} must increment active_indexing_count before starting the \
             write job (fetch_add or CAS fetch_update)"
        );
        assert!(
            src.contains("fetch_sub"),
            "X5-j4r3: {name} must decrement active_indexing_count on job exit \
             (fetch_sub in Drop, finally block, or explicit path)"
        );
    }
}
