//! Three independent deliveries plus their affected report/completeness consumers.
#[path = "detect_incomplete_changes_test.rs"]
mod completeness_consumers;
#[path = "dossier_source_recovery_tests.rs"]
mod dossier_recovery;
#[path = "edit_context_policy_delivery_tests.rs"]
mod editing_policy;
#[path = "explain_change_delivery_tests.rs"]
mod explanation_delivery;
#[path = "webforms_report_delivery_tests.rs"]
mod shared_delivery_consumers;
