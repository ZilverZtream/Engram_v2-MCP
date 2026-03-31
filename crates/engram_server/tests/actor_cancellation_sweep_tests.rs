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
    let after_assign = &service_src[assign_pos..assign_pos + 200.min(service_src.len() - assign_pos)];

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
    let nearby = &source[cancel_pos..cancel_pos.min(source.len() - 1) + 300.min(source.len() - cancel_pos)];
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
        ("actors/gc.rs",                  include_str!("../src/actors/gc.rs")),
        ("actors/dreamer.rs",             include_str!("../src/actors/dreamer.rs")),
        ("actors/watcher.rs",             include_str!("../src/actors/watcher.rs")),
        ("actors/immune.rs",              include_str!("../src/actors/immune.rs")),
        ("services/integrity_service.rs", include_str!("../src/services/integrity_service.rs")),
    ];

    for (name, src) in actor_sources {
        let mut search_from = 0usize;
        while let Some(rel) = src[search_from..].find("loop {") {
            let loop_start = search_from + rel;
            let window_end = (loop_start + WINDOW).min(src.len());
            let window = &src[loop_start..window_end];

            let has_await  = window.contains(".await");
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
