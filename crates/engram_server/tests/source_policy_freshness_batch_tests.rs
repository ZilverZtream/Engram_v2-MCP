//! Focused validation for three independent original workflow improvements.
#[path = "batch_source_read_tests.rs"]
mod batch_source;
#[path = "batch_read_tests.rs"]
mod original_batch_consumer;
#[path = "edit_context_policy_delivery_tests.rs"]
mod pre_edit_policy;
#[path = "webforms_report_delivery_tests.rs"]
mod shared_delivery_consumers;
#[path = "vb_instruction_body_freshness_tests.rs"]
mod vb_freshness;
