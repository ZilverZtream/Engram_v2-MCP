//! One focused native target for three independent original requirements.
#[path = "linked_policy_sources_tests.rs"]
mod current_link_regression;
#[path = "historical_linked_policy_tests.rs"]
mod historical_guidance;
#[path = "alter_foreign_key_schema_tests.rs"]
mod post_deployment_schema;
#[path = "retained_index_output_tests.rs"]
mod retained_output;
