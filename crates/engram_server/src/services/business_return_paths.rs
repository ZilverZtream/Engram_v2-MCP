//! Source-derived evaluation histories, separate from inferred business rules.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReturnPathEvidence {
    pub source_blake3: String,
    pub start_line: u32,
    pub end_line: u32,
    pub result: Option<Value>,
    pub unavailable_reason: Option<String>,
}

pub async fn collect(file: &str, source: &str, body: &str, start: u32, language: &str) -> Option<ReturnPathEvidence> {
    if language != "vb" { return None; }
    let end = start.saturating_add(body.lines().count().saturating_sub(1) as u32);
    let mut evidence = ReturnPathEvidence { source_blake3: blake3::hash(source.as_bytes()).to_hex().to_string(),
        start_line:start, end_line:end, result:None, unavailable_reason:None };
    if source.len() > 2_000_000 || start == 0 || body.is_empty() {
        evidence.unavailable_reason = Some("source or method bounds outside supported slice".into());
        return Some(evidence);
    }
    if !source_body_matches(source, body, start) {
        evidence.unavailable_reason = Some("method body does not match supplied source range".into());
        return Some(evidence);
    }
    let file = file.to_owned(); let source = source.to_owned();
    match tokio::task::spawn_blocking(move || engram_index::vb_extractor::vb_return_paths(std::path::Path::new(&file), &source, start, end)).await {
        Ok(Ok(result)) if valid(&result,start,end) => evidence.result = Some(result),
        Ok(Ok(_)) => evidence.unavailable_reason = Some("invalid or unsupported return-path response shape".into()),
        Ok(Err(error)) => evidence.unavailable_reason = Some(error.chars().take(384).collect()),
        Err(_) => evidence.unavailable_reason = Some("return-path worker unavailable".into()),
    }
    Some(evidence)
}

// VB extraction starts at the declaration token, whereas a physical source line
// includes its indentation. Accept that single known offset, without normalizing
// any interior indentation, trailing whitespace, or executable text.
fn source_body_matches(source: &str, body: &str, start: u32) -> bool {
    if start == 0 || body.is_empty() { return false; }
    let mut source_lines = source.lines().skip(start as usize - 1);
    let mut body_lines = body.lines();
    let (Some(source_first), Some(body_first)) = (source_lines.next(), body_lines.next()) else { return false; };
    if source_first != body_first
        && source_first.trim_start_matches([' ', '\t']) != body_first {
        return false;
    }
    body_lines.all(|line| source_lines.next() == Some(line))
}

fn valid(result: &Value, start: u32, end: u32) -> bool {
    let line = |v: &Value| v.as_u64().is_some_and(|n| n >= start as u64 && n <= end as u64);
    let expression = |v: &Value| v.as_str().is_some_and(|s| s.len() <= 4096);
    if result.get("version").and_then(Value::as_str) != Some("vb-return-paths-v1")
        || result.get("method_start_line").and_then(Value::as_u64) != Some(start as u64)
        || result.get("method_end_line").and_then(Value::as_u64) != Some(end as u64) { return false; }
    let Some(paths) = result.get("paths").and_then(Value::as_array) else { return false; };
    if paths.len() > 64 { return false; }
    match result.get("status").and_then(Value::as_str) {
        Some("unavailable") => return paths.is_empty() && result.get("structural_complete").and_then(Value::as_bool) == Some(false),
        Some("available") => {},
        _ => return false,
    }
    if result.get("structural_complete").and_then(Value::as_bool) != Some(true) { return false; }
    // Explicit Return records and normal fallthrough histories share the sidecar's 64-path budget.
    // A missing count cannot establish that implicit termination alternatives are absent.
    let Some(fallthrough) = result.get("normal_fallthrough_paths").and_then(Value::as_u64) else { return false; };
    if fallthrough > 64 || paths.len() as u64 + fallthrough > 64 { return false; }
    paths.iter().all(|path| {
        line(&path["return_line"]) && expression(&path["return_expression"])
            && path["normal_completion_required"].as_bool() == Some(true)
            && path["conditions"].as_array().is_some_and(|conditions| conditions.len() <= 64 && conditions.iter().all(|c| {
                line(&c["line"]) && expression(&c["expression"])
                    && match c["kind"].as_str() {
                        Some("if") => matches!(c["outcome"].as_str(), Some("true"|"false")),
                        Some("case") => matches!(c["outcome"].as_str(), Some("match"|"no_match"))
                            && c.get("case_expression").is_some_and(expression)
                            && c.get("case_line").is_some_and(line),
                        _ => false,
                    }
            }))
    })
}

/// Never emit a current checklist when exact source bytes or method anchors differ.
pub fn checklist(
    evidence: Option<&ReturnPathEvidence>,
    current_hash: Option<&str>,
    range: Option<(u32, u32)>,
    max_paths: usize,
) -> Vec<String> {
    let Some(e) = evidence else {
        return vec!["Structured return paths: unavailable (legacy analysis or unsupported language); no coverage claim.".into()];
    };
    if current_hash != Some(e.source_blake3.as_str()) || range != Some((e.start_line, e.end_line)) {
        return vec!["Structured return paths: STALE_OR_UNVERIFIED source bytes or method anchors; checklist withheld, refresh analysis.".into()];
    }
    let Some(result) = e
        .result
        .as_ref()
        .filter(|r| valid(r, e.start_line, e.end_line))
    else {
        return vec![format!(
            "Structured return paths: unavailable; {}.",
            e.unavailable_reason
                .as_deref()
                .unwrap_or("invalid stored response")
        )];
    };
    let history_bytes = if max_paths <= 2 { 1536 } else { 8 * 1024 };
    let recovery = "Recovery: use full_document, page to end; stored Outcome dependencies v1 / return_paths.result.";
    let binding = "source_binding=verified_bytes_and_range; model_claim_alignment=not_assessed; path_feasibility=not_assessed; test_execution=not_run_by_this_evidence";
    if result["status"] != "available" {
        let mut lines = vec![format!("Structured return paths: unavailable for unsupported syntax or exhausted bounds; {binding}. No fallback branches inferred. {recovery}")];
        append_unsupported(result, &mut lines, if max_paths <= 2 { 512 } else { 4096 });
        return lines;
    }
    let paths = result["paths"].as_array().expect("validated paths");
    // Preserve the existing selection order; this is a rendering change only.
    let mut selected: Vec<_> = paths.iter().enumerate().collect();
    selected.sort_by_key(|(i, p)| {
        (
            std::cmp::Reverse(
                paths
                    .iter()
                    .filter(|other| other["return_line"] == p["return_line"])
                    .count(),
            ),
            std::cmp::Reverse(p["return_line"].as_u64().unwrap_or(0)),
            *i,
        )
    });
    let mut details = Vec::new();
    let mut bytes = 0;
    let mut omitted = Vec::new();
    for (i, path) in selected {
        let raw = serde_json::to_string(path).expect("JSON value serializes");
        if details.len() >= max_paths.min(8) || raw.len() > history_bytes - bytes {
            omitted.push(i + 1);
            continue;
        }
        bytes += raw.len();
        details.push(format!(
            "Source-path test checklist {} (return at line {}): {}",
            i + 1,
            path["return_line"],
            raw
        ));
    }
    let shown = details.len();
    let mut lines = vec![format!("Structured return paths: {} recorded, {shown} shown, {} omitted; {binding}. Syntactic normal-completion histories, not path-feasibility, helper/current-state or requirement proof. Complete records only; {history_bytes} JSON bytes; omitted original path ordinals: {omitted:?}. {recovery}", paths.len(), paths.len()-shown)];
    let fallthrough = result["normal_fallthrough_paths"]
        .as_u64()
        .expect("validated fallthrough count");
    lines[0].push_str(&format!(" Normal fallthrough: {fallthrough} unexpanded; no implicit return values or final state inferred."));
    lines.extend(details);
    append_unsupported(result, &mut lines, if max_paths <= 2 { 512 } else { 4096 });
    lines
}

// Keep whole stored diagnostic records, with display omissions separate from
// producer omissions. Missing legacy metadata is unknown, never zero coverage.
fn append_unsupported(result: &Value, lines: &mut Vec<String>, diagnostic_bytes: usize) {
    let producer_omitted = result
        .get("unsupported_omitted")
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".into());
    let Some(records) = result.get("unsupported").and_then(Value::as_array) else {
        lines[0].push_str(" Stored unsupported diagnostics: inventory unknown; recover full evidence, do not infer absence.");
        return;
    };
    if records.is_empty() {
        lines[0].push_str(&format!(" Unsupported: 0 retained; producer-omitted {producer_omitted}."));
        return;
    }
    let mut details = Vec::new();
    let mut bytes = 0;
    let mut omitted = Vec::new();
    let mut omitted_count = 0;
    for (i, record) in records.iter().enumerate() {
        let raw = serde_json::to_string(record).expect("JSON value serializes");
        if details.len() >= 8 || raw.len() > diagnostic_bytes - bytes {
            omitted_count += 1;
            if omitted.len() < 16 { omitted.push(i + 1); }
            continue;
        }
        bytes += raw.len();
        details.push(format!("Stored unsupported diagnostic {}: {raw}", i + 1));
    }
    lines[0].push_str(&format!(" Stored unsupported diagnostics: {} retained, {} shown, {} display-omitted (first ordinals {omitted:?}; {} further omitted ordinals not listed, use full_document); producer-omitted {producer_omitted}. Raw diagnostic records, not additional semantic validation.", records.len(), details.len(), omitted_count, omitted_count - omitted.len()));
    lines.extend(details);
}

#[cfg(test)]
mod body_source_identity_tests {
    use super::source_body_matches;
    use crate::services::business_logic_service::extract_logic_methods;

    #[test]
    fn actual_vb_extractor_declaration_offset_matches_lf_and_crlf_source() {
        for newline in ["\n", "\r\n"] {
            let source = ["Public Class Sample", "\t    Public Function Allowed(flag As Boolean) As Boolean",
                "        If flag Then", "            Return True", "        End If",
                "        Return False", "    End Function", "End Class"].join(newline);
            let methods = extract_logic_methods(&source, "vb");
            let method = methods.iter().find(|m| m.name == "Allowed").expect("real method extractor");
            assert_eq!(method.start_line, 2);
            assert!(method.body.starts_with("Public Function"));
            assert!(source_body_matches(&source, &method.body, method.start_line));
            let full_lines = source.lines().skip(1).take(method.body.lines().count()).collect::<Vec<_>>().join("\n");
            assert!(source_body_matches(&source, &full_lines, 2));
            assert!(!source_body_matches(&source, &method.body, 1));
            for changed in [
                method.body.replace("            Return True", "           Return True"),
                method.body.replace("Return True", "Return False"),
                method.body.replace("Return True", "Return True "),
                method.body.replace("        End If", "\n        End If"),
                format!(" {}", method.body),
            ] {
                assert!(!source_body_matches(&source, &changed, 2), "unexpected identity: {changed:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> ReturnPathEvidence {
        ReturnPathEvidence {source_blake3:"abc".into(),start_line:10,end_line:20,unavailable_reason:None,
            result:Some(serde_json::json!({"version":"vb-return-paths-v1","method_start_line":10,"method_end_line":20,
                "status":"available","structural_complete":true,"normal_fallthrough_paths":0,"paths":[{"conditions":[{"kind":"if","expression":"enabled","line":12,"outcome":"false"}],"return_line":19,"return_expression":"False","normal_completion_required":true}]}))}
    }
    #[test] fn stale_bytes_or_moved_anchors_withhold_path_checklist() {
        let e=example();
        assert!(checklist(Some(&e),Some("changed"),Some((10,20)),8)[0].contains("withheld"));
        assert!(checklist(Some(&e),Some("abc"),Some((11,21)),8)[0].contains("withheld"));
        let lines=checklist(Some(&e),Some("abc"),Some((10,20)),8);
        assert_eq!(lines.len(),2);assert!(lines[1].contains("enabled"));assert!(lines[0].contains("not path-feasibility"));
    }
    #[test] fn malformed_out_of_range_or_unqualified_paths_are_not_emitted() {
        for field in ["return_line","normal_completion_required"] {
            let mut e=example();e.result.as_mut().unwrap()["paths"][0][field]=Value::Null;
            assert!(checklist(Some(&e),Some("abc"),Some((10,20)),8)[0].contains("unavailable"));
        }
        let mut e=example();e.result.as_mut().unwrap()["paths"][0]["conditions"][0]["line"]=100.into();
        assert!(!valid(e.result.as_ref().unwrap(),10,20));
    }
    #[test] fn summary_only_counts_omissions_without_rendering_paths() {
        let e=example();let lines=checklist(Some(&e),Some("abc"),Some((10,20)),0);
        assert_eq!(lines.len(),1);assert!(lines[0].contains("1 omitted"));
    }
    #[test] fn bounded_display_keeps_shared_fallback_alternatives() {
        let mut e=example();let mut paths=Vec::new();
        for i in 0..9 {
            let mut path=e.result.as_ref().unwrap()["paths"][0].clone();
            path["return_line"]=if i<6 { (10+i).into() } else { 19.into() };
            path["conditions"][0]["expression"]=format!("condition{i}").into();
            paths.push(path);
        }
        e.result.as_mut().unwrap()["paths"]=paths.into();
        let lines=checklist(Some(&e),Some("abc"),Some((10,20)),8);
        assert_eq!(lines.len(),9);assert!(lines[0].contains("1 omitted"));
        for i in 6..9 { assert!(lines.iter().any(|line|line.contains(&format!("condition{i}")))); }
    }
    #[test] fn implicit_only_and_mixed_fallthrough_histories_are_explicitly_unexpanded() {
        for count in [1,2] {
            let mut e=example();let result=e.result.as_mut().unwrap();
            result["paths"]=serde_json::json!([]);result["normal_fallthrough_paths"]=count.into();
            let lines=checklist(Some(&e),Some("abc"),Some((10,20)),8);
            assert_eq!(lines.len(),1);assert!(lines[0].contains("0 recorded, 0 shown, 0 omitted"));
            assert!(lines[0].contains(&format!("Normal fallthrough: {count} unexpanded")));
            assert!(lines[0].contains("no implicit return values"));
        }
        let mut mixed=example();mixed.result.as_mut().unwrap()["normal_fallthrough_paths"]=1.into();
        let lines=checklist(Some(&mixed),Some("abc"),Some((10,20)),8);
        assert_eq!(lines.len(),2);assert!(lines[0].contains("1 recorded, 1 shown, 0 omitted"));
        assert!(lines[0].contains("Normal fallthrough: 1 unexpanded"));
    }
    #[test] fn missing_malformed_or_over_budget_fallthrough_count_is_not_complete() {
        for value in [Value::Null,serde_json::json!(-1),serde_json::json!(1.5),serde_json::json!("2"),serde_json::json!(64),serde_json::json!(65)] {
            let mut e=example();e.result.as_mut().unwrap()["normal_fallthrough_paths"]=value;
            // example has one explicit history, so 64 implicit histories exceed the combined budget.
            assert!(!valid(e.result.as_ref().unwrap(),10,20));
            assert!(checklist(Some(&e),Some("abc"),Some((10,20)),8)[0].contains("unavailable"));
        }
        let mut e=example();e.result.as_mut().unwrap().as_object_mut().unwrap().remove("normal_fallthrough_paths");
        assert!(!valid(e.result.as_ref().unwrap(),10,20));
    }
    #[test] fn zero_fallthrough_preserves_historical_display_counts() {
        let mut e=example();let path=e.result.as_ref().unwrap()["paths"][0].clone();
        e.result.as_mut().unwrap()["paths"]=serde_json::json!(vec![path;26]);
        let lines=checklist(Some(&e),Some("abc"),Some((10,20)),8);
        assert!(lines[0].contains("26 recorded, 8 shown, 18 omitted"));
        assert!(lines[0].contains("Normal fallthrough: 0 unexpanded"));
    }

}

/// Independent optional inventory. Invalid metadata never changes path validity.
/// The parent report was bound to exact UTF-8 source by the sidecar transport;
/// retrieval additionally requires the full-file BLAKE3 and exact member range.
pub fn parameter_qualification(evidence: Option<&ReturnPathEvidence>, current_hash: Option<&str>, range: Option<(u32, u32)>) -> Vec<String> {
    parameter_qualification_for(evidence, current_hash, range, true)
}

/// Retrieval gets one bounded document-level source fact summary, never all parameter facts.
/// Absence is not an empty inventory; existing source/legacy qualification remains.
pub fn parameter_retrieval_qualification(evidence: Option<&ReturnPathEvidence>, current_hash: Option<&str>, range: Option<(u32, u32)>) -> Vec<String> {
    parameter_qualification_for(evidence, current_hash, range, false)
}

fn parameter_qualification_for(evidence: Option<&ReturnPathEvidence>, current_hash: Option<&str>, range: Option<(u32, u32)>, details: bool) -> Vec<String> {
    if !details && evidence.and_then(|e| e.result.as_ref()).and_then(|r| r.get("parameter_occurrences")).is_none() {
        return Vec::new();
    }
    const UNKNOWN: &str = "Parameter identifier inventory: UNKNOWN (VB-only; legacy, unsupported or invalid optional evidence). No absence/read/unused claim.";
    let Some(e) = evidence else { return vec![UNKNOWN.into()]; };
    if current_hash != Some(e.source_blake3.as_str()) || range != Some((e.start_line, e.end_line)) {
        return vec!["Parameter identifier inventory: STALE_OR_UNVERIFIED source bytes or method range; facts withheld, refresh analysis.".into()];
    }
    let Some(r) = e.result.as_ref() else { return vec![UNKNOWN.into()]; };
    if r["version"] != "vb-return-paths-v1"
        || r["method_start_line"].as_u64() != Some(e.start_line as u64)
        || r["method_end_line"].as_u64() != Some(e.end_line as u64) {
        return vec![UNKNOWN.into()];
    }
    let p = &r["parameter_occurrences"];
    let Some(items) = p["parameters"].as_array() else { return vec![UNKNOWN.into()]; };
    let Some(tokens) = p["scanned_body_tokens"].as_u64() else { return vec![UNKNOWN.into()]; };
    if p["version"] != "vb-parameter-occurrences-v1" || p["status"] != "available"
        || items.len() > 64 || tokens > 100_000 { return vec![UNKNOWN.into()]; }
    let mut names = std::collections::HashSet::new();
    let mut zero = Vec::new();
    let mut sum = 0_u64;
    for item in items {
        let (Some(name), Some(line), Some(count)) = (item["identifier"].as_str(), item["declaration_line"].as_u64(), item["body_identifier_occurrences"].as_u64()) else { return vec![UNKNOWN.into()]; };
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control)
            || !names.insert(name.to_lowercase()) || line < e.start_line as u64 || line > e.end_line as u64
            || count > tokens { return vec![UNKNOWN.into()]; }
        sum += count;
        if sum > tokens { return vec![UNKNOWN.into()]; }
        if count == 0 { zero.push((name, line)); }
    }
    if !details {
        // JSON-quoted identifiers remain intact; omit whole facts rather than clip names.
        // This shared document qualification stays separate from substantive rule excerpts.
        let mut facts = String::new();
        let mut shown = 0usize;
        for (name, line) in zero.iter().take(2) {
            let fact = format!("{} (declaration line {line})", serde_json::to_string(name).expect("string serializes"));
            let separator = if facts.is_empty() { "" } else { "; " };
            if facts.len() + separator.len() + fact.len() > 320 { break; }
            facts.push_str(separator);
            facts.push_str(&fact);
            shown += 1;
        }
        let named = if facts.is_empty() { String::new() } else { format!(" Zero body-token matches: {facts}.") };
        return vec![format!("Parameter identifier inventory: {} declarations, {} zero body-token matches ({shown} shown, {} omitted).{named} Syntax only, not read/unused proof; reconcile inferred data-flow claims with these facts. Full counts/scope: get_chunk for this document (outcome_evidence.return_paths.parameter_occurrences).", items.len(),zero.len(),zero.len().saturating_sub(shown))];
    }
    let mut lines = vec![format!("Parameter identifier inventory: {} declarations checked; {} have zero body IdentifierToken occurrences ({} shown, {} omitted). Signature/defaults/comments/literal text excluded. Captures/writes/ByRef/NameOf/member collisions count. Positive counts are not reads or bound references; zero is not runtime-unused proof. VB only; limits 64 parameters/100000 body tokens. Full metadata: get_chunk for this document.", items.len(), zero.len(), zero.len().min(8), zero.len().saturating_sub(8))];
    for (name, line) in zero.into_iter().take(8) {
        lines.push(format!("Source token fact: parameter {} declared at line {line} has zero body identifier occurrences. Do not assert a body read from declaration alone; this is syntax-only evidence.", serde_json::to_string(name).expect("string serializes")));
    }
    lines
}

#[cfg(test)]
mod parameter_inventory_tests {
    use super::*;
    fn evidence() -> ReturnPathEvidence {
        ReturnPathEvidence { source_blake3: "source".into(), start_line: 2, end_line: 9, unavailable_reason: None,
            result: Some(serde_json::json!({"version":"vb-return-paths-v1", "method_start_line":2,"method_end_line":9,
                "status":"available","structural_complete":true,"normal_fallthrough_paths":0,"paths":[{"conditions":[],"return_line":8,"return_expression":"1","normal_completion_required":true}],
                "parameter_occurrences":{"version":"vb-parameter-occurrences-v1","status":"available","scanned_body_tokens":12,
                    "parameters":[{"identifier":"choice","declaration_line":2,"body_identifier_occurrences":0},
                        {"identifier":"value","declaration_line":2,"body_identifier_occurrences":3}]}})) }
    }
    fn summary(e: &ReturnPathEvidence) -> String {
        parameter_qualification(Some(e),Some("source"),Some((2,9))).join("\n")
    }
    #[test]
    fn independent_inventory_survives_unsupported_flow_and_never_claims_positive_read() {
        let mut e=evidence();
        e.result.as_mut().unwrap()["status"]=serde_json::json!("unavailable");
        e.result.as_mut().unwrap()["structural_complete"]=serde_json::json!(false);
        e.result.as_mut().unwrap()["paths"]=serde_json::json!([]);
        let text=summary(&e);
        assert!(valid(e.result.as_ref().unwrap(),2,9));
        assert!(checklist(Some(&e),Some("source"),Some((2,9)),8)[0].contains("unavailable"));
        assert!(text.contains("2 declarations checked; 1 have zero"));
        assert!(text.contains("parameter \"choice\" declared at line 2"));
        assert!(!text.contains("parameter \"value\""));
        assert!(text.contains("Positive counts are not reads"));
    }
    #[test]
    fn malformed_optional_evidence_does_not_reject_paths_or_invent_absence() {
        for change in [serde_json::json!(null),serde_json::json!({}),serde_json::json!({"version":"vb-parameter-occurrences-v1","status":"unknown","scanned_body_tokens":0,"parameters":[]})] {
            let mut e=evidence(); e.result.as_mut().unwrap()["parameter_occurrences"]=change;
            assert!(valid(e.result.as_ref().unwrap(),2,9));
            assert!(summary(&e).contains("UNKNOWN"));
        }
        for (field,value) in [("body_identifier_occurrences",serde_json::json!(13)),("declaration_line",serde_json::json!(10)),("identifier",serde_json::json!(""))] {
            let mut e=evidence(); e.result.as_mut().unwrap()["parameter_occurrences"]["parameters"][0][field]=value;
            assert!(summary(&e).contains("UNKNOWN"));
        }
        let mut e=evidence(); e.result.as_mut().unwrap()["parameter_occurrences"]["scanned_body_tokens"]=serde_json::json!(100001);
        assert!(summary(&e).contains("UNKNOWN"));
        let mut e=evidence(); let duplicate=e.result.as_ref().unwrap()["parameter_occurrences"]["parameters"][0].clone();
        e.result.as_mut().unwrap()["parameter_occurrences"]["parameters"].as_array_mut().unwrap().push(duplicate);
        assert!(summary(&e).contains("UNKNOWN"));
    }
    #[test]
    fn stale_source_range_legacy_and_bounded_negative_display_are_explicit() {
        let mut e=evidence();
        for (hash,range) in [("edit",(2,9)),("source",(3,10))] {
            assert!(parameter_qualification(Some(&e),Some(hash),Some(range))[0].contains("withheld"));
        }
        assert!(parameter_qualification(None,None,None)[0].contains("VB-only"));
        e.result.as_mut().unwrap()["parameter_occurrences"]["parameters"] = serde_json::json!((0..12).map(|n|serde_json::json!({"identifier":format!("p{n}"),"declaration_line":2,"body_identifier_occurrences":0})).collect::<Vec<_>>());
        let text=summary(&e);
        assert!(text.contains("8 shown, 4 omitted"));
        assert_eq!(text.matches("Source token fact:").count(),8);
        let raw=serde_json::to_string(&e).unwrap();
        assert!(raw.contains("p11"));
        assert!(!text.contains("p11"));
        let brief=parameter_retrieval_qualification(Some(&e),Some("source"),Some((2,9)));
        assert_eq!(brief.len(),1);assert!(brief[0].len()<700);
        assert!(brief[0].contains("12 declarations, 12 zero body-token matches"));
        assert!(brief[0].contains("\"p0\" (declaration line 2)"));
        assert!(brief[0].contains("2 shown, 10 omitted"));
        assert!(!brief[0].contains("p2"));
        assert!(parameter_retrieval_qualification(None,None,None).is_empty());
        e.result.as_mut().unwrap().as_object_mut().unwrap().remove("parameter_occurrences");
        assert!(parameter_retrieval_qualification(Some(&e),Some("source"),Some((2,9))).is_empty());
    }
    #[test]
    fn retrieval_names_are_bounded_unicode_facts_and_stale_facts_stay_withheld() {
        let mut e = evidence();
        let name = "\u{00C5}ngstr\u{00F6}m";
        e.result.as_mut().unwrap()["parameter_occurrences"]["parameters"][0]["identifier"] = serde_json::json!(name);
        let raw = serde_json::to_string(&e).unwrap();
        let text = parameter_retrieval_qualification(Some(&e), Some("source"), Some((2,9))).join("\n");
        assert!(text.contains(&format!("\"{name}\" (declaration line 2)")));
        assert!(text.contains("1 shown, 0 omitted"));
        assert!(text.contains("not read/unused proof"));
        assert!(!text.contains("\"value\""));
        assert_eq!(raw, serde_json::to_string(&e).unwrap());
        for (hash, range) in [("edit", (2,9)), ("source", (3,10))] {
            let stale = parameter_retrieval_qualification(Some(&e), Some(hash), Some(range)).join("\n");
            assert!(stale.contains("withheld"));
            assert!(!stale.contains(name));
        }
        // Maximum UTF-8 identifiers remain complete under the byte budget;
        // their bytes must not be split by display clipping.
        let first = "\u{00E9}".repeat(64);
        let second = "\u{00F6}".repeat(64);
        e.result.as_mut().unwrap()["parameter_occurrences"]["parameters"] = serde_json::json!([
            {"identifier":first,"declaration_line":2,"body_identifier_occurrences":0},
            {"identifier":second,"declaration_line":2,"body_identifier_occurrences":0}]);
        let text = parameter_retrieval_qualification(Some(&e), Some("source"), Some((2,9))).join("\n");
        assert!(text.len() < 700);
        assert!(text.contains(&first));
        // Both facts fit exactly within 320 bytes; no Unicode identifier is clipped.
        assert!(text.contains(&second));
        assert!(text.contains("2 shown, 0 omitted"));
    }

}
#[cfg(test)]
mod complete_predicate_rendering_tests {
    use super::*;
    use serde_json::json;

    fn evidence(paths: Value) -> ReturnPathEvidence {
        ReturnPathEvidence {
            source_blake3: "source".into(),
            start_line: 1,
            end_line: 100,
            unavailable_reason: None,
            result: Some(json!({
                "version":"vb-return-paths-v1", "status":"available", "structural_complete":true,
                "method_start_line":1, "method_end_line":100, "normal_fallthrough_paths":0,
                "paths":paths, "unsupported":[], "unsupported_omitted":0
            })),
        }
    }
    fn path(conditions: Value) -> Value {
        json!({"conditions":conditions,"return_line":90,"return_expression":"False","normal_completion_required":true})
    }
    fn render(e: &ReturnPathEvidence) -> Vec<String> {
        checklist(Some(e), Some("source"), Some((1, 100)), 8)
    }
    fn records(lines: &[String]) -> Vec<Value> {
        lines
            .iter()
            .filter(|line| line.starts_with("Source-path test checklist "))
            .map(|line| serde_json::from_str(line.split_once("): ").unwrap().1).unwrap())
            .collect()
    }
    #[test]
    fn conjunction_after_old_byte_cut_and_case_groups_survive_exactly() {
        let condition = "enabled AndAlso ".repeat(90) + "tail_required";
        let p = path(json!([
            {"kind":"if","line":2,"expression":condition,"outcome":"false"},
            {"kind":"case","line":4,"expression":"category","outcome":"no_match","case_line":5,"case_expression":"1, 3 To 5"},
            {"kind":"case","line":4,"expression":"category","outcome":"match","case_line":8,"case_expression":"7, 9"}
        ]));
        assert!(serde_json::to_string(&p).unwrap().len() > 1024);
        let e = evidence(json!([p.clone()]));
        let before = serde_json::to_vec(&e).unwrap();
        let lines = render(&e);
        assert_eq!(records(&lines), vec![p]);
        assert!(lines[0].contains("model_claim_alignment=not_assessed"));
        assert!(lines[0].contains("path_feasibility=not_assessed"));
        assert_eq!(serde_json::to_vec(&e).unwrap(), before);
    }
    #[test]
    fn oversize_history_is_omitted_whole_not_clipped_and_next_history_can_fit() {
        let conditions = (0..4)
            .map(|_| {
                json!({"kind":"if","line":2,
            "expression":"\u{00e9}".repeat(1500),"outcome":"true"})
            })
            .collect::<Vec<_>>();
        let small = path(json!([]));
        let e = evidence(json!([path(json!(conditions)), small.clone()]));
        let lines = render(&e);
        assert_eq!(records(&lines), vec![small]);
        assert!(lines[0].contains("2 recorded, 1 shown, 1 omitted"));
        assert!(lines[0].contains("omitted original path ordinals: [1]"));
        assert!(lines[0].contains("page to end"));
        assert!(!lines.join("\n").contains("shortened"));
    }
    #[test]
    fn compact_preview_omits_whole_large_record_and_keeps_recovery() {
        let p = path(
            json!([{"kind":"if","line":2,"expression":"enabled AndAlso ".repeat(100),"outcome":"false"}]),
        );
        let e = evidence(json!([p]));
        let lines = checklist(Some(&e), Some("source"), Some((1, 100)), 2);
        assert!(records(&lines).is_empty());
        assert!(lines[0].contains("1 recorded, 0 shown, 1 omitted"));
        assert!(lines[0].contains("full_document"));
        assert!(lines.iter().map(String::len).sum::<usize>() < 1200);
    }
    #[test]
    fn multiline_unicode_and_json_escapes_round_trip() {
        let p = path(
            json!([{"kind":"if","line":2,"expression":"name = \"\u{00e9}\" AndAlso _\r\n permitted", "outcome":"true"}]),
        );
        assert_eq!(records(&render(&evidence(json!([p.clone()])))), vec![p]);
    }
    #[test]
    fn unsupported_preserves_complete_records_and_separate_producer_omissions() {
        let mut e = evidence(json!([]));
        let r = e.result.as_mut().unwrap();
        r["status"] = json!("unavailable");
        r["structural_complete"] = json!(false);
        let issue = json!({"kind":"unsupported_statement","line":17,"reason":"Try/Catch path is not expanded"});
        r["unsupported"] =
            json!([issue.clone(),{"kind":"limit","line":18,"reason":"x".repeat(5000)}]);
        r["unsupported_omitted"] = json!(4);
        let lines = render(&e);
        assert!(lines[0].contains("unavailable"));
        assert!(lines[0].contains("2 retained, 1 shown, 1 display-omitted"));
        assert!(lines[0].contains("producer-omitted 4"));
        assert!(lines[1].ends_with(&serde_json::to_string(&issue).unwrap()));
        assert_eq!(lines.len(), 2);
    }
    #[test]
    fn unknown_diagnostics_are_not_empty_and_stale_facts_stay_withheld() {
        let mut e = evidence(json!([]));
        e.result
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("unsupported");
        assert!(render(&e)[0].contains("inventory unknown"));
        assert!(checklist(Some(&e), Some("changed"), Some((1, 100)), 8)[0].contains("withheld"));
        assert!(checklist(Some(&e), Some("source"), Some((2, 101)), 8)[0].contains("withheld"));
    }
}

#[cfg(test)]
mod unsupported_display_bound_tests {
    use super::*;
    #[test]
    fn large_legacy_diagnostic_inventory_keeps_exact_count_but_bounds_ordinal_preview() {
        let issue=serde_json::json!({"kind":"unsupported","line":2,"reason":"x".repeat(5000)});
        let e=ReturnPathEvidence {source_blake3:"source".into(),start_line:1,end_line:3,unavailable_reason:None,
            result:Some(serde_json::json!({"version":"vb-return-paths-v1","method_start_line":1,"method_end_line":3,
                "status":"unavailable","structural_complete":false,"paths":[],"unsupported":vec![issue;1000],"unsupported_omitted":4}))};
        let before=serde_json::to_vec(&e).unwrap();
        let lines=checklist(Some(&e),Some("source"),Some((1,3)),8);
        assert_eq!(lines.len(),1);
        assert!(lines[0].contains("1000 retained, 0 shown, 1000 display-omitted"));
        assert!(lines[0].contains("984 further omitted ordinals not listed"));
        assert!(lines[0].contains("producer-omitted 4"));
        assert!(lines[0].contains("full_document"));
        assert!(lines[0].len()<1600);
        assert!(!lines[0].contains(&"x".repeat(20)));
        assert_eq!(serde_json::to_vec(&e).unwrap(),before);
    }
}
