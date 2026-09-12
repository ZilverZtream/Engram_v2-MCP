//! Missing enclosing predicates must remain visible even when return paths are unavailable.
use engram_core::ContentHash;
use engram_server::services::{
    business_logic_service::{attach_method_source_diagnostics_with_evidence, parse_llm_response, render_method_as_doc},
    business_outcome_dependencies::{collect, OutcomeEvidence},
    business_return_paths::ReturnPathEvidence,
    business_rule_diagnostics::associate,
};
use serde_json::{json, Value};

const VERSION: &str = "business-logic-member-v17-distinct-call-guards";

fn query_fixture(query: &str) -> (String, OutcomeEvidence) {
    let body = format!("Function Build(rows As Object) As Object\n If Access.Allowed() Then\n{query}\n Return Nothing\n End If\nEnd Function");
    let end = 10 + body.lines().count() as u32 - 1;
    let mut evidence = evidence(BODY, "Access.Allowed()");
    let paths = evidence.return_paths.as_mut().unwrap();
    paths.end_line = end;
    paths.source_blake3 = blake3::hash(body.as_bytes()).to_hex().to_string();
    let report = paths.result.as_mut().unwrap();
    report["method_end_line"] = json!(end);
    report["branch_contexts"]["method_end_line"] = json!(end);
    report["branch_contexts"]["entries"][0]["statement_kind"] = json!("LocalDeclarationStatement");
    report["branch_contexts"]["entries"][0]["end_line"] = json!(12 + query.lines().count() - 1);
    evidence.fingerprint(&body, VERSION);
    (body, evidence)
}

fn query_rule(when: &str, then: &str, anchor: u32) -> String {
    json!({"purpose":"Builds a query.","business_rules":[{"when":when,"then":then,"source_line":anchor,"refs":[]}]}).to_string()
}

fn query_warnings(analysis: &engram_server::services::business_logic_service::MethodBusinessLogic) -> Vec<&str> {
    analysis.validation_warnings.iter().filter(|w| w.starts_with("Rule 1:") && w.contains("complete compound query predicate")).map(String::as_str).collect()
}

#[test]
fn partial_query_predicate_cannot_be_promoted_to_sufficient_qualification() {
    let (body, evidence) = query_fixture(" Dim q = From row In rows\n Where row.Member AndAlso row.Active AndAlso Not row.Total\n Select row");
    let raw = query_rule("Access.Allowed() AndAlso row.Member", "row qualifies for the query", 13);
    let analysis = check(&body, &evidence, &raw);
    assert_eq!(query_warnings(&analysis).len(), 1);
    assert!(query_warnings(&analysis)[0].contains("row.Member AndAlso row.Active AndAlso Not row.Total"));
    assert!(associate(analysis.rule_source_diagnostics.as_ref(), &analysis.business_rules[0]).blocks_outcome);
    assert!(analysis.business_rules[0].contains("row qualifies for the query"), "Never rewrite the original claim");
    assert_eq!(analysis.semantic_validation, "not_performed");
    assert!(render_method_as_doc(&analysis).contains(query_warnings(&analysis)[0]));
}

#[test]
fn full_query_spelling_avoids_coverage_warning_without_certifying_semantics() {
    let (body, evidence) = query_fixture(" Dim q = From row In rows\n Where (row.Member OrElse row.Linked) AndAlso row.Active\n Order By row.Name\n Select row");
    let raw = query_rule("Access.Allowed() AndAlso (ROW . MEMBER OrElse row.Linked) AndAlso row.Active", "query predicate matches; other query requirements remain", 13);
    let analysis = check(&body, &evidence, &raw);
    assert!(query_warnings(&analysis).is_empty());
    assert!(analysis.validation_warnings.iter().any(|w| w.starts_with("Query predicate coverage:") && w.contains("1 supported")));
    assert_eq!(analysis.semantic_validation, "not_performed");
}

#[test]
fn query_predicate_literals_then_text_and_identifier_prefixes_do_not_fake_full_coverage() {
    let predicate = "row.Label = \"Active\" AndAlso row.Visible";
    let (body, evidence) = query_fixture(&format!(" Dim q = From row In rows\n Where {predicate}\n Select row"));
    for when in ["Access.Allowed() AndAlso row.Label = \"active\" AndAlso row.Visible", "Access.Allowed() AndAlso Other.row.Label = \"Active\" AndAlso row.Visible", "Access.Allowed() AndAlso label = \"row.Label = \"\"Active\"\" AndAlso row.Visible\""] {
        assert_eq!(query_warnings(&check(&body, &evidence, &query_rule(when, predicate, 13))).len(), 1);
    }
    assert!(query_warnings(&check(&body, &evidence, &query_rule(&format!("Access.Allowed() AndAlso {predicate}"), "matches this predicate only", 13))).is_empty());
}

#[test]
fn multiline_or_ambiguous_query_shapes_are_unassessed_not_partial_success() {
    for query in [
        " Dim q = From row In rows\n Where row.Member AndAlso\n row.Active\n Select row",
        " Dim q = From row In rows\n Where row.Member AndAlso row.Active Select row",
        " Dim q As Object = From row In rows\n Where row.Member AndAlso row.Active\n Select row",
        " Dim q = From row In rows\n Where row.Member OrElse row.Active AndAlso row.Visible\n Select row",
        " Dim q = From row In rows\n Where (row.Member AndAlso row.Active\n Select row",
    ] {
        let (body, evidence) = query_fixture(query);
        let analysis = check(&body, &evidence, &query_rule("Access.Allowed() AndAlso row.Member", "row qualifies", 13));
        assert!(query_warnings(&analysis).is_empty());
        assert!(analysis.validation_warnings.iter().any(|w| w.starts_with("Query predicate coverage:") && w.contains("0 supported") && w.contains("unassessed")));
    }
}

#[test]
fn query_lookalikes_in_comments_and_multiline_literals_do_not_supply_predicates() {
    for query in [
        " Dim q = From row In rows\n ' Where row.Member AndAlso row.Active\n Select row",
        " Dim q = From row In rows\n Where row.Label = \"first\nWhere row.Member AndAlso row.Active\nSelect row\nlast\" AndAlso row.Visible\n Select row",
    ] {
        let (body, evidence) = query_fixture(query);
        let analysis = check(&body, &evidence, &query_rule("Access.Allowed() AndAlso row.Member", "row qualifies", 15));
        assert!(query_warnings(&analysis).is_empty());
        assert!(analysis.validation_warnings.iter().any(|w| w.starts_with("Query predicate coverage:") && w.contains("0 supported")));
    }
}

#[test]
fn stale_and_overlapping_query_ownership_never_attach_predicate_claims() {
    let (body, mut evidence) = query_fixture(" Dim q = From row In rows\n Where row.Member AndAlso row.Active\n Select row");
    let raw = query_rule("Access.Allowed() AndAlso row.Member", "row qualifies", 13);
    let changed = body.replace("row.Active", "row.Enabled");
    assert!(query_warnings(&check(&changed, &evidence, &raw)).is_empty());
    let branch = &mut evidence.return_paths.as_mut().unwrap().result.as_mut().unwrap()["branch_contexts"];
    let duplicate = branch["entries"][0].clone();
    branch["entries"].as_array_mut().unwrap().push(duplicate);
    evidence.fingerprint(&body, VERSION);
    assert!(query_warnings(&check(&body, &evidence, &raw)).is_empty());
}

#[test]
fn xml_query_text_is_not_a_where_clause_and_excluded_comparisons_remain_unknown() {
    for (query, anchor) in [
        (" Dim q = From row In rows\n Select <note>\nWhere row.Member AndAlso row.Active\nSelect row\n</note>", 14),
        (" Dim q = From row In rows\n Where row.Count < 5 AndAlso row.Active\n Select row", 13),
    ] {
        let (body, evidence) = query_fixture(query);
        let analysis = check(&body, &evidence, &query_rule("Access.Allowed() AndAlso row.Member", "row qualifies", anchor));
        assert!(query_warnings(&analysis).is_empty(), "XML text/comparisons excluded from this lexical slice: {:?}", analysis.validation_warnings);
        assert!(analysis.validation_warnings.iter().any(|w| w.starts_with("Query predicate coverage:") && w.contains("0 supported")));
    }
    let (body, evidence) = query_fixture(" Dim q = From row In rows\n Where row.Label = \"<note>\" AndAlso row.Active\n Select row");
    let analysis = check(&body, &evidence, &query_rule("Access.Allowed() AndAlso row.Active", "row qualifies", 13));
    assert_eq!(query_warnings(&analysis).len(), 1, "A quoted less-than sign is not XML syntax");
    for (declaration_lines, expected) in [(1024, 1), (1025, 0)] {
        let padding = "\n".repeat(declaration_lines - 3);
        let query = format!(" Dim q = From row In rows\n{padding} Where row.Member AndAlso row.Active\n Select row");
        assert_eq!(query.lines().count(), declaration_lines);
        let anchor = 12 + declaration_lines as u32 - 2;
        let (body, evidence) = query_fixture(&query);
        let analysis = check(&body, &evidence, &query_rule("Access.Allowed() AndAlso row.Member", "row qualifies", anchor));
        assert_eq!(query_warnings(&analysis).len(), expected, "Inclusive declaration line cap");
    }
}
const BODY: &str = "Function Build(flag As Boolean) As Object\n If Access.Allowed() Then\n  If flag Then\n   Return 1\n  Else\n   Return 2\n  End If\n Else\n  Return 3\n End If\nEnd Function";

fn evidence(body: &str, expression: &str) -> OutcomeEvidence {
    let mut evidence = collect(body, "Fixture", "vb", 10, "Fixture.vb", None);
    let hash = "a".repeat(64);
    evidence.return_paths = Some(ReturnPathEvidence {
        source_blake3: blake3::hash(body.as_bytes()).to_hex().to_string(),
        start_line: 10, end_line: 20, unavailable_reason: None,
        result: Some(json!({
            "version":"vb-return-paths-v1", "status":"unavailable",
            "source_sha256":hash, "method_start_line":10,"method_end_line":20,
            "structural_complete":false,"paths":[],"normal_fallthrough_paths":0,
            "branch_contexts":{
                "version":"vb-branch-contexts-v1","status":"available",
                "source_sha256":hash,"method_start_line":10,"method_end_line":20,
                "scope":"Syntactic ancestry only; no semantic proof.",
                "unavailable_lines":[],
                "entries":[{"statement_kind":"IfStatement","start_line":12,"end_line":12,
                    "conditions":[{"expression":expression,"line":11,"end_line":11,"outcome":"true"}]}]
            }
        })),
    });
    evidence.fingerprint(body, VERSION);
    evidence
}

fn raw(when: &str, then: &str, refs: Value) -> String {
    json!({"purpose":"Builds a conditional result.","business_rules":[
        {"when":when,"then":then,"source_line":12,"refs":refs}
    ]}).to_string()
}

fn check(body: &str, evidence: &OutcomeEvidence, raw: &str) -> engram_server::services::business_logic_service::MethodBusinessLogic {
    let mut analysis = parse_llm_response(raw,"Fixture.vb","Build","Fixture.Build",&ContentHash::compute(body.as_bytes()).0);
    attach_method_source_diagnostics_with_evidence(&mut analysis,raw,body,10,"vb",evidence);
    analysis
}

fn branch_warnings(analysis: &engram_server::services::business_logic_service::MethodBusinessLogic) -> Vec<&str> {
    analysis.validation_warnings.iter().filter(|w| w.starts_with("Rule 1:") && w.to_lowercase().contains("enclosing")).map(String::as_str).collect()
}

#[test]
fn omitted_outer_predicate_is_diagnosed_and_preserved_when_return_analysis_is_unavailable() {
    let raw = raw("flag", "return the nested result",json!([]));
    let analysis = check(BODY,&evidence(BODY,"Access.Allowed()"),&raw);
    let warnings = branch_warnings(&analysis);
    assert_eq!(warnings.len(),1,"{:?}",analysis.validation_warnings);
    assert!(warnings[0].contains("Access.Allowed()") && warnings[0].contains("11"));
    assert!(analysis.business_rules[0].starts_with("IF flag THEN"));
    assert_eq!(analysis.semantic_validation,"not_performed");
    assert!(associate(analysis.rule_source_diagnostics.as_ref(),&analysis.business_rules[0]).blocks_outcome);
    assert!(render_method_as_doc(&analysis).contains(warnings[0]));
}

#[test]
fn explicit_outer_condition_keeps_no_diagnostic_without_claiming_truth() {
    for when in ["Access.Allowed() AndAlso flag", "ACCESS . ALLOWED ( ) AndAlso flag"] {
        let analysis = check(BODY,&evidence(BODY,"Access.Allowed()"),&raw(when,"return the nested result",json!([])));
        assert!(branch_warnings(&analysis).is_empty(),"{:?}",analysis.validation_warnings);
        assert_eq!(analysis.semantic_validation,"not_performed");
        assert!(!associate(analysis.rule_source_diagnostics.as_ref(),&analysis.business_rules[0]).blocks_outcome);
    }
}

#[test]
fn then_refs_strings_and_identifier_suffixes_do_not_supply_a_missing_condition() {
    for when in ["flag", "label = \"Access.Allowed()\" AndAlso flag", "OtherAccess.Allowed() AndAlso flag", "Other.Access.Allowed() AndAlso flag"] {
        let analysis = check(BODY,&evidence(BODY,"Access.Allowed()"),&raw(when,"Access.Allowed() is mentioned here",json!(["Access.Allowed()"] )));
        assert_eq!(branch_warnings(&analysis).len(),1,"{when}: {:?}",analysis.validation_warnings);
    }
}

#[test]
fn literal_case_is_preserved_while_vb_identifier_case_is_not() {
    let body = BODY.replace("Access.Allowed()","Mode = \"Admin\"");
    let evidence = evidence(&body,"Mode = \"Admin\"");
    let matching = check(&body,&evidence,&raw("MODE = \"Admin\" AndAlso flag","return result",json!([])));
    assert!(branch_warnings(&matching).is_empty());
    let differing = check(&body,&evidence,&raw("Mode = \"admin\" AndAlso flag","return result",json!([])));
    assert_eq!(branch_warnings(&differing).len(),1,"{:?}",differing.validation_warnings);
}

#[test]
fn stale_fingerprint_or_changed_body_cannot_attach_a_branch_warning() {
    let raw = raw("flag","return result",json!([]));
    let evidence = evidence(BODY,"Access.Allowed()");
    let changed = BODY.replace("Return 3","Return 4");
    let analysis = check(&changed,&evidence,&raw);
    assert!(branch_warnings(&analysis).is_empty());
    assert!(analysis.validation_warnings.iter().any(|w| !w.starts_with("Rule ")));
}

#[test]
fn ambiguous_line_unknown_version_or_mismatched_source_provides_no_inferred_guard() {
    for mutation in 0..4 {
        let mut evidence = evidence(BODY,"Access.Allowed()");
        let contexts = &mut evidence.return_paths.as_mut().unwrap().result.as_mut().unwrap()["branch_contexts"];
        match mutation {
            0 => contexts["unavailable_lines"] = json!([12]),
            1 => contexts["version"] = json!("future-version"),
            2 => contexts["source_sha256"] = json!("b".repeat(64)),
            _ => { let duplicate = contexts["entries"][0].clone(); contexts["entries"].as_array_mut().unwrap().push(duplicate); },
        }
        evidence.fingerprint(BODY,VERSION);
        let analysis = check(BODY,&evidence,&raw("flag","return result",json!([])));
        assert!(branch_warnings(&analysis).is_empty(),"mutation {mutation}: {:?}",analysis.validation_warnings);
    }
}

#[test]
fn guard_spelling_is_not_polarity_or_semantic_equivalence_validation() {
    let analysis = check(BODY,&evidence(BODY,"Access.Allowed()"),&raw("Not Access.Allowed() AndAlso flag","return result",json!([])));
    // This deliberately narrower detector cannot certify polarity from spelling presence.
    assert!(branch_warnings(&analysis).is_empty());
    assert_eq!(analysis.semantic_validation,"not_performed");
    assert!(analysis.validation_warnings.iter().any(|w| !w.starts_with("Rule ") && w.to_lowercase().contains("polarity")));
}

#[test]
fn multiline_source_conditions_preserve_crlf_and_all_statement_coordinates() {
    for newline in ["\n", "\r\n"] {
        let expression = format!("Access.Allowed() AndAlso{newline}  Tenant.Ready()");
        let body = BODY.replace("Access.Allowed()", "Access.Allowed() AndAlso\n  Tenant.Ready()").replace('\n',newline);
        let mut evidence = evidence(&body,&expression);
        let paths = evidence.return_paths.as_mut().unwrap();
        paths.end_line = 21;
        let result = paths.result.as_mut().unwrap();
        result["method_end_line"] = json!(21);
        let contexts = &mut result["branch_contexts"];
        contexts["method_end_line"] = json!(21);
        contexts["entries"][0]["start_line"] = json!(13);
        contexts["entries"][0]["end_line"] = json!(13);
        contexts["entries"][0]["conditions"][0]["end_line"] = json!(12);
        evidence.fingerprint(&body,VERSION);
        let mut raw: Value = serde_json::from_str(&raw("flag","return result",json!([]))).unwrap();
        raw["business_rules"][0]["source_line"] = json!(13);
        let analysis = check(&body,&evidence,&raw.to_string());
        assert_eq!(branch_warnings(&analysis).len(),1,"{newline:?}: {:?}",analysis.validation_warnings);
    }
}
