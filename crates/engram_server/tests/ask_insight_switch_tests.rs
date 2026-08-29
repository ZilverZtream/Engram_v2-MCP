#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 — Dream row. The dreamer's only retrieval-visible
//! output is the `insights` namespace, read by ask_codebase's "insight" arm on
//! Explain-intent questions. An honest on/off ablation needs a switch that
//! removes exactly that arm and nothing else: `include_insights` on the
//! request (default on).

use std::collections::HashSet;

use engram_server::models::requests::AskCodebaseRequest;
use engram_server::services::ask_engine::plan::Intent;
use engram_server::services::ask_engine::retrieval::insight_arm_enabled;

#[test]
fn the_insight_arm_runs_for_explain_questions_when_insights_are_on() {
    let intents: HashSet<Intent> = [Intent::Explain].into_iter().collect();
    assert!(insight_arm_enabled(&intents, true));
}

#[test]
fn the_switch_removes_the_insight_arm_and_only_that() {
    let intents: HashSet<Intent> = [Intent::Explain].into_iter().collect();
    assert!(!insight_arm_enabled(&intents, false));
    // Non-explain questions never ran the arm; the switch does not add it.
    let usage: HashSet<Intent> = [Intent::Usage].into_iter().collect();
    assert!(!insight_arm_enabled(&usage, true));
}

#[test]
fn the_request_accepts_include_insights_and_defaults_to_on() {
    let r: AskCodebaseRequest = serde_json::from_str(
        r#"{"project_id":"p","question":"how does marker clustering work","include_insights":false}"#,
    )
    .unwrap();
    assert_eq!(r.include_insights, Some(false));
    let d: AskCodebaseRequest =
        serde_json::from_str(r#"{"project_id":"p","question":"how does marker clustering work"}"#)
            .unwrap();
    assert_eq!(
        d.include_insights, None,
        "absent = on (the default behaviour is unchanged)"
    );
}
