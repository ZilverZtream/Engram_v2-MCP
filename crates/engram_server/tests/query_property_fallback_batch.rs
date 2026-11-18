//! Focused acceptance for the queued query-budget, VB-evidence and availability outcomes.
#[path = "hybrid_query_fallback_tests.rs"]
mod hybrid_availability;
#[path = "vb_property_evidence_tests.rs"]
mod property_evidence;
#[path = "query_default_delivery.rs"]
mod query_delivery;
// Existing observed method ranges remain valid beside new property candidates.
#[path = "vb_instruction_body_freshness_tests.rs"]
mod method_freshness;
