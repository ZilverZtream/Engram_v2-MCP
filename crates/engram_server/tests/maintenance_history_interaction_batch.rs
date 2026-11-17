//! Focused combined acceptance for three independent original requirements.
#[path = "edit_session_maintenance_tests.rs"]
mod maintenance;
#[path = "history_header_filter_tests.rs"]
mod history;
#[path = "ask_webforms_interactions.rs"]
mod interactions;
// The request collector/composer changed; retain its HTTP and scoped behavior.
#[path = "ask_request_trace.rs"]
mod http;
