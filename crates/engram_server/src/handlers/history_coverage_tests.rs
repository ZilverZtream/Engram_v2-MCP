use super::{ArmCoverage, record_history_semantics};
use crate::services::change_card_service::SemanticOutcome;

#[test]
fn fallback_keeps_history_hits_and_prior_limits_without_a_complete_semantic_arm() {
    for outcome in [
        SemanticOutcome::TimedOut { budget_ms: 50 },
        SemanticOutcome::Failed {
            reason: "owned provider unavailable".into(),
        },
    ] {
        let mut arm = ArmCoverage::complete(3, 60);
        arm.note = "Two commit messages were not bounded file identities".into();
        record_history_semantics(&mut arm, &outcome, 50);
        assert_eq!(arm.status, "incomplete");
        assert_eq!((arm.hits, arm.ms), (3, 60));
        let details = arm.diagnostics.entries.join("\n");
        assert!(details.contains("Two commit messages"));
        assert!(details.contains("Semantic-only matches are unassessed"));
        assert!(!arm.diagnostics.details_complete);
        assert!(arm.diagnostics.omitted_occurrences.is_none());
    }
}

#[test]
fn requested_lexical_history_keeps_its_rows_and_discloses_its_ranking_scope() {
    for outcome in [
        SemanticOutcome::ZeroBudget,
        SemanticOutcome::VectorsDisabled,
    ] {
        let mut arm = ArmCoverage::complete(2, 10);
        record_history_semantics(&mut arm, &outcome, 0);
        assert_eq!(arm.status, "complete");
        assert_eq!(arm.hits, 2);
        assert!(
            arm.note
                .contains("does not establish equivalent semantic matches")
        );
        assert_eq!(arm.diagnostics.entries.len(), 1);
    }
}
