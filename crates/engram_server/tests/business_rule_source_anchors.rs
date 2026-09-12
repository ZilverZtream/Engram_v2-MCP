use engram_server::services::business_logic_service::{
    parse_llm_response, render_method_as_doc, validate_llm_output, validate_method_source_anchors,
};
use serde_json::json;

fn response(line: Option<u32>, reference: &str) -> String {
    json!({"purpose":"Validate invoice", "business_rules":[{
        "when":"invoice.Total < 0", "then":"reject", "source_line":line,
        "refs":[reference]
    }]})
    .to_string()
}

#[test]
fn prose_owner_mismatch_is_visible_even_when_rule_refs_are_correct() {
    let raw = json!({
        "purpose":"Checks access",
        "steps":["Check access for Access.Checks.ReadItems"],
        "business_rules":[{"when":"Not Access.Checks.Read(Access.Objects.ReadItems)","then":"reject","source_line":1,"refs":["Access.Objects.ReadItems"]}]
    }).to_string();
    let body = "If Not Access.Checks.Read(Access.Objects.ReadItems) Then Return";
    let warnings = validate_method_source_anchors(&raw, body, 1, "vb");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("Step 1: qualified expression `Access.Checks.ReadItems`"));
    let mut parsed = parse_llm_response(&raw, "Access.vb", "Read", "Access.Read", "hash");
    parsed.validation_warnings = warnings;
    assert!(render_method_as_doc(&parsed).contains("Access.Checks.ReadItems"));
    assert_eq!(parsed.semantic_validation, "not_performed");
}

#[test]
fn prose_checks_preserve_case_spacing_boundaries_and_ignore_unknown_roots() {
    let raw = json!({"purpose":"VB.NET analysis at example.org", "steps":["Read ITEM . TOTAL", "Read item.TotalAmount"]}).to_string();
    let body = "Return item . Total";
    let warnings = validate_method_source_anchors(&raw, body, 1, "vb");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("item.TotalAmount"));
    // Unknown roots are outside this bounded prose check, including case-mismatched
    // roots in C#. Explicit refs continue to receive the existing stricter check.
    let raw = json!({"purpose":"Read item.total"}).to_string();
    assert_eq!(validate_method_source_anchors(&raw, body, 1, "cs").len(), 1);
}

#[test]
fn prose_checks_cover_each_substantive_field_and_bound_new_diagnostics() {
    let raw = json!({"purpose":"item.Missing", "steps":["item.Other"], "data_flow":"item.Unknown", "error_handling":"item.Failure", "side_effects_detail":"item.Write", "business_rules":[{"when":"item.WrongCondition","then":"item.WrongOutcome","source_line":1,"refs":[]}]}).to_string();
    let warnings = validate_method_source_anchors(&raw, "Return item.Total", 1, "vb");
    assert_eq!(warnings.len(), 7, "{warnings:?}");
    assert!(warnings.iter().any(|w| w.starts_with("Rule 1: consequence")));
    let raw = json!({"steps":(0..30).map(|i|format!("item.Missing{i}")).collect::<Vec<_>>(),"business_rules":[{"when":"item.Total < 0","then":"reject","source_line":999,"refs":["Unknown.Table"]}]}).to_string();
    let warnings = validate_method_source_anchors(&raw, "Return item.Total", 1, "vb");
    assert_eq!(warnings.len(), 27, "{warnings:?}");
    assert!(warnings.iter().any(|w| w.contains("6 additional diagnostics omitted")));
    assert!(warnings.iter().any(|w| w.contains("source line is missing or outside")));
    assert!(warnings.iter().any(|w| w.contains("Unknown.Table")));
}

#[test]
fn prose_roots_and_identity_evidence_exclude_comments_strings_and_urls() {
    for (language, body) in [
        ("vb", "' docs.Root\nDim label = \"text.Root\"\nReturn item.Total ' item.Wrong"),
        ("cs", "/* docs.Root */\nvar label = \"text.Root\";\nreturn item.Total; // item.Wrong"),
    ] {
        let raw = json!({"steps":["docs.Unknown text.Unknown", "See https://item.example/path and ftp://item.archive/file", "Read item.Wrong"]}).to_string();
        let warnings = validate_method_source_anchors(&raw, body, 1, language);
        assert_eq!(warnings.len(), 1, "{language}: {warnings:?}");
        assert!(warnings[0].contains("Step 3") && warnings[0].contains("item.Wrong"));
    }
}

#[test]
fn prose_sentence_boundaries_do_not_invent_members_or_restart_skipped_chains() {
    let body = "Return item.Total + Total.Actual";
    let raw = json!({"steps":[
        "Reads item.Total. Returns the value.",
        "Reads item.Total.\nReturns the value.",
        "Reads ITEM . TOTAL and item .Total.",
        "Reads item. Total.Other and item.\nTotal.Other.",
        "Reads item.Missing. Then returns the value."
    ]}).to_string();
    let warnings = validate_method_source_anchors(&raw, body, 1, "vb");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("item.Missing"));
    assert!(!warnings[0].contains("Missing.Then"));
}

#[test]
fn unsupported_languages_keep_existing_refs_without_c_family_prose_masking() {
    let raw = json!({"steps":["item.Wrong"],"business_rules":[{"when":"valid","then":"read","source_line":1,"refs":["Missing.Reference"]}]}).to_string();
    let warnings = validate_method_source_anchors(&raw, "item.Total", 1, "ml");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("Missing.Reference"));
}

#[test]
fn metadata_alone_cannot_be_counted_as_a_business_rule() {
    let raw = json!({"purpose":"Save invoice", "business_rules":[{
        "source_line":1,"refs":["invoice.Total"]
    }]}).to_string();
    let parsed = parse_llm_response(&raw, "Invoice.vb", "Save", "Invoice.Save", "hash");
    assert!(parsed.business_rules.is_empty(), "{:?}", parsed.business_rules);
    let warnings = validate_method_source_anchors(&raw, "If invoice.Total < 0 Then Return", 1, "vb");
    assert!(warnings.iter().any(|w| w.contains("no rule text")), "{warnings:?}");
}

#[test]
fn partial_and_legacy_structured_rules_are_preserved_with_shape_warnings() {
    for entry in [
        json!({"when":"invoice.Total < 0","source_line":1}),
        json!({"then":"reject","source_line":1}),
        json!({"rule":"Negative invoices are rejected","source_line":1}),
    ] {
        let raw = json!({"purpose":"Save invoice", "business_rules":[entry]}).to_string();
        let parsed = parse_llm_response(&raw, "Invoice.vb", "Save", "Invoice.Save", "hash");
        assert_eq!(parsed.business_rules.len(), 1);
        let warnings = validate_method_source_anchors(&raw, "If invoice.Total < 0 Then Return", 1, "vb");
        assert!(warnings.iter().any(|w| w.contains("incomplete WHEN/THEN")), "{warnings:?}");
    }
}

#[test]
fn invented_qualified_reference_is_visible_in_json_and_stored_markdown() {
    let raw = response(Some(11), "Invoices.InvoiceId");
    let body = "Sub Save(invoiceId As Integer)\n If invoiceId < 1 Then Return\nEnd Sub";
    let warnings = validate_method_source_anchors(&raw, body, 10, "vb");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Invoices.InvoiceId"));
    let mut analysis = parse_llm_response(&raw, "Invoice.vb", "Save", "Invoice.Save", "hash");
    analysis.validation_warnings = warnings;
    assert_eq!(analysis.confidence, "unverified");
    let markdown = render_method_as_doc(&analysis);
    assert!(markdown.contains("Source checks requiring review"));
    assert!(markdown.contains("not present in the supplied method"));
    let cross_check = validate_llm_output(&analysis, &analysis, &[]);
    assert!(
        cross_check
            .warnings
            .iter()
            .any(|w| w.contains("Invoices.InvoiceId"))
    );
    assert_ne!(cross_check.confidence.to_string(), "High");
}

#[test]
fn line_checks_respect_real_method_offset_and_comment_boundaries() {
    let body = "Sub Save()\n ' validation comment\n If invoice.Total < 0 Then Return\nEnd Sub";
    for line in [None, Some(1), Some(9), Some(14)] {
        assert!(
            validate_method_source_anchors(&response(line, "invoice.Total"), body, 10, "vb")
                .iter()
                .any(|w| w.contains("missing or outside"))
        );
    }
    assert!(
        validate_method_source_anchors(&response(Some(11), "invoice.Total"), body, 10, "vb")
            .iter()
            .any(|w| w.contains("blank or a comment"))
    );
    assert!(
        validate_method_source_anchors(&response(Some(12), "invoice.Total"), body, 10, "vb")
            .is_empty()
    );
}

#[test]
fn references_follow_language_case_rules_without_claiming_semantic_validation() {
    let body = "Sub Save()\n If invoice . Total < 0 Then Return\nEnd Sub";
    let raw = response(Some(2), "INVOICE.TOTAL");
    assert!(validate_method_source_anchors(&raw, body, 1, "vb").is_empty());
    assert_eq!(validate_method_source_anchors(&raw, body, 1, "cs").len(), 1);
    assert_eq!(
        parse_llm_response(&raw, "Invoice.vb", "Save", "Invoice.Save", "hash").confidence,
        "unverified"
    );
}

#[test]
fn legacy_rules_remain_usable_but_cannot_silently_claim_source_anchors() {
    let raw = r#"{"purpose":"Save", "business_rules":["Invoice amounts must be positive"]}"#;
    let warnings = validate_method_source_anchors(raw, "Sub Save()\nEnd Sub", 1, "vb");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("unstructured rule"));
    assert!(
        parse_llm_response(raw, "Invoice.vb", "Save", "Invoice.Save", "hash")
            .parse_diagnostic
            .is_empty()
    );
}

#[test]
fn a_longer_identifier_does_not_validate_an_invented_reference() {
    for body in [
        "If invoice.TotalAmount < 0 Then Return",
        "If archived_invoice.Total < 0 Then Return",
    ] {
        let warnings =
            validate_method_source_anchors(&response(Some(1), "invoice.Total"), body, 1, "vb");
        assert!(
            warnings.iter().any(|w| w.contains("not present")),
            "{warnings:?}"
        );
    }
    assert!(
        validate_method_source_anchors(
            &response(Some(1), "invoice.Total"),
            "If invoice.Total < 0 Then Return",
            1,
            "vb"
        )
        .is_empty()
    );
}

#[test]
fn high_static_consistency_does_not_remove_inference_qualification() {
    let raw = response(Some(1), "invoice.Total");
    let mut analysis = parse_llm_response(&raw, "Invoice.vb", "Save", "Invoice.Save", "hash");
    for level in ["High", "Medium", "Low"] {
        analysis.confidence = level.into();
        assert!(
            render_method_as_doc(&analysis)
                .contains("semantic accuracy has not been independently verified")
        );
    }
}

#[test]
fn unknown_code_shaped_root_needs_known_member_and_respects_case() {
    let body = "Return account_owner.display_name";
    let raw = json!({"data_flow":"Read account_owenr.display_name. Returns the value.", "steps":["Read ACCOUNT_OWNER . DISPLAY_NAME.","Read invented_owner.unknown_member.","Read unknown.DisplayName."]}).to_string();
    let warnings = validate_method_source_anchors(&raw, body, 1, "vb");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].starts_with("Data flow:") && warnings[0].contains("unknown root"));
    assert!(warnings[0].contains("account_owenr.display_name") && warnings[0].contains("does not identify an intended owner"));
    let raw = json!({"steps":["Read Account_owner.display_name."]}).to_string();
    assert!(validate_method_source_anchors(&raw, body, 1, "vb").is_empty());
    assert!(validate_method_source_anchors(&raw, body, 1, "cs")[0].contains("unknown root"));
    let raw = json!({"steps":["Read ACCOUNT_OWNER.DISPLAY_NAME."]}).to_string();
    // A differently cased unknown suffix is deliberately outside the C# slice.
    assert!(validate_method_source_anchors(&raw, body, 1, "cs").is_empty());
}

#[test]
fn unknown_root_slice_excludes_noncode_context_and_nonexecutable_suffixes() {
    let raw = json!({"steps":[
        "VB.NET v1_2.display_name user_guide.md ordinary.prose.",
        "See https://wrong_owner.display_name/path ftp://wrong_owner.display_name and www.wrong_owner.display_name.",
        "Contact person@wrong_owner.display_name or open /wrong_owner.display_name and C:\\wrong_owner.display_name.",
        "See wrong_owner. display_name and wrong_owner.\ndisplay_name.",
        "Read wrong_owner.comment_member or wrong_owner.string_member."
    ]}).to_string();
    for (language, body) in [
        ("vb", "' docs.comment_member\nDim text = \"docs.string_member\"\nReturn account_owner.display_name"),
        ("cs", "/* docs.comment_member */\nvar text = \"docs.string_member\";\nreturn account_owner.display_name;"),
    ] {
        assert!(validate_method_source_anchors(&raw, body, 1, language).is_empty(), "{language}");
    }
    let raw = json!({"steps":["Read wrong_owner.display_name"]}).to_string();
    assert!(validate_method_source_anchors(&raw, "account_owner.display_name", 1, "ml").is_empty());
}

#[test]
fn unknown_root_checks_share_cap_and_attach_rule_diagnostics() {
    let expressions = (0..30).map(|i| format!("wrong_owner{i}.display_name")).collect::<Vec<_>>().join(" ");
    let raw = json!({"steps":[expressions],"business_rules":[{"when":"ready","then":"read wrong_owner.display_name","source_line":1,"refs":["account_owner.display_name"]}]}).to_string();
    let warnings = validate_method_source_anchors(&raw, "Return account_owner.display_name", 1, "vb");
    assert_eq!(warnings.len(), 25, "{warnings:?}");
    assert!(warnings.last().unwrap().contains("7 additional diagnostics omitted after 24"));
    let raw = json!({"business_rules":[{"when":"ready","then":"read wrong_owner.display_name","source_line":1,"refs":["account_owner.display_name"]}]}).to_string();
    let mut parsed = parse_llm_response(&raw, "Generic.vb", "Read", "Generic.Read", "hash");
    engram_server::services::business_logic_service::attach_method_source_diagnostics(&mut parsed, &raw, "Return account_owner.display_name", 1, "vb");
    let association = engram_server::services::business_rule_diagnostics::associate(parsed.rule_source_diagnostics.as_ref(), &parsed.business_rules[0]);
    assert!(association.blocks_outcome);
    assert!(association.summary.contains("Rule 1: consequence"));
}


#[test]
fn explicit_references_only_in_comments_do_not_clear_source_checks() {
    for (language, body) in [
        ("vb", "' invoice.Total\nReturn value"),
        ("vbnet", "REM invoice.Total\nReturn value"),
        ("vb", "Dim x = 1 : Rem invoice.Total\nReturn value"),
        ("cs", "// invoice.Total\nreturn value;"),
        ("csharp", "/* header\n invoice.Total\n */\nreturn value;"),
    ] {
        let raw = json!({"business_rules":[{"when":"condition","then":"outcome",
            "source_line":body.lines().count(),"refs":["invoice.Total"]}]}).to_string();
        let warnings = validate_method_source_anchors(&raw, body, 1, language);
        assert!(warnings.iter().any(|w| w.contains("reference `invoice.Total` is not present")), "{language}: {warnings:?}");
    }
}

#[test]
fn interior_block_comment_anchor_is_not_executable_but_code_after_comment_is() {
    let body = "/* header\r\n invoice.Total\r\n */ return value;";
    let raw = |line| json!({"business_rules":[{"when":"condition","then":"outcome","source_line":line,"refs":[]}]}).to_string();
    assert!(validate_method_source_anchors(&raw(21), body, 20, "cs").iter().any(|w| w.contains("blank or a comment")));
    assert!(validate_method_source_anchors(&raw(22), body, 20, "cs").is_empty());
}

#[test]
fn explicit_literal_refs_survive_comment_mask_with_language_case_rules() {
    for (language, body, reference) in [
        ("vb", "Dim sql = \"SELECT invoice.Total FROM ledger -- // /* not comments */\" ' other.Missing", "INVOICE.TOTAL"),
        ("cs", "var sql = \"SELECT invoice.Total FROM ledger -- // /* not comments */\"; // other.Missing", "invoice.Total"),
        ("vb", "Dim key = \"app.setting\"\r\nReturn item . Total", "item.Total"),
        ("cs", "var key = @\"app.setting // literal\";", "app.setting"),
    ] {
        let raw = json!({"business_rules":[{"when":"condition","then":"outcome","source_line":1,"refs":[reference]}]}).to_string();
        assert!(validate_method_source_anchors(&raw, body, 1, language).is_empty(), "{language}");
    }
    let raw = json!({"business_rules":[{"when":"condition","then":"outcome","source_line":1,"refs":["Invoice.Total"]}]}).to_string();
    assert!(validate_method_source_anchors(&raw, "return invoice.Total;", 1, "cs").iter().any(|w| w.contains("reference `Invoice.Total` is not present")));
}

#[test]
fn unsupported_language_retains_legacy_reference_presence_without_masking_claim() {
    let raw = json!({"business_rules":[{"when":"condition","then":"outcome","source_line":2,"refs":["invoice.Total"]}]}).to_string();
    assert!(validate_method_source_anchors(&raw, "# invoice.Total\nreturn value", 1, "python").is_empty());
}


#[test]
fn literal_payload_refs_are_preserved_but_literal_only_anchor_is_qualified() {
    for (language, body) in [
        ("cs", "var text = \"\"\"\n invoice.Total // /* literal */\n\"\"\";"),
        ("csharp", "var text = @\"\n invoice.Total // /* literal */\n\";"),
        ("vb", "Dim text = \"\n invoice.Total ' literal\n\""),
] {
        let raw = json!({"business_rules":[{"when":"condition","then":"outcome","source_line":2,"refs":["invoice.Total"]}]}).to_string();
        let warnings = validate_method_source_anchors(&raw, body, 1, language);
        assert!(warnings.iter().any(|w| w.contains("literal-only span")), "{warnings:?}");
        assert!(!warnings.iter().any(|w| w.contains("reference `invoice.Total` is not present")), "{warnings:?}");
    }
    for body in [
        "Dim text = \"label: Rem invoice.Total\"",
        "Dim text = \"quoted \"\"value\"\" invoice.Total\"",
        "Dim stamp = #1/1/2020 12:30# : Dim text = \"invoice.Total\"",
] {
        let raw = json!({"business_rules":[{"when":"condition","then":"outcome","source_line":1,"refs":["invoice.Total"]}]}).to_string();
        assert!(validate_method_source_anchors(&raw, body, 1, "vb").is_empty());
    }
}

#[test]
fn vb_with_qualified_spelling_matches_source_without_binding_claims() {
    let body = "Sub Save(row As Item)\nWith row\n .CreatedBy = actor.Id\n .CreatedTime = Now\n If String.IsNullOrWhiteSpace(.Path) Then .Path = \"\"\nEnd With\nEnd Sub";
    let raw = json!({"steps":["Sets ROW . CREATEDBY and row.CreatedTime and row.Path"],"business_rules":[{"when":"value supplied","then":"assign row.CreatedBy","source_line":3,"refs":["row.CreatedBy"]}]}).to_string();
    assert!(validate_method_source_anchors(&raw, body, 1, "vb").is_empty());
    assert!(!validate_method_source_anchors(&raw, body, 1, "cs").is_empty());
    let parsed = parse_llm_response(&raw, "Generic.vb", "Save", "Generic.Save", "hash");
    assert_eq!(parsed.semantic_validation, "not_performed");
}

#[test]
fn vb_with_nested_receivers_restore_outer_and_reject_wrong_identity() {
    let body = "Sub Save(row As Item, other As Item)\nWith row\n .Outer = 1\n With other\n  .Inner = 2\n End With\n With .Child\n  .Value = 3\n End With\n .After = 4\nEnd With\nEnd Sub";
    let good = json!({"steps":["row.Outer other.Inner row.Child.Value row.After"],"business_rules":[]}).to_string();
    assert!(validate_method_source_anchors(&good, body, 1, "vb").is_empty());
    let bad = json!({"business_rules":[{"when":"condition","then":"action","source_line":3,"refs":["row.Inner","other.Outer","row.Value"]}]}).to_string();
    let warnings = validate_method_source_anchors(&bad, body, 1, "vb");
    assert_eq!(warnings.len(),3,"{warnings:?}");
}

#[test]
fn vb_with_unknown_helper_comments_literals_and_deferred_bodies_do_not_supply_aliases() {
    for body in [
        "Sub Save(row As Item)\nWith Resolve()\n .Missing = 1\nEnd With\nEnd Sub",
        "Sub Save(row As Item)\nWith row\n ' .Missing = 1\n Dim text = \".Missing\"\nEnd With\nEnd Sub",
        "Sub Save(row As Item)\nWith row\n Dim callback = Sub()\n .Missing = 1\n End Sub\nEnd With\nEnd Sub",
        "Sub Save(row As Item)\nWith row\n .Missing = 1\nEnd Sub",
    ] {
        let raw = json!({"business_rules":[{"when":"condition","then":"action","source_line":1,"refs":["row.Missing"]}]}).to_string();
        assert!(validate_method_source_anchors(&raw, body, 1, "vb").iter().any(|w| w.contains("reference `row.Missing` is not present")),"{body}");
    }
    let body = "Sub Save(row As Item)\nWith row\n .Total = 1\nEnd With\nDim sql = \"config.key\"\nEnd Sub";
    let raw = json!({"business_rules":[{"when":"condition","then":"action","source_line":3,"refs":["config.key","row.Total"]}]}).to_string();
    assert!(validate_method_source_anchors(&raw, body, 1, "vb").is_empty());
}

#[test]
fn vb_with_named_arguments_and_unrelated_initializers_keep_valid_identity() {
    let body = "Sub Save(row As Item, db As Store)\nDim initial = New Item With {\n .Other = 1\n}\nWith row\n .Created = Now\n Log(value:=.Created)\nEnd With\nLog(db:=db)\nEnd Sub";
    let raw = json!({"steps":["Set row.Created"],"business_rules":[{"when":"condition","then":"assign","source_line":6,"refs":["row.Created"]}]}).to_string();
    assert!(validate_method_source_anchors(&raw, body, 1, "vb").is_empty());
    for body in [
        "Sub Save(row As Item)\nWith row\n Dim item = New Item With {.Other = 1}\n .Missing = 1\nEnd With\nEnd Sub",
        "Sub Save(row As Item)\nWith row\n .Missing = 1 : Other()\nEnd With\nEnd Sub",
    ] {
        let raw = json!({"business_rules":[{"when":"condition","then":"action","source_line":1,"refs":["row.Missing"]}]}).to_string();
        assert!(validate_method_source_anchors(&raw, body, 1, "vb").iter().any(|w|w.contains("reference `row.Missing` is not present")));
    }
}

#[test]
fn vb_with_header_does_not_bypass_scanner_for_first_member() {
    let raw = json!({"business_rules":[{"when":"condition","then":"action","source_line":1,"refs":["row.Missing"]}]}).to_string();
    for body in [
        "Sub Save(row As Item)\nWith row\n .Missing=1\nEnd Sub",
        "Sub Save(row As Item)\nWith row\n .Missing=1 : Other()\nEnd With\nEnd Sub",
        "Sub Save(row As Item)\nWith row\n .Missing=1\n Dim item = New Item With {.Value=2}\nEnd With\nEnd Sub",
    ] {
        let warnings = validate_method_source_anchors(&raw, body, 1, "vb");
        assert!(warnings.iter().any(|w|w.contains("reference `row.Missing` is not present")),"{body}: {warnings:?}");
    }
    let supported = "Sub Save(row As Item)\nWith row\n .Missing=1\nEnd With\nEnd Sub";
    assert!(validate_method_source_anchors(&raw, supported, 1, "vb").is_empty());
    for (language, body) in [("cs", "row\n .Missing = 1;"), ("vb", "row\n .Missing = 1") ] {
        assert!(validate_method_source_anchors(&raw, body, 1, language).is_empty());
    }
}

#[test]
fn vb_with_header_retains_explicit_receiver_and_argument_references() {
    for (body, reference) in [
        ("Sub Save(row As Item)\nWith row.Child\n .Value = 1\nEnd With\nEnd Sub", "row.Child"),
        ("Sub Save(row As Item)\r\nWith Lookup(row.Id)\r\n .Value = 1\r\nEnd With\r\nEnd Sub", "row.Id"),
        ("Sub Save(row As Item)\nWith Lookup(\"config.key\")\n .Value = 1\nEnd With\nEnd Sub", "config.key"),
    ] {
        let raw = json!({"business_rules":[{"when":"condition","then":"action","source_line":1,"refs":[reference]}]}).to_string();
        assert!(validate_method_source_anchors(&raw, body, 1, "vb").is_empty(),"{body}");
    }
    let body = "Sub Save(row As Item)\nWith Lookup(row.Id)\n .Value = 1\nEnd With\nEnd Sub";
    let raw = json!({"business_rules":[{"when":"condition","then":"action","source_line":1,"refs":["row.Value"]}]}).to_string();
    assert!(validate_method_source_anchors(&raw, body, 1, "vb").iter().any(|w|w.contains("reference `row.Value` is not present")));
}
