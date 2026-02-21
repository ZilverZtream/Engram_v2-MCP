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
}
