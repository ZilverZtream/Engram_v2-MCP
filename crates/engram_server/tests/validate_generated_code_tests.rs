#![allow(clippy::unwrap_used)]
//! Round-5 P0: validate_generated_code green-lit a nonexistent file.
//!
//! Every check in the handler is guarded by `if !expected_X.is_empty()`, and
//! the verdict was `if has_fail {FAIL} else if has_warn {WARN} else {PASS}` —
//! so a call with no `expected_*` fields ran ZERO checks and defaulted to
//! PASS. A "post-generation safety net" that verifies nothing has not passed;
//! it is INSUFFICIENT. These pin the fail-closed verdict.

use engram_server::handlers::access_layer_tools::{ValidationCheck, compute_validation_verdict};

fn chk(status: &str) -> ValidationCheck {
    ValidationCheck {
        category: "t".into(),
        status: status.into(),
        details: vec![],
    }
}

#[test]
fn no_checks_is_insufficient_not_pass() {
    // The safety bug, exactly: zero checks ran => the old code returned PASS.
    assert_eq!(compute_validation_verdict(&[]), "INSUFFICIENT");
}

#[test]
fn a_fail_check_fails() {
    assert_eq!(compute_validation_verdict(&[chk("fail")]), "FAIL");
    assert_eq!(
        compute_validation_verdict(&[chk("pass"), chk("fail")]),
        "FAIL"
    );
}

#[test]
fn a_warn_without_fail_warns() {
    assert_eq!(compute_validation_verdict(&[chk("warn")]), "WARN");
    assert_eq!(
        compute_validation_verdict(&[chk("pass"), chk("warn")]),
        "WARN"
    );
}

#[test]
fn all_pass_passes() {
    assert_eq!(
        compute_validation_verdict(&[chk("pass"), chk("pass")]),
        "PASS"
    );
}
