//! Source-version evidence for persisted inferred business rules.
use crate::services::business_logic_service::{detect_language, extract_logic_methods};
use engram_core::ContentHash;
use std::{collections::HashMap, io::Read, path::Path};

type MethodHashes = Vec<(String, String, u32, u32)>;

fn document_identity(document: &str) -> Option<(&str, &str, &str)> {
    let fqn = document.lines().find_map(|line| line.strip_prefix("# "))?;
    let path = document.lines().find_map(|line| {
        line.strip_prefix("_Source: ")
            .and_then(|value| value.strip_suffix('_'))
    })?;
    let hash = document.lines().find_map(|line| {
        line.strip_prefix("**Analysis method hash**: `")
            .and_then(|value| value.strip_suffix('`'))
    })?;
    Some((fqn, path, hash))
}

pub(super) fn utf8_prefix(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Mutually exclusive evidence categories over dependency entries, not files.
/// Only a retained source_verified status with a complete file fingerprint can
/// support the matching-fingerprint category after the shared source audit.
fn helper_evidence_summary(
    dependencies: &[crate::services::business_outcome_dependencies::OutcomeDependency],
) -> String {
    let (mut matched, mut stale, mut unavailable, mut not_requested, mut budget, mut unknown) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for dependency in dependencies {
        let complete_fingerprint = dependency
            .helper_file
            .as_deref()
            .is_some_and(|s| !s.is_empty())
            && dependency
                .helper_file_hash
                .as_deref()
                .is_some_and(|s| !s.is_empty());
        match dependency.evidence_status.as_str() {
            "source_verified" if complete_fingerprint => matched += 1,
            "stale_or_unavailable" if complete_fingerprint => stale += 1,
            "unavailable" => unavailable += 1,
            "not_requested" => not_requested += 1,
            "budget_omitted" => budget += 1,
            _ => unknown += 1,
        }
    }
    let total = dependencies.len();
    format!(
        "Helper source evidence ({total} dependency entries; not unique helper files): {matched} source_verified_with_matching_file_fingerprint, {stale} stale_or_unavailable, {unavailable} unavailable, {not_requested} not_requested, {budget} budget_omitted, {unknown} incomplete_or_unknown. Only {matched} of {total} entries have verified matching helper-file fingerprints; {} are not established fresh. stale_or_unavailable combines fingerprint mismatch, unreadable source or recheck budget failure; unavailable/not_requested/budget_omitted/incomplete_or_unknown are not freshness passes. Fingerprint matches do not establish normal completion or compilation binding.",
        total.saturating_sub(matched)
    )
}

/// Derived retrieval view only. Stored fields and raw get_chunk stay unchanged.
/// Classify complete exact rule identities before any display truncation.
fn claim_presentation(document: &str) -> String {
    use crate::services::business_rule_diagnostics::{associate, exact_document_rules, from_document};
    let mapping = from_document(document);
    let exact = exact_document_rules(mapping.as_ref(), document);
    let mut in_checks = false;
    let mut known_rule_warning = false;
    for line in document.lines() {
        if line.starts_with("## ") { in_checks = line.trim() == "## Source checks requiring review"; continue; }
        if in_checks {
            let warning = line.trim().strip_prefix("- ").unwrap_or(line.trim());
            if warning.strip_prefix("Rule ").and_then(|tail| tail.split_once(':')).is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) { known_rule_warning = true; }
        }
    }
    // Older metadata stored only digests: valid attribution, but no exact display boundaries.
    // Keep its warning-free prose unverified; mixed/current/malformed records stay fail-closed.
    let diagnostic_free_digest_legacy = mapping.as_ref().is_some_and(|m| {
        !m.rules.is_empty() && m.rules.iter().all(|r| {
            r.displayed_rule.is_none() && r.warnings.is_empty()
                && r.displayed_rule_digest.strip_prefix("blake3-raw-utf8:").is_some_and(|digest| {
                    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
                })
        })
    });
    let invalid_metadata = document.lines().any(|l| l.starts_with("**Rule source diagnostics v1**:"))
        && exact.is_none() && !diagnostic_free_digest_legacy;
    let suppress_unknown = exact.is_none() && (known_rule_warning || invalid_metadata);
    let mut records = Vec::new();
    let mut withheld = 0usize;
    let mut inferred = 0usize;
    if let Some(rules) = exact.as_ref() {
        for (rule, identity) in rules.iter().zip(&mapping.as_ref().expect("exact mapping").rules) {
            let association = associate(mapping.as_ref(), rule);
            let ordinal = identity.source_rule_ordinal;
            if association.blocks_outcome {
                withheld += 1;
                records.push(format!("- Rule {ordinal}: SOURCE_VALIDATION_REQUIRED; proposed claim withheld. Exact diagnostic identity retained; inspect original rule diagnostics with get_chunk. Diagnostic prose is not repeated here because it may quote the withheld claim.\n"));
            } else {
                inferred += 1;
                records.push(format!("- Rule {}: INFERRED_UNVERIFIED; no mapped diagnostic is not semantic verification. {rule}\n", ordinal));
            }
        }
    }
    if exact.is_none() && !suppress_unknown {
        let mut in_rules = false;
        let mut legacy = String::from("Legacy rule prose: INFERRED_UNVERIFIED / ASSOCIATION_UNKNOWN; no exact per-rule identity or diagnostic ownership claimed (uncounted legacy rules):\n");
        for line in document.lines() {
            if line.starts_with("## ") { in_rules = line.trim() == "## Business Rules"; continue; }
            if line.starts_with("_Source: ") { in_rules = false; }
            if in_rules {
                if legacy.len() >= 128 * 1024 { legacy.push_str("\n[Legacy prose shortened; recover raw document.]\n"); break; }
                legacy.push_str(utf8_prefix(line, (128 * 1024usize).saturating_sub(legacy.len())));
                legacy.push('\n');
            }
        }
        records.push(legacy);
    }
    let association = if exact.is_some() { "exact displayed-rule mapping" } else if suppress_unknown { "ASSOCIATION_UNKNOWN; rule prose withheld because known rule diagnostics or invalid identity metadata cannot be safely associated; no warning ownership guessed" } else { "ASSOCIATION_UNKNOWN; legacy rule prose remains unverified, not an exact-count inventory" };
    let counts = if exact.is_some() {
        format!("{withheld} diagnosed withheld, {inferred} inferred/unverified")
    } else {
        "counts unknown".into()
    };
    let mut out = format!("Rule claims: {counts}; {association}. Raw original fields/rules/diagnostics: get_chunk for this document. No remaining claim is verified.\n");
    let mut records_shown = 0usize;
    for record in &records {
        if out.len().saturating_add(record.len()) > 128 * 1024 { break; }
        out.push_str(record);
        records_shown += 1;
    }
    if records_shown < records.len() {
        out.push_str(&format!("\n[Claim records shown {records_shown}, omitted {}; recover full raw evidence.]\n", records.len() - records_shown));
    }
    out.push_str("\nOther analysis prose (raw inference, not validated; document-level diagnostics may apply):\n");
    let mut section = "";
    for line in document.lines() {
        if line.starts_with("## ") { section = line.trim(); }
        if matches!(section, "## Business Rules" | "## Source checks requiring review") { continue; }
        if line.starts_with("_Source: ") || line.starts_with("# ")
            || line.starts_with("**Extraction provenance**:") || line.starts_with("**Analysis method hash**:")
            || line.starts_with("**Outcome dependencies v1**:") || line.starts_with("**Rule source diagnostics v1**:")
            || line.starts_with("**Overload declaration**:") || line.starts_with("**Member kind**:") { continue; }
        if out.len() >= 128 * 1024 {
            out.push_str("\n[Other analysis prose omitted at presentation budget; recover full raw evidence.]\n");
            break;
        }
        out.push_str(utf8_prefix(line, (128 * 1024usize).saturating_sub(out.len())));
        out.push('\n');
    }
    out
}

/// Ask JSON and Markdown receive the same classified view, never unqualified
/// raw Business Rules separated from their known exact diagnostics.
pub(super) fn substantive_excerpt(document: &str) -> String {
    let view = claim_presentation(document);
    let mut excerpt: String = view.chars().take(1200).collect();
    if view.chars().count() > 1200 {
        excerpt.push_str("\n[Substantive excerpt shortened; get_chunk returns full raw evidence.]");
    }
    excerpt
}

/// Expose every analysis field without burying later fields behind rule prose or
/// large diagnostic metadata. Presence is not review or semantic verification.
/// Recovery is an exact, hash-bound range of the unchanged stored document.
pub(super) fn claim_review_guidance(project_id: &str, doc_id: &str, document: &str) -> Vec<String> {
    const FIELDS: [(&str, &str); 6] = [
        ("purpose", "**Purpose**:"),
        ("steps", "## Steps"),
        ("business_rules", "## Business Rules"),
        ("data_flow", "## Data Flow"),
        ("error_handling", "## Error Handling"),
        ("side_effects", "## Side Effects"),
    ];
    let mut counts = [0usize; 6];
    let mut first = [0usize; 6];
    let mut last_nonempty = 0usize;
    let mut before_footer = 0usize;
    let mut footer = 0usize;
    let mut footer_count = 0usize;
    for (index, line) in document.lines().enumerate() {
        let number = index + 1;
        for (field, (_, heading)) in FIELDS.iter().enumerate() {
            let matches = if field == 0 { line.starts_with(*heading) } else { line.trim_end() == *heading };
            if matches {
                counts[field] += 1;
                if first[field] == 0 { first[field] = number; }
            }
        }
        if line.starts_with("_Source: ") && line.ends_with('_') {
            footer_count += 1;
            footer = number;
            before_footer = last_nonempty;
        }
        if !line.trim().is_empty() { last_nonempty = number; }
    }
    let inventory = FIELDS.iter().enumerate().map(|(index, (name, _))| {
        match counts[index] {
            0 => format!("{name}=not_identified"),
            1 => format!("{name}=present(line {})", first[index]),
            count => format!("{name}=ambiguous({count} headings)"),
        }
    }).collect::<Vec<_>>().join("; ");
    let mut result = vec![format!(
        "Model-claim field inventory (presence, not review): {inventory}. Numbered-rule diagnostics do not verify the other fields. Review each populated field separately; missing headings do not establish absence of claims."
    )];
    let start = first.into_iter().filter(|line| *line > 0).min();
    // Do not invent a compact range for duplicated headings or a nonterminal
    // source footer. The complete original remains available via full_document.
    if let Some(start) = start.filter(|_| counts.iter().all(|count| *count <= 1)) {
        if footer_count == 1 && footer == last_nonempty && before_footer >= start {
            let recovery = serde_json::json!({
                "project_id": project_id, "namespace": "business_logic", "doc_id": doc_id,
                "citation": {"unit":"lines", "start":start, "end":before_footer,
                    "expected_raw_hash":format!("blake3-raw-utf8:{}", blake3::hash(document.as_bytes()))}
            });
            result.push(format!("analysis_prose (raw unverified fields; complete diagnostics remain in full_document): get_chunk({recovery}). Follow continuation if present; this range is not a semantic approval."));
        }
    }
    let provenance: Vec<_> = document.lines()
        .filter_map(|line| line.strip_prefix("**Extraction provenance**:"))
        .take(2)
        .collect();
    if !provenance.is_empty() {
        let parsed = (provenance.len() == 1 && provenance[0].len() <= 16 * 1024)
            .then(|| provenance[0].trim().strip_prefix('`').and_then(|s| s.strip_suffix('`')))
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        let current = crate::services::business_logic_service::MEMBER_PROMPT_VERSION;
        match parsed.as_ref().and_then(|p| p.get("origin")).and_then(serde_json::Value::as_str) {
            Some("llm") => {
                let recorded = parsed.as_ref().and_then(|p| p.get("prompt_version")).and_then(serde_json::Value::as_str);
                if recorded != Some(current) {
                    result.push(format!(
                        "Extraction contract differs or is unknown: this stored analysis does not identify the current prompt contract `{current}`. Matching source bytes do not mean current extraction instructions were applied. Use analyze_business_logic to regenerate if current-contract evidence is required; a contract mismatch alone does not prove a claim false."
                    ));
                }
            }
            Some("deterministic") => {}
            _ => result.push("Extraction contract is unknown: stored provenance is malformed, ambiguous or has an unrecognized origin; do not infer its generator from current configuration.".to_string()),
        }
    }
    result
}

#[cfg(test)]
mod claim_field_guidance_tests {
    use super::*;

    fn document(prefix: &str, rules: &str) -> String {
        format!("# Generic.Check\n{prefix}\n**Purpose**: Decide whether to continue.\n\n## Steps\n1. Inspect the row.\n\n## Business Rules\n{rules}\n\n## Data Flow\nReads a row.\n\n## Error Handling\nThe model claims every skipped fallback returns False.\n\n## Side Effects\nNo direct writes are visible.\n\n_Source: Check.vb_\n")
    }

    #[test]
    fn later_fields_have_exact_hash_bound_recovery_after_large_metadata_and_rule_blocks() {
        let doc = document(&format!("**Outcome dependencies v1**: `{}`", "x".repeat(30_000)), &"- Inferred rule.\n".repeat(400));
        let original = doc.clone();
        let guidance = claim_review_guidance("project-a", "document-a", &doc);
        assert_eq!(guidance.len(), 2);
        assert!(guidance[0].contains("error_handling=present("));
        assert!(guidance[0].contains("side_effects=present("));
        assert!(guidance[0].contains("Numbered-rule diagnostics do not verify the other fields"));
        let arguments = guidance[1].split_once("get_chunk(").unwrap().1.split_once("). Follow").unwrap().0;
        let args: serde_json::Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(args["project_id"], "project-a");
        assert_eq!(args["namespace"], "business_logic");
        assert_eq!(args["doc_id"], "document-a");
        assert_eq!(args["citation"]["expected_raw_hash"], format!("blake3-raw-utf8:{}", blake3::hash(doc.as_bytes())));
        let start = args["citation"]["start"].as_u64().unwrap() as usize;
        let end = args["citation"]["end"].as_u64().unwrap() as usize;
        let recovered = doc.lines().skip(start - 1).take(end - start + 1).collect::<Vec<_>>().join("\n");
        assert!(recovered.starts_with("**Purpose**:"));
        assert!(recovered.contains("The model claims every skipped fallback returns False."));
        assert!(recovered.ends_with("No direct writes are visible."));
        assert!(!recovered.contains("Outcome dependencies") && !recovered.contains("_Source:"));
        assert!(guidance.join("\n").len() < 1800);
        assert_eq!(doc, original);
    }

    #[test]
    fn raw_hash_preserves_unicode_and_crlf_instead_of_normalizing_storage() {
        let lf = document("", "- Åäö rule.");
        let crlf = lf.replace('\n', "\r\n");
        let a = claim_review_guidance("p", "d", &lf);
        let b = claim_review_guidance("p", "d", &crlf);
        assert_eq!(a[0], b[0]);
        assert_ne!(a[1], b[1]);
        assert!(b[1].contains(&format!("blake3-raw-utf8:{}", blake3::hash(crlf.as_bytes()))));
    }

    #[test]
    fn duplicate_sections_are_ambiguous_and_do_not_offer_a_guessed_range() {
        let doc = document("## Error Handling\nOther claim.", "- Rule.");
        let guidance = claim_review_guidance("p", "d", &doc);
        assert_eq!(guidance.len(), 1);
        assert!(guidance[0].contains("error_handling=ambiguous(2 headings)"));
    }

    #[test]
    fn unrecognized_layout_is_not_treated_as_absence_of_claims() {
        let guidance = claim_review_guidance("p", "d", "Legacy free-form claims.");
        assert_eq!(guidance.len(), 1);
        assert!(guidance[0].contains("error_handling=not_identified"));
        assert!(guidance[0].contains("missing headings do not establish absence of claims"));
    }

    #[test]
    fn nonterminal_source_footer_requires_full_document_recovery() {
        let doc = document("", "- Rule.") + "\nFurther unclassified prose.\n";
        assert_eq!(claim_review_guidance("p", "d", &doc).len(), 1);
    }

    #[test]
    fn old_prompt_evidence_is_distinguished_from_current_configuration_without_rewriting_claims() {
        let current = crate::services::business_logic_service::MEMBER_PROMPT_VERSION;
        for (origin, version, mismatch) in [
            ("llm", "previous-contract", true),
            ("llm", current, false),
            ("deterministic", "", false),
        ] {
            let provenance = serde_json::json!({"origin":origin,"prompt_version":version,"requested_model":"same-model"});
            let doc = document(&format!("**Extraction provenance**: `{provenance}`"), "- Raw hypothesis.");
            let raw = doc.clone();
            let guidance = claim_review_guidance("p", "d", &doc).join("\n");
            assert_eq!(guidance.contains("Extraction contract differs or is unknown"), mismatch, "{guidance}");
            assert_eq!(doc, raw);
            assert!(!guidance.contains("semantic PASS"));
        }
    }

    #[test]
    fn malformed_or_duplicate_provenance_cannot_claim_current_extraction() {
        for prefix in [
            "**Extraction provenance**: `not json`",
            "**Extraction provenance**: `{\"origin\":\"llm\"}`\n**Extraction provenance**: `{\"origin\":\"deterministic\"}`",
        ] {
            let guidance = claim_review_guidance("p", "d", &document(prefix, "- Raw hypothesis.")).join("\n");
            assert!(guidance.contains("Extraction contract is unknown"), "{guidance}");
        }
    }
}

/// Bounded excerpts retain source status and a retrievable document identity.
pub(super) fn render_matches(
    project_id: &str,
    question: &str,
    root: &Path,
    analyses: Vec<(String, String, f32, String)>,
    budget: usize,
) -> String {
    let mut audit = SourceAudit::default();
    let matched = analyses.len();
    let mut displayed = 0;
    let mut truncated = 0;
    let mut out = format!(
        "# Business-logic matches for '{}'\nEvidence excerpts, not complete rule inventories. Limits: 8 KiB content per document, 48 KiB total response.\n",
        utf8_prefix(question, 1024)
    );
    if question.len() > 1024 {
        out.push_str("Query display truncated at 1024 bytes.\n");
    }
    for (doc_id, path, score, content) in analyses {
        let status = audit.describe(&content, root);
        let mut qualifications = audit.qualifications(&content, root);
        qualifications.extend(claim_review_guidance(project_id, &doc_id, &content));
        let recovery = serde_json::json!({"project_id":project_id,"doc_id":doc_id,"namespace":"business_logic"});
        let presentation = claim_presentation(&content);
        let excerpt = utf8_prefix(&presentation, 8 * 1024);
        let is_truncated = excerpt.len() < presentation.len();
        // Critical qualifications are independent of the bounded raw excerpt.
        let summary = qualifications
            .iter()
            .map(|q| format!("- {q}\n"))
            .collect::<String>();
        let mut card = format!(
            "\n## #{} {} (score {score:.3})\nSource status: {}\nfull_document: get_chunk({recovery})\nQualification summary (not semantic verification):\n{summary}\n{excerpt}\n",
            displayed + 1,
            utf8_prefix(&path, 512),
            utf8_prefix(&status, 1024)
        );
        if let Some((source_path, start, end)) = audit.verified_range(&content, root) {
            let source = serde_json::json!({"project_id":project_id,"file_path":source_path,
                "line_start":start,"line_end":end,"context_lines":0});
            card.push_str(&format!("source_body: get_full_method_body({source})\nCurrent verified source range; re-query after source edits. Range retrieval does not establish indexed caller coverage.\n"));
            if let Some((fqn, _, _)) = document_identity(&content) {
                // The source audit resolved one exact member. Use its current
                // declaration line to disambiguate overloads in the graph;
                // a direct source range cannot itself expand indexed callers.
                let mut parts = fqn.rsplit('.');
                let name = parts.next().unwrap_or("");
                let class = parts.next().unwrap_or("");
                if !name.is_empty() {
                    let mut caller = serde_json::json!({"project_id":project_id,
                        "file_path":source_path,"method_name":name,"line":start,
                        "include_full_body":true,"include_caller_bodies":false,"max_callers":3,
                        "include_business_logic":false,"include_history":false,"output_json":true});
                    if !class.is_empty() { caller["class_name"] = class.into(); }
                    card.push_str(&format!("caller_context: get_method_edit_context({caller})\nCaller scope is not established by this method-scoped analysis. Inspect caller-supplied arguments and enclosing guards against the parameter declarations/defaults. This follow-up requests bounded source-checked call excerpts, not exhaustive callers or proven argument binding; graph resolution may be unavailable. Recover a full caller body if its excerpt is truncated.\n"));
                }
            }
        }
        if !content
            .lines()
            .any(|line| line.starts_with("**Extraction provenance**:"))
        {
            card.push_str("Extraction provenance: unknown (legacy document; current model configuration does not identify its generator).\n");
        }
        if path.len() > 512 || status.len() > 1024 {
            card.push_str(
                "INCOMPLETE: path/source-status display shortened; retrieve the source document.\n",
            );
        }
        if is_truncated {
            card.push_str("INCOMPLETE: document excerpt truncated at 8 KiB; use full_document for the stored rules.\n");
        }
        if out.len() + card.len() + 512 > budget {
            break;
        }
        displayed += 1;
        truncated += usize::from(is_truncated);
        out.push_str(&card);
    }
    out.push_str(&format!("\nDisplay coverage: matched={matched}, displayed={displayed}, omitted={}, truncated_documents={truncated}. Counts apply to retrieved matches, not the entire corpus.\n", matched - displayed));
    if displayed < matched {
        out.push_str("INCOMPLETE: total response budget reached; narrow the query to recover omitted matches.\n");
    }
    out
}

#[derive(Default)]
pub(super) struct SourceAudit {
    files: HashMap<String, Result<MethodHashes, String>>,
    raw_file_hashes: HashMap<String, String>,
    bytes_read: u64,
    outcomes: crate::services::business_outcome_dependencies::OutcomeAudit,
}

impl SourceAudit {
    /// Full-document qualifications must survive query windows and excerpts.
    /// They are independent of caller body-hash verification, not extra stale
    /// source counts or proof of semantic correctness.
    pub(super) fn qualifications(&mut self, document: &str, root: &Path) -> Vec<String> {
        use crate::services::business_outcome_dependencies::{
            for_rule_with_audit, from_document, render_dependencies,
        };
        let mut result = vec!["Semantic validation: not performed by retrieval; raw rules remain inferred and are not verified test outcomes.".into()];
        let mut in_checks = false;
        let mut count = 0usize;
        let mut shortened = 0usize;
        let mut shown = Vec::new();
        for line in document.lines() {
            if line.starts_with("## ") {
                in_checks = line.trim() == "## Source checks requiring review";
                continue;
            }
            if line.starts_with("_Source: ") {
                in_checks = false;
            }
            if !in_checks || line.trim().is_empty() {
                continue;
            }
            // Persisted warnings are bullet records. Continuation lines count as
            // additional displayed fragments rather than silently disappearing.
            let warning = line.trim().strip_prefix("- ").unwrap_or(line.trim());
            count += 1;
            if shown.len() < 4 {
                let excerpt = utf8_prefix(warning, 384);
                shortened += usize::from(excerpt.len() < warning.len());
                shown.push(excerpt.to_string());
            }
        }
        result.push(format!("Persisted source-check fragments: {count}; shown {}, omitted {}, shortened {shortened}. Full original warnings: get_chunk(namespace=\"business_logic\") using this evidence's document ID. No warnings is not verification.", shown.len(), count.saturating_sub(shown.len())));
        result.extend(
            shown
                .into_iter()
                .map(|warning| format!("Persisted source check: {warning}")),
        );
        let evidence = from_document(document);
        result.push(crate::services::business_reaching_context::qualify(evidence.as_ref().and_then(|e| e.reaching_context.as_ref()), None).summary);
        let range = self.verified_range(document, root);
        result.extend(self.return_path_checklist(document, root, 2));
        let parameter_hash = range.as_ref().and_then(|(file, _, _)| self.raw_file_hashes.get(file)).map(String::as_str);
        result.extend(crate::services::business_return_paths::parameter_retrieval_qualification(
            evidence.as_ref().and_then(|e| e.return_paths.as_ref()), parameter_hash,
            range.as_ref().map(|(_, start, end)| (*start, *end))));
        match (evidence.as_ref(), range.as_ref()) {
            (Some(recorded), Some((path, start, _))) if recorded.caller_start_line > 0 => {
                let identity = document_identity(document);
                let recorded_owner_matches = identity.is_some_and(|(fqn, _, _)| fqn.rsplit_once('.').is_some_and(|(owner, _)| owner == recorded.caller_owner));
                if recorded.caller_file == *path && recorded.caller_start_line == *start && recorded_owner_matches {
                    result.push("Anchor position: recorded caller declaration matches current location; this does not validate each rule's condition or outcome.".into());
                } else {
                    result.push(format!("STALE_ANCHORS: recorded caller declaration {}:{} differs from current {}:{start}; body hash can match while old rule line anchors are stale. Refresh the analysis.", utf8_prefix(&recorded.caller_file,256), recorded.caller_start_line, utf8_prefix(path,256)));
                }
            }
            (Some(_), None) => result.push("Anchor position: unverified because current caller identity/body is stale, unavailable or ambiguous.".into()),
            _ => result.push("Anchor position: unknown; legacy or invalid dependency metadata does not record a verifiable caller declaration location.".into()),
        }
        // Empty rule text deliberately audits all dependencies, not one guessed
        // branch. The shared OutcomeAudit bounds helper reads across all hits.
        let (status, mut dependencies) =
            for_rule_with_audit(evidence.as_ref(), "", root, &mut self.outcomes);
        for dependency in &mut dependencies {
            if dependency.evidence_status == "source_verified"
                && (dependency.helper_file.is_none() || dependency.helper_file_hash.is_none())
            {
                dependency.evidence_status = "unverified_incomplete_fingerprint".into();
            }
        }
        result.push(format!("Outcome qualification: {status}; this is document-level conservative dependency coverage, not branch reachability."));
        if !dependencies.is_empty() {
            result.push(helper_evidence_summary(&dependencies));
            result.push(format!(
                "Outcome dependencies: {}",
                render_dependencies(&dependencies)
            ));
        }
        if let Some(evidence) = evidence {
            result.push(format!("Immediate-return dependency scope: {}{}; recorded omissions: {}. Retrieve full document for complete scope and omissions.", utf8_prefix(&evidence.scope,384), if evidence.scope.len()>384 { " [shortened]" } else { "" }, evidence.omissions.len()));
        }
        result
    }

    pub(super) fn return_path_checklist(&mut self, document: &str, root: &Path, max_paths: usize) -> Vec<String> {
        let evidence = crate::services::business_outcome_dependencies::from_document(document);
        let range = self.verified_range(document,root);
        let hash = range.as_ref().and_then(|(file,_,_)| self.raw_file_hashes.get(file)).map(String::as_str);
        crate::services::business_return_paths::checklist(evidence.as_ref().and_then(|e|e.return_paths.as_ref()),hash,
            range.as_ref().map(|(_,start,end)|(*start,*end)),max_paths)
    }

    pub(super) fn describe(&mut self, document: &str, root: &Path) -> String {
        let Some((fqn, path, hash)) = document_identity(document) else {
            return "UNVERIFIED: legacy analysis lacks method/source/hash identity; regenerate it before relying on its rules".into();
        };
        if !self.files.contains_key(path) {
            let loaded = self.load(root, path);
            self.files.insert(path.into(), loaded);
        }
        match &self.files[path] {
            Err(reason) => format!("UNVERIFIED: {reason}"),
            Ok(methods) => {
                let candidates: Vec<_> = methods
                    .iter()
                    .filter(|(name, _, _, _)| name == fqn)
                    .collect();
                let matching = candidates
                    .iter()
                    .filter(|(_, current_hash, _, _)| current_hash == hash)
                    .collect::<Vec<_>>();
                if matching.len() == 1 {
                    format!(
                        "VERIFIED_METHOD_HASH: current declaration {path}:{}; source body matches the analyzed method. Rules remain inferred, not domain-validated",
                        matching[0].2
                    )
                } else if matching.len() > 1 {
                    "UNVERIFIED: multiple current declarations share this method identity and hash"
                        .into()
                } else if candidates.is_empty() {
                    "UNVERIFIED: recorded method owner/name is absent from current source; analysis may be legacy or stale".into()
                } else {
                    "STALE: current method body differs from the stored analysis; rerun analyze_business_logic before using these rules".into()
                }
            }
        }
    }

    /// Exact source lookup also works for executable members outside the graph's
    /// method index. Never emit a plausible range for stale or ambiguous evidence.
    fn verified_range(&mut self, document: &str, root: &Path) -> Option<(String, u32, u32)> {
        if !self
            .describe(document, root)
            .starts_with("VERIFIED_METHOD_HASH:")
        {
            return None;
        }
        let (fqn, path, hash) = document_identity(document)?;
        let methods = self.files.get(path)?.as_ref().ok()?;
        let member = methods
            .iter()
            .find(|(name, current_hash, _, _)| name == fqn && current_hash == hash)?;
        Some((path.into(), member.2, member.3))
    }

    fn load(&mut self, root: &Path, path: &str) -> Result<MethodHashes, String> {
        let full = engram_core::safe_join(root, path).map_err(|error| error.to_string())?;
        let root = root.canonicalize().map_err(|error| error.to_string())?;
        let full = full
            .canonicalize()
            .map_err(|error| format!("source unavailable: {error}"))?;
        if !full.starts_with(&root) {
            return Err("source escapes the registered project".into());
        }
        let bytes = std::fs::metadata(&full)
            .map_err(|error| error.to_string())?
            .len();
        if bytes > 8 * 1024 * 1024 || self.bytes_read.saturating_add(bytes) > 64 * 1024 * 1024 {
            return Err("source verification budget exceeded (8 MiB/file, 64 MiB/query)".into());
        }
        let mut content = String::new();
        std::fs::File::open(full)
            .map_err(|error| error.to_string())?
            .take(8 * 1024 * 1024 + 1)
            .read_to_string(&mut content)
            .map_err(|error| error.to_string())?;
        let actual = content.len() as u64;
        if actual > 8 * 1024 * 1024 || self.bytes_read.saturating_add(actual) > 64 * 1024 * 1024 {
            return Err("source verification budget exceeded while reading".into());
        }
        self.bytes_read += actual;
        self.raw_file_hashes.insert(path.to_owned(), blake3::hash(content.as_bytes()).to_hex().to_string());
        let language = detect_language(path, &content);
        Ok(extract_logic_methods(&content, language)
            .into_iter()
            .map(|method| {
                (
                    format!("{}.{}", method.owner, method.name),
                    ContentHash::compute(method.body.as_bytes()).0,
                    method.start_line,
                    method
                        .start_line
                        .saturating_add(method.body.lines().count().saturating_sub(1) as u32),
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dependency_document(
        dependencies: Vec<crate::services::business_outcome_dependencies::OutcomeDependency>,
    ) -> String {
        let mut evidence = crate::services::business_outcome_dependencies::collect(
            "First()\nReturn Nothing",
            "Rules",
            "vb",
            1,
            "Rules.vb",
            None,
        );
        evidence.dependencies = dependencies;
        format!(
            "# Rules.Save\n**Outcome dependencies v1**: `{}`\n",
            serde_json::to_string(&evidence).unwrap()
        )
    }

    fn example_dependency() -> crate::services::business_outcome_dependencies::OutcomeDependency {
        crate::services::business_outcome_dependencies::collect(
            "First()\nReturn Nothing",
            "Rules",
            "vb",
            1,
            "Rules.vb",
            None,
        )
        .dependencies
        .remove(0)
    }

    #[test]
    fn unresolved_helpers_are_not_reported_as_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dependency = example_dependency();
        dependency.evidence_status = "unavailable".into();
        let document = dependency_document(vec![dependency; 4]);
        let text = SourceAudit::default()
            .qualifications(&document, tmp.path())
            .join("\n");
        assert!(
            text.contains("Helper source evidence (4 dependency entries; not unique helper files)"),
            "{text}"
        );
        assert!(text.contains("0 source_verified_with_matching_file_fingerprint, 0 stale_or_unavailable, 4 unavailable"), "{text}");
        assert!(text.contains("Only 0 of 4 entries have verified matching helper-file fingerprints; 4 are not established fresh"), "{text}");
        assert!(text.contains("Outcome qualification: blocked_pending_helper_outcome"));
        assert!(text.contains("Immediate-return dependency scope:"));
        assert!(!text.contains("Helper source freshness:"));
        // The rendered qualification must not mutate the persisted raw inference.
        assert!(
            crate::services::business_outcome_dependencies::from_document(&document)
                .unwrap()
                .dependencies
                .iter()
                .all(|d| d.evidence_status == "unavailable" && d.helper_file.is_none())
        );
    }

    #[test]
    fn helper_summary_separates_file_rechecks_from_missing_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "Module Helpers\n Sub Save()\n End Sub\nEnd Module\n";
        std::fs::write(tmp.path().join("Fresh.vb"), source).unwrap();
        std::fs::write(
            tmp.path().join("Changed.vb"),
            format!("' changed\n{source}"),
        )
        .unwrap();
        let mut fresh = example_dependency();
        fresh.evidence_status = "source_verified".into();
        fresh.helper_file = Some("Fresh.vb".into());
        fresh.helper_file_hash = Some(blake3::hash(source.as_bytes()).to_hex().to_string());
        let mut changed = fresh.clone();
        changed.helper_file = Some("Changed.vb".into());
        let mut missing = fresh.clone();
        missing.helper_file = Some("Missing.vb".into());
        let mut unavailable = example_dependency();
        unavailable.evidence_status = "unavailable".into();
        let not_requested = example_dependency();
        let mut budget = example_dependency();
        budget.evidence_status = "budget_omitted".into();
        let mut incomplete = example_dependency();
        incomplete.evidence_status = "source_verified".into();
        let mut unknown = example_dependency();
        unknown.evidence_status = "future_status".into();
        // Duplicate references to one fresh file are two entries, not two files.
        let document = dependency_document(vec![
            fresh.clone(),
            fresh,
            changed,
            missing,
            unavailable,
            not_requested,
            budget,
            incomplete,
            unknown,
        ]);
        let text = SourceAudit::default()
            .qualifications(&document, tmp.path())
            .join("\n");
        assert!(
            text.contains("9 dependency entries; not unique helper files"),
            "{text}"
        );
        assert!(text.contains("2 source_verified_with_matching_file_fingerprint, 2 stale_or_unavailable, 1 unavailable, 1 not_requested, 1 budget_omitted, 2 incomplete_or_unknown"), "{text}");
        assert!(text.contains("Only 2 of 9 entries have verified matching helper-file fingerprints; 7 are not established fresh"), "{text}");
        assert!(text.contains("stale_or_unavailable combines fingerprint mismatch, unreadable source or recheck budget failure"));
        assert!(text.contains("Outcome qualification: blocked_pending_helper_outcome"));
    }

    #[test]
    fn qualifications_bound_warnings_and_mark_legacy_or_invalid_metadata_unknown() {
        let warnings = (0..12)
            .map(|n| format!("- Step {n}: {}\n", "unknown identity ".repeat(80)))
            .collect::<String>();
        for metadata in ["", "**Outcome dependencies v1**: `invalid-json`\n"] {
            let document = format!(
                "# Rules.Save\n{metadata}## Source checks requiring review\n{warnings}\n## Business Rules\n- Validate inventory\n"
            );
            let summary = SourceAudit::default().qualifications(&document, Path::new("."));
            let text = summary.join("\n");
            assert!(text.contains("shown 4, omitted 8, shortened 4"), "{text}");
            assert!(text.contains("legacy_dependency_coverage_unknown"));
            assert!(text.contains("Anchor position: unknown"));
            assert!(text.len() < 4000, "{}", text.len());
            assert!(!text.contains("Step 4:"));
        }
    }

    #[test]
    fn substantive_excerpt_prefers_rules_and_retains_explicit_incompleteness() {
        let doc = format!(
            "# Rules.Save\n**Outcome dependencies v1**: `{}`\n**Purpose**: Apply inventory policy\n## Source checks requiring review\n- known warning\n## Steps\n1. Prepare\n## Business Rules\n- Inventory requires validation {}\n_Source: Rules.vb_",
            "metadata".repeat(1000),
            "rule detail ".repeat(200)
        );
        let excerpt = substantive_excerpt(&doc);
        assert!(excerpt.starts_with("Rule claims:"));
        assert!(excerpt.contains("ASSOCIATION_UNKNOWN"));
        assert!(excerpt.contains("Inventory requires validation"));
        assert!(!excerpt.contains("metadata") && !excerpt.contains("known warning"));
        assert!(excerpt.len() < 1500);
        let failed =
            substantive_excerpt("# Rules.Save\n**Extraction status**: failed or incomplete.\n");
        assert!(failed.contains("failed or incomplete"));
        let unicode = substantive_excerpt(&"🦀".repeat(1300));
        assert!(unicode.contains("Substantive excerpt shortened"));
    }

    fn document(source: &str) -> String {
        let method = extract_logic_methods(source, "vb").remove(0);
        format!(
            "# {}.{}\n\n**Analysis method hash**: `{}`\n\n_Source: Rules.vb_\n",
            method.owner,
            method.name,
            ContentHash::compute(method.body.as_bytes()).0
        )
    }

    fn recovery_call(rendered: &str) -> Option<serde_json::Value> {
        rendered
            .lines()
            .find_map(|line| line.strip_prefix("source_body: get_full_method_body("))
            .map(|value| serde_json::from_str(value.strip_suffix(')').unwrap()).unwrap())
    }

    #[test]
    fn verified_property_recovery_covers_both_accessors_and_tracks_moved_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "Class Rules\r\n Public Property Limit As Integer\r\n Get\r\n Return value\r\n End Get\r\n Set(input As Integer)\r\n value = input\r\n End Set\r\n End Property\r\nEnd Class\r\n";
        let doc = document(source);
        for (prefix, start, end) in [("", 2, 9), ("' unrelated header\r\n\r\n", 4, 11)] {
            std::fs::write(tmp.path().join("Rules.vb"), format!("{prefix}{source}")).unwrap();
            let rendered = render_matches(
                "project",
                "Limit",
                tmp.path(),
                vec![("doc".into(), "analysis.md".into(), 1.0, doc.clone())],
                48 * 1024,
            );
            let call =
                recovery_call(&rendered).expect("verified property source must be recoverable");
            assert_eq!(
                call,
                serde_json::json!({"project_id":"project", "file_path":"Rules.vb",
                "line_start":start,"line_end":end,"context_lines":0})
            );
            let disk = std::fs::read_to_string(tmp.path().join("Rules.vb")).unwrap();
            let body = disk
                .lines()
                .skip(start as usize - 1)
                .take((end - start + 1) as usize)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                body.contains("Set(input As Integer)\n value = input\n End Set\n End Property")
            );
            assert!(rendered.contains("does not establish indexed caller coverage"));
        }
    }

    #[test]
    fn source_recovery_is_withheld_for_stale_ambiguous_missing_and_legacy_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "Class Rules\n Public Function ReadValue() As Integer\n Return 1\n End Function\nEnd Class\n";
        let doc = document(source);
        for disk in [
            source.replace("Return 1", "Return 2"),
            format!("{source}{source}"),
        ] {
            std::fs::write(tmp.path().join("Rules.vb"), disk).unwrap();
            let rendered = render_matches(
                "project",
                "ReadValue",
                tmp.path(),
                vec![("doc".into(), "analysis.md".into(), 1.0, doc.clone())],
                48 * 1024,
            );
            assert!(recovery_call(&rendered).is_none());
        }
        std::fs::remove_file(tmp.path().join("Rules.vb")).unwrap();
        for content in [doc, "legacy summary".into()] {
            let rendered = render_matches(
                "project",
                "ReadValue",
                tmp.path(),
                vec![("doc".into(), "analysis.md".into(), 1.0, content)],
                48 * 1024,
            );
            assert!(recovery_call(&rendered).is_none());
        }
    }

    #[test]
    fn unchanged_and_edited_methods_have_distinct_evidence_status() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "Class Rules\n Public Function Save() As Integer\n  Return 1\n End Function\nEnd Class\n";
        let doc = document(source);
        std::fs::write(tmp.path().join("Rules.vb"), source).unwrap();
        let rendered = render_matches(
            "project",
            "query",
            tmp.path(),
            vec![("doc".into(), "Rules.vb".into(), 1.0, doc.clone())],
            48 * 1024,
        );
        assert!(rendered.contains("VERIFIED_METHOD_HASH"));
        assert!(rendered.contains("Extraction provenance: unknown (legacy document"));
        assert!(
            SourceAudit::default()
                .describe(&doc, tmp.path())
                .starts_with("VERIFIED_METHOD_HASH")
        );
        std::fs::write(
            tmp.path().join("Rules.vb"),
            source.replace("Return 1", "Return 2"),
        )
        .unwrap();
        assert!(
            SourceAudit::default()
                .describe(&doc, tmp.path())
                .starts_with("STALE:")
        );
        let wrong_owner = doc.replace("# Rules.Save", "# Another.Save");
        assert!(
            SourceAudit::default()
                .describe(&wrong_owner, tmp.path())
                .starts_with("UNVERIFIED:")
        );
        assert!(
            SourceAudit::default()
                .describe("legacy pack", tmp.path())
                .starts_with("UNVERIFIED:")
        );
    }

    #[test]
    fn property_hash_verification_covers_setter_and_rejects_malformed_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "Class Rules\n Public Property Limit As Integer\n Get\n Return value\n End Get\n Set(input As Integer)\n value = input\n End Set\n End Property\nEnd Class";
        let doc = document(source);
        std::fs::write(tmp.path().join("Rules.vb"), source).unwrap();
        assert!(
            SourceAudit::default()
                .describe(&doc, tmp.path())
                .starts_with("VERIFIED_METHOD_HASH")
        );
        std::fs::write(
            tmp.path().join("Rules.vb"),
            source.replace("value = input", "value = 0"),
        )
        .unwrap();
        assert!(
            SourceAudit::default()
                .describe(&doc, tmp.path())
                .starts_with("STALE:")
        );
        std::fs::write(tmp.path().join("Rules.vb"), source.replace("End Set", "")).unwrap();
        assert!(
            SourceAudit::default()
                .describe(&doc, tmp.path())
                .starts_with("UNVERIFIED:")
        );
    }

    #[test]
    fn xml_literal_terminators_cannot_verify_a_truncated_property_hash() {
        let tmp = tempfile::tempdir().unwrap();
        for opener in [
            "Dim x = <x>",
            "Dim x = Wrap(<x>",
            "Return Wrap(<x>",
            "Return prefix & <x>",
        ] {
            let truncated = format!(
                "Public ReadOnly Property Payload As Object\nGet\n{opener}\nEnd Get\nEnd Property"
            );
            let source = format!(
                "Class Rules\n{truncated}\n</x>\nReturn x\nEnd Get\nEnd Property\nEnd Class"
            );
            let doc = format!(
                "# Rules.Payload\n**Analysis method hash**: `{}`\n_Source: Rules.vb_\n",
                ContentHash::compute(truncated.as_bytes()).0
            );
            for source in [&source, &source.replace("Return x", "Return Nothing")] {
                std::fs::write(tmp.path().join("Rules.vb"), source).unwrap();
                assert!(
                    SourceAudit::default()
                        .describe(&doc, tmp.path())
                        .starts_with("UNVERIFIED:")
                );
            }
        }
    }

    #[test]
    fn multiline_string_terminators_cannot_verify_a_truncated_property_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let truncated = "Public ReadOnly Property Text As String\nGet\nDim text = \"hello\nEnd Get\nEnd Property";
        let source = format!(
            "Class Rules\n{truncated}\nworld\"\nReturn text\nEnd Get\nEnd Property\nEnd Class"
        );
        let doc = format!(
            "# Rules.Text\n**Analysis method hash**: `{}`\n_Source: Rules.vb_\n",
            ContentHash::compute(truncated.as_bytes()).0
        );
        for source in [&source, &source.replace("Return text", "Return Nothing")] {
            std::fs::write(tmp.path().join("Rules.vb"), source).unwrap();
            assert!(
                SourceAudit::default()
                    .describe(&doc, tmp.path())
                    .starts_with("UNVERIFIED:")
            );
        }
    }
    fn claim_doc(entries: Vec<(usize, String)>, warnings: &[String]) -> String {
        let mapping = crate::services::business_rule_diagnostics::build(entries.clone(), warnings);
        let rules = entries.iter().map(|(_, rule)| format!("- {rule}\n")).collect::<String>();
        format!("# Fixture.Run\n**Rule source diagnostics v1**: `{}`\n\n## Business Rules\n{rules}\n## Data Flow\nUnverified independent field\n_Source: Fixture.vb_\n", serde_json::to_string(&mapping).unwrap())
    }
    #[test]
    fn diagnosed_claims_are_withheld_before_both_excerpt_caps_without_changing_raw() {
        let bad = "IF gate THEN return wrong_owner.Allow [line 2]".to_string();
        let doc = claim_doc(vec![(1, "ordinary rule".into()), (10, bad.clone())], &["Rule 10: wrong source anchor".into()]);
        let original = doc.clone();
        let view = claim_presentation(&doc);
        assert!(view.starts_with("Rule claims: 1 diagnosed withheld, 1 inferred/unverified"));
        assert!(view.contains("Rule 10: SOURCE_VALIDATION_REQUIRED"));
        assert!(!view.contains(&bad));
        assert!(view.contains("Rule 1: INFERRED_UNVERIFIED"));
        assert!(substantive_excerpt(&doc).starts_with("Rule claims: 1 diagnosed withheld"));
        let query = render_matches("fixture", "question", Path::new("."), vec![("doc".into(),"fixture.md".into(),1.0,doc.clone())], 48 * 1024);
        assert!(query.contains("1 diagnosed withheld"));
        assert!(!query.contains(&bad));
        assert_eq!(doc, original);
        assert!(doc.contains(&bad));
    }
    #[test]
    fn mismatched_digest_ordinal_and_display_never_guess_claim_identity() {
        let doc = claim_doc(vec![(2,"A claim".into())], &["Rule 2: warning".into()]);
        for changed in [doc.replace("A claim", "Changed claim").replacen("Changed claim", "A claim", 1), doc.replace("Rule 2: warning", "Rule 3: warning"), doc.replace("- A claim\n", "- different claim\n")] {
            let view = claim_presentation(&changed);
            assert!(view.contains("ASSOCIATION_UNKNOWN"));
            assert!(!view.contains("SOURCE_VALIDATION_REQUIRED"));
        }
    }
    #[test]
    fn multiline_duplicate_rules_keep_original_ordinals_and_conservative_diagnostics() {
        let rule = "IF enabled\nTHEN allow".to_string();
        let doc=claim_doc(vec![(2,rule.clone()),(7,rule.clone())], &["Rule 7: anchor review".into()]);
        let view=claim_presentation(&doc);
        // Same exact displayed text carries the union of its diagnostics, not a guessed occurrence.
        assert!(view.contains("2 diagnosed withheld"));
        assert!(view.contains("Rule 2: SOURCE_VALIDATION_REQUIRED"));
        assert!(view.contains("Rule 7: SOURCE_VALIDATION_REQUIRED"));
        assert!(!view.contains(&rule));
    }
    #[test]
    fn warning_after_long_rule_is_counted_before_caps_and_other_fields_remain_inferred() {
        let doc=claim_doc(vec![(1,"x".repeat(9000)),(2,"bad tail".into())], &["Rule 2: tail diagnostic".into()]);
        let view=substantive_excerpt(&doc);
        assert!(view.starts_with("Rule claims: 1 diagnosed withheld, 1 inferred/unverified"));
        assert!(view.contains("Substantive excerpt shortened"));
        assert!(!view.contains("bad tail"));
        assert!(claim_presentation(&doc).contains("Other analysis prose (raw inference, not validated"));
    }

    #[test]
    fn legacy_without_rule_diagnostics_remains_visible_but_not_verified() {
        for warning in ["", "## Source checks requiring review\n- Data flow: source reference unknown\n"] {
            let doc=format!("# Old.Run\n{warning}## Business Rules\n- legacy behavior\n  continuation\n\n_Source: Old.vb_\n");
            let view=substantive_excerpt(&doc);
            assert!(view.contains("ASSOCIATION_UNKNOWN"));
            assert!(view.contains("INFERRED_UNVERIFIED"));
            assert!(view.contains("legacy behavior\n  continuation"));
            assert!(!view.contains("SOURCE_VALIDATION_REQUIRED"));
        }
    }
    #[test]
    fn digest_only_legacy_without_diagnostics_retains_unverified_multiline_prose() {
        use crate::services::business_rule_diagnostics::{build, from_document, exact_document_rules};
        let mut mapping = build([(3, "legacy claim\n  continuation".into())], &[]);
        mapping.rules[0].displayed_rule = None;
        let doc = format!("# Old.Run\n**Rule source diagnostics v1**: `{}`\n## Business Rules\n- legacy claim\n  continuation\n\n_Source: Old.vb_\n", serde_json::to_string(&mapping).unwrap());
        let original = doc.clone();
        assert!(from_document(&doc).is_some());
        assert!(exact_document_rules(Some(&mapping), &doc).is_none());
        for view in [claim_presentation(&doc), substantive_excerpt(&doc), render_matches("p", "q", Path::new("."), vec![("doc".into(), "old.md".into(), 1.0, doc.clone())], 48 * 1024)] {
            assert!(view.contains("Rule claims: counts unknown; ASSOCIATION_UNKNOWN"));
            assert!(!view.contains("0 diagnosed withheld, 0 inferred"));
            assert!(view.contains("legacy claim\n  continuation"));
            assert!(view.contains("INFERRED_UNVERIFIED") && view.contains("ASSOCIATION_UNKNOWN"));
            assert!(!view.contains("rule prose withheld"));
        }
        assert_eq!(doc, original);
    }

    #[test]
    fn legacy_metadata_warnings_corruption_mixing_and_current_mismatch_still_withhold() {
        use crate::services::business_rule_diagnostics::build;
        let base = build([(1, "private legacy claim".into()), (2, "second claim".into())], &[]);
        let mut legacy = base.clone();
        for r in &mut legacy.rules { r.displayed_rule = None; }
        let mut warned = legacy.clone(); warned.rules[1].warnings.push("Rule 2: source mismatch".into());
        let mut mixed = legacy.clone(); mixed.rules[0].displayed_rule = base.rules[0].displayed_rule.clone();
        let mut corrupt = legacy.clone(); corrupt.rules[0].displayed_rule_digest = "invalid".into();
        let mut duplicate = legacy.clone(); duplicate.rules[1].source_rule_ordinal = 1;
        let mut empty = legacy.clone(); empty.rules.clear();
        let mut wrong_version = legacy.clone(); wrong_version.version = "unknown".into();
        for mapping in [warned, mixed, corrupt, duplicate, empty, wrong_version, base] {
            // Current exact-text metadata deliberately disagrees with this display too.
            let doc = format!("# Old.Run\n**Rule source diagnostics v1**: `{}`\n## Business Rules\n- private legacy claim\n  extra display continuation\n- second claim\n\n_Source: Old.vb_\n", serde_json::to_string(&mapping).unwrap());
            let view = claim_presentation(&doc);
            assert!(view.starts_with("Rule claims: counts unknown; ASSOCIATION_UNKNOWN"));
            assert!(view.contains("rule prose withheld"));
            assert!(!view.contains("private legacy claim") && !view.contains("second claim"));
        }
        let doc = format!("# Old.Run\n**Rule source diagnostics v1**: `{}`\n## Source checks requiring review\n- Rule 9: source review\n## Business Rules\n- private legacy claim\n\n", serde_json::to_string(&legacy).unwrap());
        assert!(!claim_presentation(&doc).contains("private legacy claim"));
    }

    #[test]
    fn legacy_known_rule_warning_withholds_unattributable_rules_without_guessing() {
        let doc="# Old.Run\n## Source checks requiring review\n- Rule 10: disputed claim\n## Business Rules\n- disputed claim\n- another claim\n\n_Source: Old.vb_\n";
        let view=claim_presentation(doc);
        assert!(view.contains("rule prose withheld"));
        assert!(!view.contains("disputed claim") && !view.contains("another claim"));
    }
    #[test]
    fn diagnostic_that_quotes_the_full_bad_claim_cannot_reintroduce_it_as_rule_prose() {
        let bad="IF absent THEN grant everything";
        let doc=claim_doc(vec![(2,bad.into())], &[format!("Rule 2: '{bad}' is not source-backed")]);
        let view=claim_presentation(&doc);
        assert!(view.contains("SOURCE_VALIDATION_REQUIRED"));
        assert!(!view.contains(bad));
    }

}

#[cfg(test)]
mod parameter_retrieval_tests {
    use super::*;
    #[test]
    fn source_audit_promotes_optional_fact_without_rewriting_raw_prose_and_withholds_stale_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "Module Rules\n Function Read(flag As Boolean) As Integer\n  Return 1\n End Function\nEnd Module\n";
        let path=tmp.path().join("Rules.vb");std::fs::write(&path,source).unwrap();
        let method=extract_logic_methods(source,"vb").remove(0);
        let start=method.start_line;let end=start+method.body.lines().count() as u32-1;
        let mut evidence=crate::services::business_outcome_dependencies::collect(&method.body,&method.owner,"vb",start,"Rules.vb",None);
        evidence.return_paths=Some(crate::services::business_return_paths::ReturnPathEvidence {
            source_blake3:blake3::hash(source.as_bytes()).to_hex().to_string(), start_line:start,end_line:end,unavailable_reason:None,
            result:Some(serde_json::json!({"version":"vb-return-paths-v1","method_start_line":start,"method_end_line":end,
                "status":"unavailable","structural_complete":false,"paths":[],
                "parameter_occurrences":{"version":"vb-parameter-occurrences-v1","status":"available","scanned_body_tokens":2,
                    "parameters":[{"identifier":"flag","declaration_line":start,"body_identifier_occurrences":0}]}})) });
        let doc=format!("# {}.{}\n**Analysis method hash**: `{}`\n**Outcome dependencies v1**: `{}`\n## Business Rules\n- FIRST_RULE_MARKER: returns one on normal completion.\n## Data Flow\nReads flag to choose a result.\n_Source: Rules.vb_\n",method.owner,method.name,ContentHash::compute(method.body.as_bytes()).0,serde_json::to_string(&evidence).unwrap());
        let original=doc.clone();
        let fresh=SourceAudit::default().qualifications(&doc,tmp.path()).join("\n");
        assert!(fresh.contains("Parameter identifier inventory: 1 declarations, 1 zero body-token matches"),"{fresh}");
        let pointers:Vec<_>=fresh.lines().filter(|line|line.starts_with("Parameter identifier inventory:")).collect();
        assert_eq!(pointers.len(),1);assert!(pointers[0].len()<700);
        assert!(pointers[0].contains(&format!("\"flag\" (declaration line {start})")));
        assert!(pointers[0].contains("1 shown, 0 omitted"));
        assert!(pointers[0].contains("not read/unused proof"));
        assert!(!fresh.contains("Source token fact:"));
        assert!(substantive_excerpt(&doc).contains("Reads flag to choose a result."));
        assert!(substantive_excerpt(&doc).contains("FIRST_RULE_MARKER"));
        assert_eq!(doc,original);assert!(doc.contains("Reads flag to choose a result."));
        for changed in [format!("{source}' unrelated source edit\n"),format!("' moved declaration\n{source}")] {
            std::fs::write(&path,changed).unwrap();
            let mut audit=SourceAudit::default();
            assert!(audit.describe(&doc,tmp.path()).contains("VERIFIED_METHOD_HASH"));
            let summary=audit.qualifications(&doc,tmp.path()).join("\n");
            assert!(summary.contains("Parameter identifier inventory: STALE_OR_UNVERIFIED"));
            assert!(!summary.contains("\"flag\" (declaration line"));
            assert!(!summary.contains("Source token fact:"));
        }
    }
}
#[cfg(test)]
mod predicate_preview_tests {
    use super::*;
    #[test]
    fn shared_retrieval_view_shows_bound_histories_before_raw_excerpt_cap_without_certifying_claims(
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let source="Module Rules\n Function Read(flag As Boolean) As Integer\n  If flag Then Return 1\n  Return 0\n End Function\nEnd Module\n";
        std::fs::write(tmp.path().join("Rules.vb"), source).unwrap();
        let method = extract_logic_methods(source, "vb").remove(0);
        let start = method.start_line;
        let end = start + method.body.lines().count() as u32 - 1;
        let mut evidence = crate::services::business_outcome_dependencies::collect(
            &method.body,
            &method.owner,
            "vb",
            start,
            "Rules.vb",
            None,
        );
        let history = serde_json::json!({"conditions":[{"kind":"if","line":start+1,"expression":"flag","outcome":"false"}],"return_line":start+2,"return_expression":"0","normal_completion_required":true});
        evidence.return_paths = Some(crate::services::business_return_paths::ReturnPathEvidence {
            source_blake3: blake3::hash(source.as_bytes()).to_hex().to_string(),
            start_line: start,
            end_line: end,
            unavailable_reason: None,
            result: Some(
                serde_json::json!({"version":"vb-return-paths-v1","status":"available","structural_complete":true,"method_start_line":start,"method_end_line":end,"paths":[history.clone()],"normal_fallthrough_paths":0,"unsupported":[],"unsupported_omitted":0}),
            ),
        });
        let doc=format!("# {}.{}\n**Analysis method hash**: `{}`\n**Outcome dependencies v1**: `{}`\n## Business Rules\n- Returns one in every case.\n## Data Flow\n{}\n_Source: Rules.vb_\n",method.owner,method.name,ContentHash::compute(method.body.as_bytes()).0,serde_json::to_string(&evidence).unwrap(),"Raw inferred prose. ".repeat(1000));
        let original = doc.clone();
        // Use the actual report renderers: warnings are outside the 1400-character
        // Markdown business excerpt and JSON preserves their complete text.
        use crate::services::ask_engine::{
            evidence::{Authority, EvidenceItem, EvidenceKind},
            planner,
            report::{render_markdown, to_json, AskReport},
            status::{AnswerStatus, FreshnessSnapshot},
        };
        let mut warnings = SourceAudit::default().qualifications(&doc, tmp.path());
        warnings.push(format!(
            "full_document: get_chunk({})",
            serde_json::json!({"project_id":"p","namespace":"business_logic","doc_id":"doc"})
        ));
        let item = EvidenceItem {
            evidence_id: "ev_1".into(),
            document_id: Some("doc".into()),
            document_namespace: Some("business_logic".into()),
            source_verification: Some("VERIFIED_METHOD_HASH: fixture".into()),
            kind: EvidenceKind::BusinessRule,
            authority: Authority::DerivedBusinessLogic,
            path: Some("Rules.vb".into()),
            lines: Some((start, end)),
            symbol_id: None,
            title: None,
            content: substantive_excerpt(&doc),
            generation: Some(1),
            commit: None,
            timestamp: None,
            confidence: 0.5,
            relevance: 1.0,
            extraction_method: "fixture".into(),
            warnings,
            provider: "business_logic".into(),
            score: Some(1.0),
            directness: None,
        };
        let report = AskReport {
            question: "read".into(),
            plan: planner::plan_query("read"),
            status: AnswerStatus::Partial,
            mode: "retrieval_only".into(),
            evidence: vec![item],
            conflicts: vec![],
            unknowns: vec![],
            next_best: vec![],
            snapshot: FreshnessSnapshot::default(),
            providers: vec![],
            answer_members: vec![],
        };
        let markdown = render_markdown(&report);
        assert!(markdown.contains("Returns one in every case."));
        assert!(markdown.contains(&serde_json::to_string(&history).unwrap()));
        let json = to_json(&report);
        assert!(json["evidence"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Returns one in every case."));
        assert!(json["evidence"][0]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v
                .as_str()
                .unwrap()
                .contains(&serde_json::to_string(&history).unwrap())));
        assert!(markdown.contains("\"project_id\":\"p\""));
        let cards = (0..5)
            .map(|i| (format!("doc{i}"), "Rules.vb".into(), 1.0, doc.clone()))
            .collect();
        let multi = render_matches("p", "read", tmp.path(), cards, 48 * 1024 - 1024);
        let useful = multi.matches("Returns one in every case.").count();
        assert!(useful >= 3, "{multi}");
        assert!(multi.contains("matched=5"));
        if useful < 5 { assert!(multi.contains("INCOMPLETE: total response budget reached")); }

        let view = render_matches(
            "p",
            "read",
            tmp.path(),
            vec![("doc".into(), "Rules.vb".into(), 1.0, doc.clone())],
            48 * 1024,
        );
        assert!(
            view.contains("model_claim_alignment=not_assessed"),
            "{view}"
        );
        assert!(view.contains(&serde_json::to_string(&history).unwrap()));
        assert!(view.contains("full_document: get_chunk("));
        assert!(view.contains("source_body: get_full_method_body("));
        assert!(view.contains("Returns one in every case."));
        assert_eq!(doc, original);
        let too_small = render_matches(
            "p",
            "read",
            tmp.path(),
            vec![("doc".into(), "Rules.vb".into(), 1.0, doc)],
            256,
        );
        assert!(!too_small.contains("Returns one in every case."));
        assert!(!too_small.contains("Source-path test checklist"));
    }
}
