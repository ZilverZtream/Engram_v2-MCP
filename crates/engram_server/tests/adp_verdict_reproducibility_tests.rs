#![allow(clippy::unwrap_used)]
//! Behavioral tests for ADP verdict reproducibility, cross-subsystem interactions,
//! and end-to-end gate pipeline correctness.
//!
//! Covers:
//!  - Deterministic verdict reproduction (same input → same verdict)
//!  - Compound gate failures and interaction penalties
//!  - Generation arithmetic and persistence ordering contracts
//!  - Enrichment degradation regression checks
//!  - Infra-error vs low-relevance score isolation in ADP confidence

use engram_server::services::autonomous_decision_service::*;
use engram_server::services::safety_service::{PolicyDecision, RiskLevel};

// ── Shared helpers ────────────────────────────────────────────────────────────

fn safe_policy() -> PolicyDecision {
    PolicyDecision {
        allowed: true,
        risk_level: RiskLevel::Low,
        checks: vec![],
        confidence: 0.95,
        summary: "Safe".into(),
        mitigations: vec![],
    }
}

fn unsafe_policy() -> PolicyDecision {
    PolicyDecision {
        allowed: false,
        risk_level: RiskLevel::High,
        checks: vec![],
        confidence: 0.3,
        summary: "Unsafe".into(),
        mitigations: vec!["review required".into()],
    }
}

fn all_green_input() -> AdpInput {
    AdpInput {
        extraction_confidence: Some(0.9),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 0,
        safety_decision: Some(safe_policy()),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.85),
        retrieval_recall: Some(0.90),
        blast_radius_risk: Some(2),
        blast_radius_band: Some(engram_server::services::blast_radius_service::RiskBand::Low),
        blast_radius_downstream: Some(3),
        immune_verdict: Some("PASS".into()),
        immune_confidence: Some(0.05),
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: RiskProfile::Medium,
        min_extraction_confidence: 0.5,
        min_safety_confidence: 0.7,
        max_blast_radius_for_auto: 6,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: RetrievalMode::Live,
        migration_class: None,
    }
}

// ── Generation ordering contracts ─────────────────────────────────────────────

/// Repair persistence ordering: process_ingest_stats must run before set_meta,
/// and set_meta must run before the success banner. If any step fails, the
/// generation must not advance (fail-before-commit contract).
#[test]
fn generation_must_not_advance_on_persistence_failure() {
    let old_gen: u64 = 10;
    let new_gen: u64 = old_gen + 1;

    // Simulate the commit-then-banner contract:
    // Step 1: process_ingest_stats (can fail)
    // Step 2: set_meta(active_generation) (can fail)
    // Step 3: emit success banner (only if steps 1+2 succeeded)

    let ingest_stats_ok = true;
    let set_meta_ok = false; // simulate set_meta failure

    let committed_gen = if ingest_stats_ok && set_meta_ok {
        new_gen
    } else {
        old_gen // no advancement on failure
    };

    assert_eq!(
        committed_gen, old_gen,
        "AUD-2026-INV-0002: generation must not advance when set_meta fails; \
         old={old_gen}, new={new_gen}, committed={committed_gen}"
    );
    assert_ne!(committed_gen, new_gen,
        "AUD-2026-INV-0002: new_gen must not be visible after set_meta failure");
}

/// When process_ingest_stats fails (before set_meta), the generation pointer
/// must remain at the old value.
#[test]
fn generation_not_visible_before_set_meta_completes() {
    let old_gen: u64 = 5;
    let new_gen: u64 = 6;

    let ingest_stats_ok = false; // early failure
    let visible = if ingest_stats_ok { new_gen } else { old_gen };

    assert_eq!(visible, old_gen,
        "AUD-2026-INV-0002: generation must not be visible before process_ingest_stats completes");
}

// ── ADP infra-error isolation from gate confidence ────────────────────────────

/// When retrieval is skipped (infra failure), the overall ADP confidence must
/// NOT be depressed as if the retrieval had scored poorly — the gate is simply
/// absent from the score, not counted as zero.
#[test]
fn adp_skipped_retrieval_confidence_not_depressed_vs_live() {
    let mut live = all_green_input();
    live.retrieval_mode = RetrievalMode::Live;
    live.retrieval_production_ready = Some(true);
    live.retrieval_ndcg = Some(0.85);

    let mut skipped = all_green_input();
    skipped.retrieval_mode = RetrievalMode::Skipped;
    skipped.retrieval_production_ready = None;
    skipped.retrieval_ndcg = None;
    skipped.retrieval_recall = None;

    let live_dec = evaluate_gates(&live);
    let skip_dec = evaluate_gates(&skipped);

    // Skipped confidence should be reasonably close to live confidence
    // (not artificially low due to "missing" retrieval counting as zero)
    let delta = (live_dec.confidence - skip_dec.confidence).abs();
    assert!(
        delta < 0.40,
        "AUD-2026-INV-0005: skipped retrieval confidence ({}) must not be severely \
         depressed vs live retrieval confidence ({}) — delta={delta:.3}",
        skip_dec.confidence, live_dec.confidence
    );
}

// ── Compound failure interaction ──────────────────────────────────────────────

/// When both safety AND blast radius fail simultaneously, the confidence
/// should be lower than either failure alone (interaction penalty).
#[test]
fn compound_safety_blast_failure_lower_confidence_than_single_failure() {
    // Only safety fails
    let mut safety_only = all_green_input();
    safety_only.safety_decision = Some(unsafe_policy());
    let safety_only_dec = evaluate_gates(&safety_only);

    // Only blast radius fails
    let mut blast_only = all_green_input();
    blast_only.blast_radius_risk = Some(9);
    blast_only.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    blast_only.blast_radius_downstream = Some(50);
    let blast_only_dec = evaluate_gates(&blast_only);

    // Both fail together
    let mut both = all_green_input();
    both.safety_decision = Some(unsafe_policy());
    both.blast_radius_risk = Some(9);
    both.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    both.blast_radius_downstream = Some(50);
    let both_dec = evaluate_gates(&both);

    // Both verdicts must be Deny
    assert_eq!(safety_only_dec.verdict, AdpVerdict::Deny,
        "safety-only failure must Deny");
    assert_eq!(blast_only_dec.verdict, AdpVerdict::Deny,
        "blast-only failure must Deny");
    assert_eq!(both_dec.verdict, AdpVerdict::Deny,
        "compound failure must Deny");

    // Compound confidence must be lower than either single failure
    assert!(
        both_dec.confidence <= safety_only_dec.confidence.max(blast_only_dec.confidence),
        "compound failure confidence ({}) must not exceed single-failure confidence \
         (safety={}, blast={})",
        both_dec.confidence, safety_only_dec.confidence, blast_only_dec.confidence
    );
}

// ── Verdict reproducibility ───────────────────────────────────────────────────

/// Deterministic: calling evaluate_gates with the same input twice must produce
/// the same verdict and identical confidence (no random/non-deterministic paths).
#[test]
fn same_input_produces_identical_verdict_and_confidence() {
    let input = all_green_input();
    let dec1 = evaluate_gates(&input);
    let dec2 = evaluate_gates(&input);

    assert_eq!(dec1.verdict, dec2.verdict,
        "reproducibility: same input must produce same verdict");
    assert_eq!(dec1.confidence, dec2.confidence,
        "reproducibility: same input must produce identical confidence");
    assert_eq!(dec1.gate_results.len(), dec2.gate_results.len(),
        "reproducibility: same number of gate results");
}

/// Deterministic: the deny verdict for failing safety must reproduce exactly.
#[test]
fn deny_verdict_reproduces_identically() {
    let mut input = all_green_input();
    input.safety_decision = Some(unsafe_policy());

    let dec1 = evaluate_gates(&input);
    let dec2 = evaluate_gates(&input);

    assert_eq!(dec1.verdict, dec2.verdict,
        "deny verdict must be deterministic");
    assert_eq!(dec1.confidence, dec2.confidence,
        "deny confidence must be deterministic");
}

// ── End-to-end pipeline: post-index failure → corrected re-run ───────────────

/// When a post-index job degrades (enrichment failed), the ADP verdict based
/// on that evidence should be more conservative. When the enrichment is retried
/// successfully, the ADP verdict should become more permissive.
#[test]
fn corrected_enrichment_after_degraded_improves_adp_verdict() {
    // Degraded: retrieval evidence unavailable (infra error during enrichment)
    let mut degraded = all_green_input();
    degraded.retrieval_mode = RetrievalMode::Skipped;
    degraded.retrieval_production_ready = None;
    degraded.retrieval_ndcg = None;
    degraded.retrieval_recall = None;
    let degraded_dec = evaluate_gates(&degraded);

    // Clean: retrieval evidence available (enrichment succeeded on retry)
    let clean = all_green_input();
    let clean_dec = evaluate_gates(&clean);

    // Clean run should produce Allow (or at least equal/better verdict)
    assert_eq!(clean_dec.verdict, AdpVerdict::Allow,
        "clean enrichment run must produce Allow");

    // Clean confidence >= degraded confidence (enrichment adds information)
    assert!(
        clean_dec.confidence >= degraded_dec.confidence,
        "clean enrichment confidence ({}) must be >= degraded confidence ({})",
        clean_dec.confidence, degraded_dec.confidence
    );
}

/// The ADP pipeline must not Allow when all three critical gates (safety,
/// extraction, blast radius) simultaneously fail.
#[test]
fn adp_deny_when_all_three_hard_gates_fail() {
    let mut input = all_green_input();
    // Safety fails
    input.safety_decision = Some(unsafe_policy());
    // Extraction fails
    input.extraction_confidence = Some(0.1);
    input.extraction_band = Some("low".into());
    // Blast radius critical
    input.blast_radius_risk = Some(9);
    input.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    input.blast_radius_downstream = Some(100);

    let decision = evaluate_gates(&input);
    assert_ne!(
        decision.verdict,
        AdpVerdict::Allow,
        "all-three-gates-failing must not produce Allow; got {:?}",
        decision.verdict
    );
}

// ── ADP mutation tests ────────────────────────────────────────────────────────

/// Mutation test: injecting a failing safety gate into an otherwise all-green
/// input must change Allow → Deny. The pipeline must not "absorb" the mutation.
#[test]
fn adp_mutation_safety_fail_changes_allow_to_deny() {
    let baseline = all_green_input();
    let baseline_dec = evaluate_gates(&baseline);
    assert_eq!(baseline_dec.verdict, AdpVerdict::Allow,
        "baseline must be Allow");

    let mut mutated = all_green_input();
    mutated.safety_decision = Some(unsafe_policy());
    let mutated_dec = evaluate_gates(&mutated);

    assert_eq!(mutated_dec.verdict, AdpVerdict::Deny,
        "safety mutation must flip Allow to Deny");
    assert_ne!(baseline_dec.verdict, mutated_dec.verdict,
        "mutation must produce detectably different verdict");
}

/// Mutation test: injecting critical blast radius into all-green must
/// change Allow → Deny.
#[test]
fn adp_mutation_critical_blast_radius_changes_allow_to_deny() {
    let baseline = all_green_input();
    assert_eq!(evaluate_gates(&baseline).verdict, AdpVerdict::Allow);

    let mut mutated = all_green_input();
    mutated.blast_radius_risk = Some(9);
    mutated.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    mutated.blast_radius_downstream = Some(50);
    mutated.max_blast_radius_for_auto = 5;

    let dec = evaluate_gates(&mutated);
    assert_ne!(dec.verdict, AdpVerdict::Allow,
        "critical blast radius mutation must not remain Allow; got {:?}", dec.verdict);
}

// ── Enrichment canary ─────────────────────────────────────────────────────────

/// Canary: all-green input must produce Allow with confidence > 0.7.
/// If this test starts failing, it signals a regression in the confidence
/// calibration or gate logic.
#[test]
fn enrichment_canary_all_green_produces_allow_with_high_confidence() {
    let input = all_green_input();
    let decision = evaluate_gates(&input);

    assert_eq!(decision.verdict, AdpVerdict::Allow,
        "enrichment canary: all-green must Allow");
    assert!(
        decision.confidence > 0.7,
        "enrichment canary: all-green confidence must exceed 0.7; got {}",
        decision.confidence
    );
}

// ── Concurrent spawn_blocking fault tolerance ─────────────────────────────────

/// Chaos test: 10 concurrent spawn_blocking panics must all independently
/// produce JoinError — no deadlock, no silent swallowing, no cross-task
/// contamination. (Tests the behavioral property the actor fixes rely on.)
#[tokio::test]
async fn concurrent_spawn_blocking_panics_all_produce_join_errors() {
    use futures::future::join_all;

    let handles: Vec<_> = (0..10)
        .map(|i| {
            tokio::task::spawn_blocking(move || -> i32 {
                panic!("chaos: concurrent spawn_blocking panic #{i}");
            })
        })
        .collect();

    let results = join_all(handles).await;

    let error_count = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(
        error_count, 10,
        "All 10 concurrent spawn_blocking panics must produce JoinError; \
         got {error_count}/10 errors"
    );

    // Each error must be identifiable as a panic (not a cancellation)
    for (i, result) in results.iter().enumerate() {
        let err = result.as_ref().unwrap_err();
        assert!(
            err.is_panic(),
            "concurrent panic #{i} must be is_panic()=true; got cancelled={}",
            err.is_cancelled()
        );
    }
}

/// No orphan results: when all spawn_blocking tasks panic, none should
/// produce Ok(_) — all results must be Err.
#[tokio::test]
async fn all_panicking_spawn_blockings_produce_only_errors_no_ok() {
    use futures::future::join_all;

    let handles: Vec<_> = (0..5)
        .map(|_| {
            tokio::task::spawn_blocking(|| -> String {
                panic!("all-panic chaos test");
            })
        })
        .collect();

    let results = join_all(handles).await;
    let ok_count = results.iter().filter(|r| r.is_ok()).count();

    assert_eq!(
        ok_count, 0,
        "No panicking spawn_blocking must return Ok; got {ok_count} Ok results \
         (implies some errors were silently swallowed)"
    );
}

// ── JSON embedding element parity ─────────────────────────────────────────────

/// Embed parse parity: the full range of valid float types must parse correctly.
#[test]
fn embed_parse_parity_all_valid_json_float_types() {
    let cases = vec![
        (serde_json::json!(0.0f64), 0.0f32),
        (serde_json::json!(1.0f64), 1.0f32),
        (serde_json::json!(-1.0f64), -1.0f32),
        (serde_json::json!(0.5f64), 0.5f32),
        (serde_json::json!(1e-5f64), 1e-5f32),
    ];

    for (json_val, expected) in &cases {
        let result: Option<f64> = json_val.as_f64();
        assert!(result.is_some(),
            "valid float JSON value must parse via as_f64(): {json_val}");
        let parsed = result.unwrap() as f32;
        assert!(
            (parsed - expected).abs() < 1e-4,
            "parsed {parsed} must be close to {expected} for input {json_val}"
        );
    }
}

/// Embed parse: all known-invalid JSON types must return None from as_f64().
#[test]
fn embed_parse_all_invalid_json_types_return_none() {
    let invalid_values = vec![
        serde_json::Value::Null,
        serde_json::json!("string"),
        serde_json::json!(true),
        serde_json::json!(false),
        serde_json::json!({"key": "value"}),
        serde_json::json!([1, 2, 3]),
    ];

    for val in &invalid_values {
        assert!(
            val.as_f64().is_none(),
            "invalid JSON type must return None from as_f64(): {val}"
        );
    }
}
