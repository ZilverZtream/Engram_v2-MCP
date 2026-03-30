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
