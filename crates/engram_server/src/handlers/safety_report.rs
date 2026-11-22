//! Public policy arithmetic must not imply source or runtime verification.
use rmcp::{
    ErrorData,
    model::{CallToolResult, Content},
};
use serde_json::{Value, json};

pub(crate) fn finish(
    mut result: CallToolResult,
    enabled: bool,
) -> Result<CallToolResult, ErrorData> {
    if result.is_error == Some(true) {
        return Ok(result);
    }
    let text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .ok_or_else(|| ErrorData::internal_error("Safety evaluation has no JSON report", None))?;
    let mut report: Value = serde_json::from_str(&text.text)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    report["input_basis"] = json!("caller_supplied_values_unverified");
    if enabled {
        report["assessment_status"] = json!("declared_policy_inputs_evaluated");
        report["confidence_basis"] =
            json!("fraction_of_policy_checks_passed_not_safety_probability");
        report["risk_basis"] = json!("heuristic_from_caller_supplied_scope");
        report["summary"] = json!(format!(
            "Policy threshold decision: {} on supplied values. Source and runtime safety are unverified.",
            if report["allowed"] == true {
                "allow"
            } else {
                "block"
            }
        ));
    } else {
        report["assessment_status"] = json!("policy_disabled_unassessed");
        report["policy_bypass_permits_edit"] = report["allowed"].clone();
        report["allowed"] = Value::Null;
        report["confidence"] = Value::Null;
        report["risk_level"] = Value::Null;
        report["confidence_basis"] = json!("not_assessed");
        report["risk_basis"] = json!("not_assessed");
        report["summary"] = json!(
            "Safety policy disabled. The configured bypass permits the edit; safety is unassessed."
        );
    }
    result.content[0] = Content::text(
        serde_json::to_string_pretty(&report)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?,
    );
    Ok(result)
}
