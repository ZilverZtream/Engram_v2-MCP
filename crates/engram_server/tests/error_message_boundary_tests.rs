#![allow(clippy::unwrap_used)]
//! Errors must reach the caller with the message the enum defines.
//!
//! `EngramError::ProjectNotFound(String)` holds the project_id, and its
//! `#[error(...)]` attribute turns that into recovery guidance:
//!
//!   "Unknown project_id: '<id>'. Call list_projects to see indexed
//!    projects, or index_project to index a new directory."
//!
//! The `From<EngramError> for McpError` impl destructured the variant and
//! passed the INNER STRING as the message, so the agent received a bare
//! UUID with no explanation and no way to recover:
//!
//!   response error id=2 error=ErrorData { code: ErrorCode(-32602),
//!     message: "664003e4-2ac5-4902-a0ce-6382b6026fe5", data: None }
//!
//! 24 occurrences in the daemon log. The guidance existed the whole time;
//! the boundary threw it away.

use engram_server::error::EngramError;
use rmcp::ErrorData as McpError;

const PID: &str = "664003e4-2ac5-4902-a0ce-6382b6026fe5";

/// The variant that regressed: the id alone is not a message.
#[test]
fn project_not_found_keeps_its_guidance() {
    let err: McpError = EngramError::ProjectNotFound(PID.to_string()).into();

    assert!(
        err.message.contains(PID),
        "the message must still name the id; got {:?}",
        err.message
    );
    assert!(
        err.message.contains("list_projects"),
        "the message must tell the caller how to recover; got {:?}",
        err.message
    );
    assert_ne!(
        err.message.as_ref(),
        PID,
        "a bare project_id is not an error message"
    );
}

/// Every variant must render through Display, so a future variant with a
/// richer `#[error(...)]` cannot silently lose it the same way.
#[test]
fn every_variant_renders_through_display() {
    let cases: Vec<EngramError> = vec![
        EngramError::ProjectNotFound(PID.to_string()),
        EngramError::InvalidParams("bad thing".into()),
        EngramError::TooManyJobs(4),
        EngramError::Internal("boom".into()),
    ];

    for case in cases {
        let expected = case.to_string();
        let mcp: McpError = case.into();
        assert_eq!(
            mcp.message.as_ref(),
            expected,
            "the boundary must forward the enum's own Display output"
        );
    }
}

/// The JSON-RPC code still has to distinguish caller error from server
/// error — forwarding Display must not flatten everything to one code.
#[test]
fn error_codes_still_classify() {
    let not_found: McpError = EngramError::ProjectNotFound(PID.to_string()).into();
    let invalid: McpError = EngramError::InvalidParams("x".into()).into();
    let internal: McpError = EngramError::Internal("x".into()).into();

    assert_eq!(not_found.code, invalid.code, "both are caller errors");
    assert_ne!(
        internal.code, invalid.code,
        "a server fault must not be reported as invalid params"
    );
}
