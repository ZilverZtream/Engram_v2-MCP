//! Safety rails for automated edits — policy enforcement engine.
//!
//! Provides a policy gate that blocks high-risk refactors unless:
//! - Impact-analysis confidence meets the configured threshold
//! - Test coverage delta is acceptable
//! - Anti-pattern checks pass
//!
//! The policy engine is invoked by agents before executing automated refactoring
//! tools, giving them a go/no-go signal with detailed reasoning.

use engram_core::metrics;
use serde::{Deserialize, Serialize};

/// Policy decision for an automated edit operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// Whether the edit is allowed to proceed.
    pub allowed: bool,
    /// Risk level of the proposed edit.
    pub risk_level: RiskLevel,
    /// Individual check results.
    pub checks: Vec<PolicyCheck>,
    /// Overall confidence in the decision (0.0 – 1.0).
    pub confidence: f64,
    /// Human-readable summary of the decision.
    pub summary: String,
    /// Suggested mitigations if the edit is blocked.
    pub mitigations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheck {
    pub name: String,
    pub passed: bool,
    pub score: f64,
    pub threshold: f64,
    pub detail: String,
}

/// Input for a safety evaluation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyEvalRequest {
    /// Project being modified.
    pub project_id: String,
    /// Files affected by the proposed edit.
    pub affected_files: Vec<String>,
    /// Type of refactoring being performed.
    pub refactor_type: String,
    /// Impact analysis results (graph traversal depth, affected nodes).
    pub impact_node_count: u64,
    /// Confidence from impact analysis (0.0 – 1.0).
    pub impact_confidence: f64,
    /// Test coverage percentage of affected files (0.0 – 1.0, or -1.0 if unknown).
    pub test_coverage: f64,
    /// Anti-pattern guard verdict for affected files.
    pub anti_pattern_clear: bool,
    /// Number of downstream dependents (callers, importers).
    pub downstream_dependents: u64,
    /// Whether the edit touches shared/global state.
    pub touches_global_state: bool,
    /// Whether the edit modifies database schema or queries.
    pub touches_database: bool,
}

/// Evaluate a proposed automated edit against safety policy.
pub fn evaluate_safety(
    req: &SafetyEvalRequest,
    policy_enabled: bool,
    min_confidence: f64,
    min_coverage: f64,
) -> PolicyDecision {
    if !policy_enabled {
        metrics::metrics().refactors_approved.inc();
        return PolicyDecision {
            allowed: true,
            risk_level: RiskLevel::Low,
            checks: vec![],
            confidence: 1.0,
            summary: "Safety policy is disabled — edit allowed by default".into(),
            mitigations: vec![],
        };
    }

    let mut checks = Vec::new();
    let mut blocking = false;

    // Check 1: Impact analysis confidence
    let impact_passed = req.impact_confidence >= min_confidence;
    checks.push(PolicyCheck {
        name: "impact_confidence".into(),
        passed: impact_passed,
        score: req.impact_confidence,
        threshold: min_confidence,
        detail: format!(
            "Impact analysis confidence: {:.2} (threshold: {:.2})",
            req.impact_confidence, min_confidence
        ),
    });
    if !impact_passed {
        blocking = true;
    }

    // Check 2: Test coverage
    let coverage_known = req.test_coverage >= 0.0;
    let coverage_passed = !coverage_known || req.test_coverage >= min_coverage;
    checks.push(PolicyCheck {
        name: "test_coverage".into(),
        passed: coverage_passed,
        score: req.test_coverage.max(0.0),
        threshold: min_coverage,
        detail: if coverage_known {
            format!(
                "Test coverage of affected files: {:.1}% (threshold: {:.1}%)",
                req.test_coverage * 100.0,
                min_coverage * 100.0
            )
        } else {
            "Test coverage unknown — consider adding tests before refactoring".into()
        },
    });
    if !coverage_passed {
        blocking = true;
    }

    // Check 3: Anti-pattern guard
    checks.push(PolicyCheck {
        name: "anti_pattern_clear".into(),
        passed: req.anti_pattern_clear,
        score: if req.anti_pattern_clear { 1.0 } else { 0.0 },
        threshold: 1.0,
        detail: if req.anti_pattern_clear {
            "No known anti-patterns in affected files".into()
        } else {
            "Anti-pattern guard flagged issues in affected files".into()
        },
    });
    if !req.anti_pattern_clear {
        blocking = true;
    }

    // Check 4: Blast radius
    let blast_radius_ok = req.downstream_dependents <= 50;
    checks.push(PolicyCheck {
        name: "blast_radius".into(),
        passed: blast_radius_ok,
        score: if blast_radius_ok { 1.0 } else { 0.3 },
        threshold: 0.5,
        detail: format!(
            "Downstream dependents: {} (max safe: 50)",
            req.downstream_dependents
        ),
    });
    if !blast_radius_ok {
        blocking = true;
    }

    // Check 5: Global state / database safety
    let state_safe = !req.touches_global_state || req.impact_confidence >= 0.9;
    checks.push(PolicyCheck {
        name: "global_state_safety".into(),
        passed: state_safe,
        score: if state_safe { 1.0 } else { 0.2 },
        threshold: 0.5,
        detail: if req.touches_global_state {
            "Edit touches global state — requires high impact confidence".into()
        } else {
            "Edit does not touch global state".into()
        },
    });
    if !state_safe {
        blocking = true;
    }

    let db_safe =
        !req.touches_database || (req.impact_confidence >= 0.9 && req.test_coverage >= 0.8);
    checks.push(PolicyCheck {
        name: "database_safety".into(),
        passed: db_safe,
        score: if db_safe { 1.0 } else { 0.1 },
        threshold: 0.5,
        detail: if req.touches_database {
            "Edit touches database — requires high confidence AND coverage".into()
        } else {
            "Edit does not touch database queries".into()
        },
    });
    if !db_safe {
        blocking = true;
    }

    // Compute risk level
    let risk_level = compute_risk_level(req);

    // Compute overall confidence
    let passed_count = checks.iter().filter(|c| c.passed).count();
    let confidence = passed_count as f64 / checks.len() as f64;

    // Generate mitigations for failed checks
    let mut mitigations = Vec::new();
    for check in &checks {
        if !check.passed {
            mitigations.push(match check.name.as_str() {
                "impact_confidence" => {
                    "Run a deeper impact analysis with more graph hops to improve confidence".into()
                }
                "test_coverage" => {
                    "Add or verify tests for affected files before proceeding".into()
                }
                "anti_pattern_clear" => {
                    "Review and resolve flagged anti-patterns before refactoring".into()
                }
                "blast_radius" => "Break the refactoring into smaller, incremental changes".into(),
                "global_state_safety" => {
                    "Audit all global state access paths before modifying shared state".into()
                }
                "database_safety" => {
                    "Create database migration scripts and rollback procedures first".into()
                }
                _ => format!("Review and address: {}", check.detail),
            });
        }
    }

    let allowed = !blocking;
    let summary = if allowed {
        format!(
            "Edit approved: {}/{} checks passed (risk: {})",
            passed_count,
            checks.len(),
            risk_level
        )
    } else {
        format!(
            "Edit BLOCKED: {}/{} checks failed (risk: {}). {}",
            checks.len() - passed_count,
            checks.len(),
            risk_level,
            if !mitigations.is_empty() {
                "See mitigations below."
            } else {
                ""
            }
        )
    };

    if allowed {
        metrics::metrics().refactors_approved.inc();
    } else {
        metrics::metrics().refactors_blocked.inc();
    }

    PolicyDecision {
        allowed,
        risk_level,
        checks,
        confidence,
        summary,
        mitigations,
    }
}

fn compute_risk_level(req: &SafetyEvalRequest) -> RiskLevel {
    let mut score = 0u32;

    // File count risk
    score += match req.affected_files.len() {
        0..=3 => 0,
        4..=10 => 1,
        11..=30 => 2,
        _ => 3,
    };

    // Downstream dependents risk
    score += match req.downstream_dependents {
        0..=5 => 0,
        6..=20 => 1,
        21..=50 => 2,
        _ => 3,
    };

    // Global state / database risk
    if req.touches_global_state {
        score += 2;
    }
    if req.touches_database {
        score += 2;
    }

    // Low confidence increases risk
    if req.impact_confidence < 0.5 {
        score += 2;
    } else if req.impact_confidence < 0.7 {
        score += 1;
    }

    match score {
        0..=2 => RiskLevel::Low,
        3..=5 => RiskLevel::Medium,
        6..=8 => RiskLevel::High,
        _ => RiskLevel::Critical,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_safe_request() -> SafetyEvalRequest {
        SafetyEvalRequest {
            project_id: "test".into(),
            affected_files: vec!["a.rs".into()],
            refactor_type: "rename".into(),
            impact_node_count: 5,
            impact_confidence: 0.95,
            test_coverage: 0.85,
            anti_pattern_clear: true,
            downstream_dependents: 3,
            touches_global_state: false,
            touches_database: false,
        }
    }

    #[test]
    fn safe_edit_is_approved() {
        let req = make_safe_request();
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(decision.allowed);
        assert_eq!(decision.risk_level, RiskLevel::Low);
        assert!(decision.mitigations.is_empty());
    }

    #[test]
    fn low_confidence_blocks() {
        let mut req = make_safe_request();
        req.impact_confidence = 0.3;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(!decision.allowed);
        assert!(!decision.mitigations.is_empty());
    }

    #[test]
    fn database_touch_with_low_coverage_blocks() {
        let mut req = make_safe_request();
        req.touches_database = true;
        req.test_coverage = 0.5;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(!decision.allowed);
    }

    #[test]
    fn policy_disabled_always_approves() {
        let mut req = make_safe_request();
        req.impact_confidence = 0.0;
        req.test_coverage = 0.0;
        let decision = evaluate_safety(&req, false, 0.7, 0.6);
        assert!(decision.allowed);
    }

    #[test]
    fn high_blast_radius_blocks() {
        let mut req = make_safe_request();
        req.downstream_dependents = 100;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(!decision.allowed);
    }

    // ── False-allow/false-deny calibration tests (Ticket 6) ─────────────

    /// Labeled safety scenario for calibration.
    struct SafetyScenario {
        name: &'static str,
        request: SafetyEvalRequest,
        expected_allowed: bool,
        risk_class: &'static str,
    }

    fn calibration_corpus() -> Vec<SafetyScenario> {
        vec![
            // ── True-allow scenarios ──
            SafetyScenario {
                name: "safe_rename_single_file",
                request: SafetyEvalRequest {
                    project_id: "cal".into(),
                    affected_files: vec!["a.cs".into()],
                    refactor_type: "rename".into(),
                    impact_node_count: 3,
                    impact_confidence: 0.95,
                    test_coverage: 0.9,
                    anti_pattern_clear: true,
                    downstream_dependents: 2,
                    touches_global_state: false,
                    touches_database: false,
                },
                expected_allowed: true,
                risk_class: "low",
            },
            SafetyScenario {
                name: "safe_add_method_high_coverage",
                request: SafetyEvalRequest {
                    project_id: "cal".into(),
                    affected_files: vec!["svc.cs".into()],
                    refactor_type: "add_method".into(),
                    impact_node_count: 1,
                    impact_confidence: 0.99,
                    test_coverage: 0.95,
                    anti_pattern_clear: true,
                    downstream_dependents: 0,
                    touches_global_state: false,
                    touches_database: false,
                },
                expected_allowed: true,
                risk_class: "low",
            },
            // ── True-deny scenarios (high risk) ──
            SafetyScenario {
                name: "dangerous_global_state_low_confidence",
                request: SafetyEvalRequest {
                    project_id: "cal".into(),
                    affected_files: vec!["Global.asax.cs".into()],
                    refactor_type: "modify_state".into(),
                    impact_node_count: 50,
                    impact_confidence: 0.4,
                    test_coverage: 0.3,
                    anti_pattern_clear: false,
                    downstream_dependents: 80,
                    touches_global_state: true,
                    touches_database: true,
                },
                expected_allowed: false,
                risk_class: "high",
            },
            SafetyScenario {
                name: "database_migration_no_tests",
                request: SafetyEvalRequest {
                    project_id: "cal".into(),
                    affected_files: (0..15).map(|i| format!("file{i}.cs")).collect(),
                    refactor_type: "schema_migration".into(),
                    impact_node_count: 200,
                    impact_confidence: 0.6,
                    test_coverage: 0.2,
                    anti_pattern_clear: true,
                    downstream_dependents: 60,
                    touches_global_state: false,
                    touches_database: true,
                },
                expected_allowed: false,
                risk_class: "high",
            },
            SafetyScenario {
                name: "anti_pattern_flagged",
                request: SafetyEvalRequest {
                    project_id: "cal".into(),
                    affected_files: vec!["spaghetti.cs".into()],
                    refactor_type: "refactor".into(),
                    impact_node_count: 10,
                    impact_confidence: 0.85,
                    test_coverage: 0.7,
                    anti_pattern_clear: false,
                    downstream_dependents: 5,
                    touches_global_state: false,
                    touches_database: false,
                },
                expected_allowed: false,
                risk_class: "medium",
            },
            // ── Edge cases ──
            SafetyScenario {
                name: "borderline_confidence_passes",
                request: SafetyEvalRequest {
                    project_id: "cal".into(),
                    affected_files: vec!["b.cs".into()],
                    refactor_type: "rename".into(),
                    impact_node_count: 5,
                    impact_confidence: 0.7, // Exactly at threshold
                    test_coverage: 0.6,     // Exactly at threshold
                    anti_pattern_clear: true,
                    downstream_dependents: 10,
                    touches_global_state: false,
                    touches_database: false,
                },
                expected_allowed: true,
                risk_class: "low",
            },
            SafetyScenario {
                name: "borderline_confidence_fails",
                request: SafetyEvalRequest {
                    project_id: "cal".into(),
                    affected_files: vec!["c.cs".into()],
                    refactor_type: "rename".into(),
                    impact_node_count: 5,
                    impact_confidence: 0.69, // Just below threshold
                    test_coverage: 0.85,
                    anti_pattern_clear: true,
                    downstream_dependents: 5,
                    touches_global_state: false,
                    touches_database: false,
                },
                expected_allowed: false,
                risk_class: "medium",
            },
        ]
    }

    /// Confusion matrix for safety calibration.
    #[derive(Debug, Default)]
    struct SafetyConfusionMatrix {
        true_allow: usize,
        true_deny: usize,
        false_allow: usize,
        false_deny: usize,
    }

    #[test]
    fn safety_calibration_corpus_no_false_allows_on_high_risk() {
        let scenarios = calibration_corpus();
        let mut matrix = SafetyConfusionMatrix::default();

        for s in &scenarios {
            let decision = evaluate_safety(&s.request, true, 0.7, 0.6);
            match (s.expected_allowed, decision.allowed) {
                (true, true) => matrix.true_allow += 1,
                (false, false) => matrix.true_deny += 1,
                (false, true) => {
                    matrix.false_allow += 1;
                    // Force a read so the compiler recognises the counter has been
                    // updated (the rate calculation below is unreachable after panic,
                    // but keeping the increment makes future soft-failure refactors safe).
                    let _ = matrix.false_allow;
                    panic!(
                        "FALSE ALLOW on high-risk scenario '{}' (risk_class={}): {:?}",
                        s.name, s.risk_class, decision.summary
                    );
                }
                (true, false) => matrix.false_deny += 1,
            }
        }

        // Report
        let total = scenarios.len();
        let false_allow_rate = matrix.false_allow as f64 / total as f64;
        let false_deny_rate = matrix.false_deny as f64 / total as f64;

        assert!(
            false_allow_rate <= 0.01,
            "False-allow rate {:.2}% exceeds 1% threshold",
            false_allow_rate * 100.0
        );

        // False-deny is acceptable (conservative) but track it
        eprintln!(
            "Safety calibration: total={}, true_allow={}, true_deny={}, \
             false_allow={} ({:.1}%), false_deny={} ({:.1}%)",
            total,
            matrix.true_allow,
            matrix.true_deny,
            matrix.false_allow,
            false_allow_rate * 100.0,
            matrix.false_deny,
            false_deny_rate * 100.0,
        );
    }

    // ── RiskLevel display ────────────────────────────────────────────────────

    #[test]
    fn risk_level_display() {
        assert_eq!(RiskLevel::Low.to_string(), "low");
        assert_eq!(RiskLevel::Medium.to_string(), "medium");
        assert_eq!(RiskLevel::High.to_string(), "high");
        assert_eq!(RiskLevel::Critical.to_string(), "critical");
    }

    // ── compute_risk_level ───────────────────────────────────────────────────

    #[test]
    fn risk_level_low_for_simple_rename() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["a.cs".into()],
            refactor_type: "rename".into(),
            impact_node_count: 2,
            impact_confidence: 0.95,
            test_coverage: 0.9,
            anti_pattern_clear: true,
            downstream_dependents: 2,
            touches_global_state: false,
            touches_database: false,
        };
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert_eq!(decision.risk_level, RiskLevel::Low);
    }

    #[test]
    fn risk_level_increases_with_many_files() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: (0..35).map(|i| format!("file{i}.cs")).collect(),
            refactor_type: "refactor".into(),
            impact_node_count: 100,
            impact_confidence: 0.95,
            test_coverage: 0.9,
            anti_pattern_clear: true,
            downstream_dependents: 3,
            touches_global_state: false,
            touches_database: false,
        };
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        // 35 files → file-count score = 3; downstream 3 → 0; total = 3 → Medium
        assert!(
            matches!(decision.risk_level, RiskLevel::Medium | RiskLevel::High | RiskLevel::Critical),
            "many files should raise risk level, got {:?}", decision.risk_level
        );
    }

    #[test]
    fn risk_level_high_when_global_state_and_database() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["Global.asax".into()],
            refactor_type: "modify_state".into(),
            impact_node_count: 50,
            impact_confidence: 0.95,
            test_coverage: 0.95,
            anti_pattern_clear: true,
            downstream_dependents: 3,
            touches_global_state: true,
            touches_database: true,
        };
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        // global state +2, database +2 = at least 4 → Medium or higher
        assert!(
            matches!(decision.risk_level, RiskLevel::Medium | RiskLevel::High | RiskLevel::Critical),
            "global state + database should raise risk"
        );
    }

    #[test]
    fn risk_level_low_confidence_adds_risk() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["a.cs".into()],
            refactor_type: "rename".into(),
            impact_node_count: 5,
            impact_confidence: 0.40, // <0.5 → adds 2 points
            test_coverage: 0.9,
            anti_pattern_clear: true,
            downstream_dependents: 2,
            touches_global_state: false,
            touches_database: false,
        };
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        // low confidence only → 2 pts → Low (0..=2)
        assert!(matches!(decision.risk_level, RiskLevel::Low | RiskLevel::Medium));
    }

    // ── evaluate_safety: individual check pass/fail ──────────────────────────

    #[test]
    fn exactly_at_confidence_threshold_passes() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["a.cs".into()],
            refactor_type: "rename".into(),
            impact_node_count: 1,
            impact_confidence: 0.70, // exactly at threshold
            test_coverage: 0.60,
            anti_pattern_clear: true,
            downstream_dependents: 5,
            touches_global_state: false,
            touches_database: false,
        };
        let decision = evaluate_safety(&req, true, 0.70, 0.60);
        let conf_check = decision.checks.iter().find(|c| c.name == "impact_confidence").unwrap();
        assert!(conf_check.passed);
    }

    #[test]
    fn just_below_confidence_threshold_fails() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["a.cs".into()],
            refactor_type: "rename".into(),
            impact_node_count: 1,
            impact_confidence: 0.699,
            test_coverage: 0.9,
            anti_pattern_clear: true,
            downstream_dependents: 5,
            touches_global_state: false,
            touches_database: false,
        };
        let decision = evaluate_safety(&req, true, 0.70, 0.60);
        let conf_check = decision.checks.iter().find(|c| c.name == "impact_confidence").unwrap();
        assert!(!conf_check.passed);
        assert!(!decision.allowed);
    }

    #[test]
    fn unknown_coverage_negative_one_passes_coverage_check() {
        // test_coverage = -1.0 means unknown → coverage_known = false → passes
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["a.cs".into()],
            refactor_type: "rename".into(),
            impact_node_count: 1,
            impact_confidence: 0.95,
            test_coverage: -1.0,
            anti_pattern_clear: true,
            downstream_dependents: 5,
            touches_global_state: false,
            touches_database: false,
        };
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        let cov_check = decision.checks.iter().find(|c| c.name == "test_coverage").unwrap();
        assert!(cov_check.passed, "unknown coverage should pass the check");
    }

    #[test]
    fn exactly_50_downstream_passes_blast_radius() {
        let mut req = make_safe_request();
        req.downstream_dependents = 50;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        let blast = decision.checks.iter().find(|c| c.name == "blast_radius").unwrap();
        assert!(blast.passed, "exactly 50 dependents should pass (threshold is <= 50)");
    }

    #[test]
    fn fifty_one_downstream_fails_blast_radius() {
        let mut req = make_safe_request();
        req.downstream_dependents = 51;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        let blast = decision.checks.iter().find(|c| c.name == "blast_radius").unwrap();
        assert!(!blast.passed);
        assert!(!decision.allowed);
    }

    #[test]
    fn global_state_with_high_confidence_passes() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["a.cs".into()],
            refactor_type: "rename".into(),
            impact_node_count: 1,
            impact_confidence: 0.95, // >= 0.9
            test_coverage: 0.9,
            anti_pattern_clear: true,
            downstream_dependents: 3,
            touches_global_state: true, // but high confidence → safe
            touches_database: false,
        };
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        let state_check = decision.checks.iter().find(|c| c.name == "global_state_safety").unwrap();
        assert!(state_check.passed, "global state with 0.95 confidence should pass");
    }

    #[test]
    fn global_state_with_low_confidence_fails() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["a.cs".into()],
            refactor_type: "rename".into(),
            impact_node_count: 1,
            impact_confidence: 0.80, // <0.9 — not enough for global state
            test_coverage: 0.9,
            anti_pattern_clear: true,
            downstream_dependents: 3,
            touches_global_state: true,
            touches_database: false,
        };
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        let state_check = decision.checks.iter().find(|c| c.name == "global_state_safety").unwrap();
        assert!(!state_check.passed);
    }

    #[test]
    fn database_with_high_confidence_and_high_coverage_passes() {
        let req = SafetyEvalRequest {
            project_id: "p".into(),
            affected_files: vec!["Repo.cs".into()],
            refactor_type: "refactor".into(),
            impact_node_count: 1,
            impact_confidence: 0.95,
            test_coverage: 0.85, // >=0.8
            anti_pattern_clear: true,
            downstream_dependents: 3,
            touches_global_state: false,
            touches_database: true,
        };
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        let db_check = decision.checks.iter().find(|c| c.name == "database_safety").unwrap();
        assert!(db_check.passed, "db with high confidence AND coverage should pass");
    }

    // ── evaluate_safety: overall confidence score ───────────────────────────

    #[test]
    fn all_checks_pass_confidence_is_one() {
        let req = make_safe_request();
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(decision.allowed);
        assert!((decision.confidence - 1.0).abs() < 0.001, "all 6 checks pass → confidence = 1.0");
    }

    #[test]
    fn one_check_fails_confidence_is_five_sixths() {
        let mut req = make_safe_request();
        req.anti_pattern_clear = false; // 1 check fails out of 6
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(!decision.allowed);
        let expected = 5.0 / 6.0;
        assert!(
            (decision.confidence - expected).abs() < 0.01,
            "5/6 checks pass → confidence ~0.833, got {}", decision.confidence
        );
    }

    // ── summary message format ───────────────────────────────────────────────

    #[test]
    fn allowed_summary_says_approved() {
        let req = make_safe_request();
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(decision.summary.contains("approved") || decision.summary.contains("Edit approved"));
    }

    #[test]
    fn blocked_summary_says_blocked() {
        let mut req = make_safe_request();
        req.downstream_dependents = 100;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(decision.summary.contains("BLOCKED"));
    }

    // ── mitigation messages ──────────────────────────────────────────────────

    #[test]
    fn mitigation_for_blast_radius_mentions_incremental_changes() {
        let mut req = make_safe_request();
        req.downstream_dependents = 100;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(decision.mitigations.iter().any(|m| m.contains("incremental")));
    }

    #[test]
    fn mitigation_for_anti_pattern_mentions_resolve() {
        let mut req = make_safe_request();
        req.anti_pattern_clear = false;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(decision.mitigations.iter().any(|m| m.contains("anti-pattern")));
    }

    #[test]
    fn mitigation_for_low_coverage_mentions_tests() {
        let mut req = make_safe_request();
        req.test_coverage = 0.1;
        let decision = evaluate_safety(&req, true, 0.7, 0.6);
        assert!(decision.mitigations.iter().any(|m| m.to_lowercase().contains("test")));
    }

    #[test]
    fn policy_disabled_returns_empty_checks() {
        let req = make_safe_request();
        let decision = evaluate_safety(&req, false, 0.7, 0.6);
        assert!(decision.checks.is_empty(), "policy disabled → no checks run");
        assert_eq!(decision.confidence, 1.0);
        assert_eq!(decision.risk_level, RiskLevel::Low);
    }

    #[test]
    fn every_deny_has_mitigations() {
        let scenarios = calibration_corpus();
        for s in &scenarios {
            let decision = evaluate_safety(&s.request, true, 0.7, 0.6);
            if !decision.allowed {
                assert!(
                    !decision.mitigations.is_empty(),
                    "Deny verdict for '{}' must include mitigations",
                    s.name
                );
            }
        }
    }
}
