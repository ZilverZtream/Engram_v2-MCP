//! A configured bypass is distinct from an independently assessed edit.
use engram_core::Config;
use engram_server::{AppState, Engram, services::safety_service};
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

#[tokio::test]
async fn public_policy_report_distinguishes_bypass_and_declared_threshold_results() {
    let temp = tempfile::tempdir().unwrap();
    for (name, enabled, passing) in [
        ("bypass", false, false),
        ("pass", true, true),
        ("block", true, false),
    ] {
        let (state, _) = AppState::new(Config {
            data_dir: temp.path().join(name),
            allowed_roots: vec![temp.path().to_owned()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            safety_policy_enabled: enabled,
            safety_min_confidence: 0.7,
            safety_min_coverage: 0.6,
            ..Default::default()
        })
        .unwrap();
        let engram = Engram::new(state.clone());
        let request = json!({"project_id":"owned-safety","affected_files":["Entry.aspx.vb"],
            "refactor_type":"rename","impact_node_count":3,
            "impact_confidence":if passing {0.95} else {0.1},
            "test_coverage":if passing {0.9} else {-1.0},"anti_pattern_clear":passing,
            "downstream_dependents":if passing {2} else {100},
            "touches_global_state":!passing,"touches_database":!passing});
        let expected = serde_json::to_value(safety_service::evaluate_safety(
            &serde_json::from_value(request.clone()).unwrap(),
            enabled,
            0.7,
            0.6,
        ))
        .unwrap();
        let result = engram
            .evaluate_safety(Parameters(serde_json::from_value(request).unwrap()))
            .await
            .unwrap();
        let report: Value =
            serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(report["allowed"], !enabled || passing);
        for field in ["allowed", "checks", "mitigations"] {
            assert_eq!(report[field], expected[field]);
        }
        assert_eq!(report["input_basis"], "caller_supplied_values_unverified");
        if enabled {
            assert_eq!(
                report["assessment_status"],
                "declared_policy_inputs_evaluated"
            );
            assert_eq!(report["confidence"], expected["confidence"]);
            assert_eq!(report["risk_level"], expected["risk_level"]);
            assert_eq!(
                report["confidence_basis"],
                "fraction_of_policy_checks_passed_not_safety_probability"
            );
            assert_eq!(report["risk_basis"], "heuristic_from_caller_supplied_scope");
        } else {
            assert_eq!(report["assessment_status"], "policy_disabled_unassessed");
            assert!(report["confidence"].is_null() && report["risk_level"].is_null());
            assert!(report["checks"].as_array().unwrap().is_empty());
        }
        drop(engram);
        drop(state);
    }
    temp.close().unwrap();
}
