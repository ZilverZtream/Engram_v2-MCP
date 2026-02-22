// Ticket 7: Validation Control Mapping Service
//
// Scans ASPX markup for ASP.NET validation controls (RequiredFieldValidator,
// CompareValidator, RangeValidator, RegularExpressionValidator, CustomValidator,
// ValidationSummary) and maps them to modern equivalents (FluentValidation,
// DataAnnotations, Blazor validation components).

use engram_graph::GraphStore;
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

// ── Result structs ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ValidationMap {
    pub file_path: String,
    pub validators: Vec<ValidatorMapping>,
    pub validation_groups: Vec<ValidationGroupInfo>,
    pub custom_validators: Vec<CustomValidatorInfo>,
    pub validation_summary: Option<ValidationSummaryInfo>,
    pub causes_validation_buttons: Vec<CausesValidationButton>,
    pub total_validators: usize,
    pub migration_complexity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatorMapping {
    pub validator_id: String,
    pub validator_type: String,
    pub control_to_validate: String,
    pub validation_group: String,
    pub error_message: String,
    pub display: String,
    pub set_focus_on_error: bool,
    pub text: String,
    // Type-specific properties
    pub compare_operator: Option<String>,
    pub value_to_compare: Option<String>,
    pub control_to_compare: Option<String>,
    pub compare_type: Option<String>,
    pub regex_pattern: Option<String>,
    pub min_value: Option<String>,
    pub max_value: Option<String>,
    pub range_type: Option<String>,
    // Modern mapping
    pub modern_data_annotation: String,
    pub modern_fluent_validation: String,
    pub modern_blazor: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationGroupInfo {
    pub group_name: String,
    pub validator_ids: Vec<String>,
    pub trigger_buttons: Vec<String>,
    pub validated_controls: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CustomValidatorInfo {
    pub validator_id: String,
    pub control_to_validate: String,
    pub server_validate_handler: Option<String>,
    pub client_validation_function: Option<String>,
    pub validate_empty_text: bool,
    pub validation_group: String,
    pub modern_approach: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationSummaryInfo {
    pub summary_id: String,
    pub validation_group: String,
    pub display_mode: String,
    pub show_summary: bool,
    pub show_message_box: bool,
    pub header_text: String,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CausesValidationButton {
    pub control_id: String,
    pub control_type: String,
    pub validation_group: String,
    pub causes_validation: bool,
}

// ── Regex patterns ────────────────────────────────────────────────────────

static RE_VALIDATOR_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:(RequiredFieldValidator|CompareValidator|RangeValidator|RegularExpressionValidator|CustomValidator)\b([^>]*?)(/\s*>|>.*?</asp:\1\s*>)")
        .unwrap()
});

static RE_VALIDATION_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:ValidationSummary\b([^>]*?)(/\s*>|>.*?</asp:ValidationSummary\s*>)")
        .unwrap()
});

static RE_BUTTON_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:(Button|LinkButton|ImageButton)\b([^>]*?)(/\s*>|>)").unwrap()
});

fn extract_attr(tag: &str, attr: &str) -> String {
    let pattern = format!(r#"(?i){}\s*=\s*"([^"]*)""#, regex::escape(attr));
    Regex::new(&pattern)
        .ok()
        .and_then(|re| re.captures(tag))
        .map(|c| c[1].to_string())
        .unwrap_or_default()
}

fn extract_attr_bool(tag: &str, attr: &str, default: bool) -> bool {
    let val = extract_attr(tag, attr);
    if val.is_empty() {
        return default;
    }
    val.eq_ignore_ascii_case("true")
}

// ── Main analysis function ────────────────────────────────────────────────

pub fn analyze_validation_controls(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    aspx_content: &str,
    codebehind_content: Option<&str>,
) -> anyhow::Result<ValidationMap> {
    let mut validators = Vec::new();
    let mut custom_validators = Vec::new();
    let mut validation_summary = None;
    let mut causes_validation_buttons = Vec::new();

    // ── Parse validator tags from ASPX markup ──

    for cap in RE_VALIDATOR_TAG.captures_iter(aspx_content) {
        let vtype = &cap[1];
        let full_tag = cap[0].as_ref();

        let id = extract_attr(full_tag, "ID");
        let ctv = extract_attr(full_tag, "ControlToValidate");
        let vgroup = extract_attr(full_tag, "ValidationGroup");
        let errmsg = extract_attr(full_tag, "ErrorMessage");
        let display = extract_attr(full_tag, "Display");
        let focus = extract_attr_bool(full_tag, "SetFocusOnError", false);
        let text = extract_attr(full_tag, "Text");

        // Type-specific properties
        let (compare_op, val_compare, ctrl_compare, compare_type) =
            if vtype.eq_ignore_ascii_case("CompareValidator") {
                (
                    Some(extract_attr(full_tag, "Operator")),
                    Some(extract_attr(full_tag, "ValueToCompare")),
                    Some(extract_attr(full_tag, "ControlToCompare")),
                    Some(extract_attr(full_tag, "Type")),
                )
            } else {
                (None, None, None, None)
            };

        let regex_pattern = if vtype.eq_ignore_ascii_case("RegularExpressionValidator") {
            Some(extract_attr(full_tag, "ValidationExpression"))
        } else {
            None
        };

        let (min_val, max_val, range_type) = if vtype.eq_ignore_ascii_case("RangeValidator") {
            (
                Some(extract_attr(full_tag, "MinimumValue")),
                Some(extract_attr(full_tag, "MaximumValue")),
                Some(extract_attr(full_tag, "Type")),
            )
        } else {
            (None, None, None)
        };

        // Modern mappings
        let (data_ann, fluent, blazor) = map_validator_to_modern(
            vtype,
            &errmsg,
            compare_op.as_deref(),
            val_compare.as_deref(),
            regex_pattern.as_deref(),
            min_val.as_deref(),
            max_val.as_deref(),
            range_type.as_deref(),
        );

        if vtype.eq_ignore_ascii_case("CustomValidator") {
            let server_handler = extract_attr(full_tag, "OnServerValidate");
            let client_fn = extract_attr(full_tag, "ClientValidationFunction");
            let validate_empty = extract_attr_bool(full_tag, "ValidateEmptyText", false);

            // Try to find handler body from event_wiring edges in graph
            let handler_name = if !server_handler.is_empty() {
                Some(server_handler.clone())
            } else {
                find_server_validate_handler(graph, project_id, &id)
            };

            let client_fn_opt: Option<&str> = if client_fn.is_empty() {
                None
            } else {
                Some(client_fn.as_str())
            };
            let modern_approach = build_custom_validator_approach(
                codebehind_content,
                handler_name.as_deref(),
                client_fn_opt,
            );

            custom_validators.push(CustomValidatorInfo {
                validator_id: id.clone(),
                control_to_validate: ctv.clone(),
                server_validate_handler: handler_name,
                client_validation_function: if client_fn.is_empty() {
                    None
                } else {
                    Some(client_fn)
                },
                validate_empty_text: validate_empty,
                validation_group: vgroup.clone(),
                modern_approach,
            });
        }

        validators.push(ValidatorMapping {
            validator_id: id,
            validator_type: vtype.to_string(),
            control_to_validate: ctv,
            validation_group: vgroup,
            error_message: errmsg,
            display: if display.is_empty() {
                "Static".to_string()
            } else {
                display
            },
            set_focus_on_error: focus,
            text,
            compare_operator: compare_op,
            value_to_compare: val_compare,
            control_to_compare: ctrl_compare,
            compare_type,
            regex_pattern,
            min_value: min_val,
            max_value: max_val,
            range_type,
            modern_data_annotation: data_ann,
            modern_fluent_validation: fluent,
            modern_blazor: blazor,
        });
    }

    // ── Parse ValidationSummary ──

    if let Some(cap) = RE_VALIDATION_SUMMARY.captures(aspx_content) {
        let full_tag = cap[0].as_ref();
        let id = extract_attr(full_tag, "ID");
        let vgroup = extract_attr(full_tag, "ValidationGroup");
        let display_mode = extract_attr(full_tag, "DisplayMode");
        let show_summary = extract_attr_bool(full_tag, "ShowSummary", true);
        let show_msgbox = extract_attr_bool(full_tag, "ShowMessageBox", false);
        let header = extract_attr(full_tag, "HeaderText");

        validation_summary = Some(ValidationSummaryInfo {
            summary_id: id,
            validation_group: vgroup,
            display_mode: if display_mode.is_empty() {
                "BulletList".to_string()
            } else {
                display_mode
            },
            show_summary,
            show_message_box: show_msgbox,
            header_text: header,
            modern_equivalent: "Blazor: <ValidationSummary /> or <DataAnnotationsValidator /> + custom error display | React: formik <ErrorMessage /> or react-hook-form errors object | FluentValidation: collect errors in ModelState".to_string(),
        });
    }

    // ── Parse buttons with CausesValidation ──

    for cap in RE_BUTTON_TAGS.captures_iter(aspx_content) {
        let btn_type = &cap[1];
        let full_tag = cap[0].as_ref();
        let btn_id = extract_attr(full_tag, "ID");
        let vgroup = extract_attr(full_tag, "ValidationGroup");
        let causes = extract_attr_bool(full_tag, "CausesValidation", true);

        causes_validation_buttons.push(CausesValidationButton {
            control_id: btn_id,
            control_type: btn_type.to_string(),
            validation_group: vgroup,
            causes_validation: causes,
        });
    }

    // ── Build validation groups ──

    let mut groups: HashMap<String, ValidationGroupInfo> = HashMap::new();
    for v in &validators {
        let group =
            groups
                .entry(v.validation_group.clone())
                .or_insert_with(|| ValidationGroupInfo {
                    group_name: v.validation_group.clone(),
                    validator_ids: Vec::new(),
                    trigger_buttons: Vec::new(),
                    validated_controls: Vec::new(),
                });
        group.validator_ids.push(v.validator_id.clone());
        if !v.control_to_validate.is_empty()
            && !group.validated_controls.contains(&v.control_to_validate)
        {
            group.validated_controls.push(v.control_to_validate.clone());
        }
    }

    for btn in &causes_validation_buttons {
        if btn.causes_validation {
            if let Some(g) = groups.get_mut(&btn.validation_group) {
                g.trigger_buttons.push(btn.control_id.clone());
            }
        }
    }

    let validation_groups: Vec<ValidationGroupInfo> = groups.into_values().collect();
    let total = validators.len();

    let migration_complexity = if total == 0 {
        "None: no validation controls found".to_string()
    } else if total <= 3 && custom_validators.is_empty() {
        "Low: few standard validators, straightforward DataAnnotation mapping".to_string()
    } else if total <= 10 && custom_validators.len() <= 1 {
        "Medium: moderate validation, consider FluentValidation for complex rules".to_string()
    } else {
        format!(
            "High: {} validators ({} custom) — recommend FluentValidation with dedicated validator classes",
            total,
            custom_validators.len()
        )
    };

    Ok(ValidationMap {
        file_path: file_path.to_string(),
        validators,
        validation_groups,
        custom_validators,
        validation_summary,
        causes_validation_buttons,
        total_validators: total,
        migration_complexity,
    })
}

// ── Modern mapping helpers ────────────────────────────────────────────────

fn map_validator_to_modern(
    vtype: &str,
    error_message: &str,
    compare_operator: Option<&str>,
    value_to_compare: Option<&str>,
    regex_pattern: Option<&str>,
    min_value: Option<&str>,
    max_value: Option<&str>,
    range_type: Option<&str>,
) -> (String, String, String) {
    let err = if error_message.is_empty() {
        "\"Validation error\""
    } else {
        error_message
    };

    match vtype.to_lowercase().as_str() {
        "requiredfieldvalidator" => (
            format!("[Required(ErrorMessage = \"{err}\")]"),
            format!("RuleFor(x => x.Field).NotEmpty().WithMessage(\"{err}\");"),
            "<InputText @bind-Value=\"Model.Field\" />\n<ValidationMessage For=\"@(() => Model.Field)\" />".to_string(),
        ),
        "comparevalidator" => {
            let op = compare_operator.unwrap_or("Equal");
            let val = value_to_compare.unwrap_or("");
            if !val.is_empty() {
                (
                    format!("[Compare(\"OtherField\", ErrorMessage = \"{err}\")] // Operator: {op}, Value: {val}"),
                    format!("RuleFor(x => x.Field).Equal(x => x.OtherField).WithMessage(\"{err}\"); // Operator: {op}"),
                    format!("// CompareValidator: compare against value '{val}' with operator '{op}'"),
                )
            } else {
                (
                    format!("[Compare(\"OtherField\", ErrorMessage = \"{err}\")]"),
                    format!("RuleFor(x => x.Field).Equal(x => x.OtherField).WithMessage(\"{err}\");"),
                    format!("// CompareValidator: match against another field, operator '{op}'"),
                )
            }
        }
        "rangevalidator" => {
            let min = min_value.unwrap_or("0");
            let max = max_value.unwrap_or("100");
            let rtype = range_type.unwrap_or("Integer");
            (
                format!("[Range({min}, {max}, ErrorMessage = \"{err}\")] // Type: {rtype}"),
                format!("RuleFor(x => x.Field).InclusiveBetween({min}, {max}).WithMessage(\"{err}\");"),
                format!("<InputNumber @bind-Value=\"Model.Field\" min=\"{min}\" max=\"{max}\" />\n<ValidationMessage For=\"@(() => Model.Field)\" />"),
            )
        }
        "regularexpressionvalidator" => {
            let pattern = regex_pattern.unwrap_or(".*");
            (
                format!("[RegularExpression(@\"{pattern}\", ErrorMessage = \"{err}\")]"),
                format!("RuleFor(x => x.Field).Matches(@\"{pattern}\").WithMessage(\"{err}\");"),
                format!("// RegexValidator pattern: {pattern}\n<InputText @bind-Value=\"Model.Field\" />\n<ValidationMessage For=\"@(() => Model.Field)\" />"),
            )
        }
        "customvalidator" => (
            format!("// CustomValidator: implement IValidatableObject or custom ValidationAttribute — ErrorMessage: {err}"),
            format!("RuleFor(x => x.Field).Must(CustomValidation).WithMessage(\"{err}\");"),
            "// CustomValidator: implement custom validation logic in component or FluentValidation".to_string(),
        ),
        _ => (
            format!("// Unknown validator type: {vtype}"),
            format!("// Unknown validator type: {vtype}"),
            format!("// Unknown validator type: {vtype}"),
        ),
    }
}

fn find_server_validate_handler(
    graph: &Arc<GraphStore>,
    project_id: &str,
    validator_id: &str,
) -> Option<String> {
    // Check graph for OnServerValidate event_wiring edge from this control
    use engram_graph::EdgeKind;
    let edges = graph
        .neighbors(project_id, EdgeKind::Dependency, validator_id, 10)
        .ok()?;
    // Also check event wiring through control → function edges
    for (target, _weight) in &edges {
        if target.contains("ServerValidate") || target.contains("_ServerValidate") {
            return Some(target.clone());
        }
    }
    None
}

fn build_custom_validator_approach(
    codebehind_content: Option<&str>,
    handler_name: Option<&str>,
    client_fn: Option<&str>,
) -> String {
    let mut parts = Vec::new();

    if let Some(handler) = handler_name {
        parts.push(format!("Server: Migrate {handler}() logic to FluentValidation .Must() predicate or IValidatableObject.Validate()"));

        // Try to find the handler body in code-behind
        if let Some(cb) = codebehind_content {
            if let Some(summary) = extract_handler_summary(cb, handler) {
                parts.push(format!("Handler logic: {summary}"));
            }
        }
    }

    if let Some(client) = client_fn {
        if !client.is_empty() {
            parts.push(format!(
                "Client: Migrate {client}() JavaScript function to Blazor validation component or React/Angular form validation"
            ));
        }
    }

    if parts.is_empty() {
        "Manual review required: no handler or client function detected".to_string()
    } else {
        parts.join(" | ")
    }
}

fn extract_handler_summary(codebehind: &str, handler_name: &str) -> Option<String> {
    // Find the handler method and extract a brief summary of what it does
    let pattern = format!(
        r"(?is)(?:Sub|void|Private\s+Sub|Protected\s+Sub)\s+{}\b[^)]*\).*?(?:End\s+Sub|\}})",
        regex::escape(handler_name)
    );
    let re = Regex::new(&pattern).ok()?;
    let m = re.find(codebehind)?;
    let body = m.as_str();

    // Extract key operations from the body
    let mut ops = Vec::new();
    if body.contains("Database") || body.contains("SqlCommand") || body.contains("SqlConnection") {
        ops.push("DB validation");
    }
    if body.contains("Regex") || body.contains("Match") {
        ops.push("regex check");
    }
    if body.contains("DateTime") || body.contains("Date.Parse") {
        ops.push("date validation");
    }
    if body.contains("Integer.TryParse")
        || body.contains("int.TryParse")
        || body.contains("Decimal.TryParse")
    {
        ops.push("numeric validation");
    }
    if body.contains(".IsValid") || body.contains("args.IsValid") {
        ops.push("sets args.IsValid");
    }

    if ops.is_empty() {
        Some("custom logic (review handler body)".to_string())
    } else {
        Some(ops.join(", "))
    }
}

// ── Format ────────────────────────────────────────────────────────────────

pub fn format_validation_map(report: &ValidationMap) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str(&format!(
        "## Validation Control Map: {}\n\n",
        report.file_path
    ));
    out.push_str(&format!(
        "**Total Validators:** {} | **Complexity:** {}\n\n",
        report.total_validators, report.migration_complexity
    ));

    if report.validators.is_empty() {
        out.push_str("No ASP.NET validation controls found in this file.\n");
        return out;
    }

    // Validators table
    out.push_str("### Validators\n\n");
    out.push_str("| ID | Type | Validates | Group | Error Message |\n");
    out.push_str("|---|---|---|---|---|\n");
    for v in &report.validators {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            v.validator_id,
            v.validator_type,
            v.control_to_validate,
            if v.validation_group.is_empty() {
                "(default)"
            } else {
                &v.validation_group
            },
            truncate_str(&v.error_message, 50)
        ));
    }

    // Modern mapping
    out.push_str("\n### Modern Equivalents\n\n");
    for v in &report.validators {
        out.push_str(&format!("**{}** ({})\n", v.validator_id, v.validator_type));
        out.push_str(&format!(
            "- DataAnnotation: `{}`\n",
            v.modern_data_annotation
        ));
        out.push_str(&format!(
            "- FluentValidation: `{}`\n",
            v.modern_fluent_validation
        ));
        out.push_str(&format!("- Blazor: `{}`\n\n", v.modern_blazor));
    }

    // Custom validators
    if !report.custom_validators.is_empty() {
        out.push_str("### Custom Validators (Require Manual Translation)\n\n");
        for cv in &report.custom_validators {
            out.push_str(&format!("**{}**\n", cv.validator_id));
            if let Some(ref handler) = cv.server_validate_handler {
                out.push_str(&format!("- Server handler: `{handler}`\n"));
            }
            if let Some(ref client) = cv.client_validation_function {
                out.push_str(&format!("- Client function: `{client}`\n"));
            }
            out.push_str(&format!("- Approach: {}\n\n", cv.modern_approach));
        }
    }

    // Validation groups
    if !report.validation_groups.is_empty() {
        out.push_str("### Validation Groups\n\n");
        for g in &report.validation_groups {
            let name = if g.group_name.is_empty() {
                "(default)"
            } else {
                &g.group_name
            };
            out.push_str(&format!(
                "- **{}**: {} validators, controls: [{}], triggers: [{}]\n",
                name,
                g.validator_ids.len(),
                g.validated_controls.join(", "),
                g.trigger_buttons.join(", ")
            ));
        }
    }

    // ValidationSummary
    if let Some(ref vs) = report.validation_summary {
        out.push_str(&format!("\n### ValidationSummary: {}\n", vs.summary_id));
        out.push_str(&format!(
            "- Display: {} | ShowSummary: {} | ShowMessageBox: {}\n",
            vs.display_mode, vs.show_summary, vs.show_message_box
        ));
        out.push_str(&format!("- Modern: {}\n", vs.modern_equivalent));
    }

    out
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_graph() -> Arc<GraphStore> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(GraphStore::open(dir.path()).unwrap())
    }

    #[test]
    fn test_required_field_validator() {
        let graph = make_graph();
        let aspx = r#"
            <asp:TextBox ID="txtName" runat="server" />
            <asp:RequiredFieldValidator ID="rfvName"
                ControlToValidate="txtName"
                ErrorMessage="Name is required"
                ValidationGroup="MainGroup"
                runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(result.total_validators, 1);
        assert_eq!(
            result.validators[0].validator_type,
            "RequiredFieldValidator"
        );
        assert_eq!(result.validators[0].control_to_validate, "txtName");
        assert_eq!(result.validators[0].validation_group, "MainGroup");
        assert_eq!(result.validators[0].error_message, "Name is required");
        assert!(
            result.validators[0]
                .modern_data_annotation
                .contains("[Required")
        );
        assert!(
            result.validators[0]
                .modern_fluent_validation
                .contains("NotEmpty")
        );
    }

    #[test]
    fn test_compare_validator() {
        let graph = make_graph();
        let aspx = r#"
            <asp:CompareValidator ID="cvPassword"
                ControlToValidate="txtConfirm"
                ControlToCompare="txtPassword"
                Operator="Equal"
                ErrorMessage="Passwords must match"
                runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(result.validators[0].validator_type, "CompareValidator");
        assert_eq!(
            result.validators[0].compare_operator.as_deref(),
            Some("Equal")
        );
        assert_eq!(
            result.validators[0].control_to_compare.as_deref(),
            Some("txtPassword")
        );
    }

    #[test]
    fn test_range_validator() {
        let graph = make_graph();
        let aspx = r#"
            <asp:RangeValidator ID="rvAge"
                ControlToValidate="txtAge"
                MinimumValue="18"
                MaximumValue="120"
                Type="Integer"
                ErrorMessage="Age must be 18-120"
                runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(result.validators[0].validator_type, "RangeValidator");
        assert_eq!(result.validators[0].min_value.as_deref(), Some("18"));
        assert_eq!(result.validators[0].max_value.as_deref(), Some("120"));
        assert!(
            result.validators[0]
                .modern_data_annotation
                .contains("[Range(18, 120")
        );
    }

    #[test]
    fn test_regex_validator() {
        let graph = make_graph();
        let aspx = r#"
            <asp:RegularExpressionValidator ID="revEmail"
                ControlToValidate="txtEmail"
                ValidationExpression="^[\w.-]+@[\w.-]+\.\w+$"
                ErrorMessage="Invalid email"
                runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(
            result.validators[0].validator_type,
            "RegularExpressionValidator"
        );
        assert!(result.validators[0].regex_pattern.is_some());
        assert!(
            result.validators[0]
                .modern_data_annotation
                .contains("[RegularExpression")
        );
    }

    #[test]
    fn test_custom_validator() {
        let graph = make_graph();
        let aspx = r#"
            <asp:CustomValidator ID="cvDate"
                ControlToValidate="txtDate"
                OnServerValidate="cvDate_ServerValidate"
                ClientValidationFunction="validateDate"
                ValidateEmptyText="true"
                ErrorMessage="Invalid date"
                runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(result.custom_validators.len(), 1);
        assert_eq!(
            result.custom_validators[0]
                .server_validate_handler
                .as_deref(),
            Some("cvDate_ServerValidate")
        );
        assert_eq!(
            result.custom_validators[0]
                .client_validation_function
                .as_deref(),
            Some("validateDate")
        );
        assert!(result.custom_validators[0].validate_empty_text);
    }

    #[test]
    fn test_validation_summary() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ValidationSummary ID="vsMain"
                ValidationGroup="MainGroup"
                DisplayMode="List"
                ShowSummary="true"
                ShowMessageBox="false"
                HeaderText="Please fix the following errors:"
                runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert!(result.validation_summary.is_some());
        let vs = result.validation_summary.unwrap();
        assert_eq!(vs.validation_group, "MainGroup");
        assert_eq!(vs.display_mode, "List");
    }

    #[test]
    fn test_validation_groups() {
        let graph = make_graph();
        let aspx = r#"
            <asp:RequiredFieldValidator ID="rfv1" ControlToValidate="txt1" ValidationGroup="GroupA" ErrorMessage="err1" runat="server" />
            <asp:RequiredFieldValidator ID="rfv2" ControlToValidate="txt2" ValidationGroup="GroupA" ErrorMessage="err2" runat="server" />
            <asp:RequiredFieldValidator ID="rfv3" ControlToValidate="txt3" ValidationGroup="GroupB" ErrorMessage="err3" runat="server" />
            <asp:Button ID="btnA" ValidationGroup="GroupA" runat="server" />
            <asp:Button ID="btnB" ValidationGroup="GroupB" CausesValidation="true" runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(result.total_validators, 3);
        assert_eq!(result.validation_groups.len(), 2);

        let group_a = result
            .validation_groups
            .iter()
            .find(|g| g.group_name == "GroupA")
            .unwrap();
        assert_eq!(group_a.validator_ids.len(), 2);
        assert_eq!(group_a.validated_controls.len(), 2);
    }

    #[test]
    fn test_multiple_validator_types() {
        let graph = make_graph();
        let aspx = r#"
            <asp:RequiredFieldValidator ID="rfv1" ControlToValidate="txtName" ErrorMessage="Required" runat="server" />
            <asp:CompareValidator ID="cv1" ControlToValidate="txtAge" Operator="GreaterThan" ValueToCompare="0" Type="Integer" ErrorMessage="Must be positive" runat="server" />
            <asp:RangeValidator ID="rv1" ControlToValidate="txtQty" MinimumValue="1" MaximumValue="999" Type="Integer" ErrorMessage="1-999" runat="server" />
            <asp:RegularExpressionValidator ID="rev1" ControlToValidate="txtEmail" ValidationExpression="\w+@\w+" ErrorMessage="Bad email" runat="server" />
            <asp:CustomValidator ID="cv2" ControlToValidate="txtDate" OnServerValidate="ValidateDate" ErrorMessage="Bad date" runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(result.total_validators, 5);
        assert_eq!(result.custom_validators.len(), 1);

        let types: Vec<&str> = result
            .validators
            .iter()
            .map(|v| v.validator_type.as_str())
            .collect();
        assert!(types.contains(&"RequiredFieldValidator"));
        assert!(types.contains(&"CompareValidator"));
        assert!(types.contains(&"RangeValidator"));
        assert!(types.contains(&"RegularExpressionValidator"));
        assert!(types.contains(&"CustomValidator"));
    }

    #[test]
    fn test_no_validators() {
        let graph = make_graph();
        let aspx = "<asp:TextBox ID=\"txt1\" runat=\"server\" />";
        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(result.total_validators, 0);
        assert!(result.migration_complexity.contains("None"));
    }

    #[test]
    fn test_causes_validation_false() {
        let graph = make_graph();
        let aspx = r#"
            <asp:RequiredFieldValidator ID="rfv1" ControlToValidate="txt1" ErrorMessage="err" runat="server" />
            <asp:Button ID="btnCancel" CausesValidation="false" runat="server" />
            <asp:Button ID="btnSubmit" CausesValidation="true" runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        assert_eq!(result.causes_validation_buttons.len(), 2);
        let cancel = result
            .causes_validation_buttons
            .iter()
            .find(|b| b.control_id == "btnCancel")
            .unwrap();
        assert!(!cancel.causes_validation);
    }

    #[test]
    fn test_format_output() {
        let graph = make_graph();
        let aspx = r#"
            <asp:RequiredFieldValidator ID="rfvName" ControlToValidate="txtName" ErrorMessage="Name required" runat="server" />
        "#;

        let result = analyze_validation_controls(&graph, "test", "Page.aspx", aspx, None).unwrap();
        let formatted = format_validation_map(&result);
        assert!(formatted.contains("Validation Control Map"));
        assert!(formatted.contains("rfvName"));
        assert!(formatted.contains("RequiredFieldValidator"));
        assert!(formatted.contains("Modern Equivalents"));
    }
}
