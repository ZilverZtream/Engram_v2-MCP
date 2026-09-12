//! Exact displayed-rule identity links source diagnostics to proposed cases.
//! Digest equality is attribution only, never semantic correctness.
use serde::{Deserialize, Serialize};

pub const VERSION: &str = "displayed-rule-source-diagnostics-v1";
pub const DOCUMENT_PREFIX: &str = "**Rule source diagnostics v1**: `";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSourceDiagnostics {
    pub version: String,
    pub rules: Vec<RuleDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleDiagnostic {
    /// Original provider array position, not a matrix or displayed bullet index.
    pub source_rule_ordinal: usize,
    pub displayed_rule_digest: String,
    /// Exact display bytes, when supplied by the current extractor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displayed_rule: Option<String>,
    pub warnings: Vec<String>,
}

pub struct RuleAssociation {
    pub blocks_outcome: bool,
    pub summary: String,
}

pub fn displayed_rule_digest(rule: &str) -> String {
    format!("blake3-raw-utf8:{}", blake3::hash(rule.as_bytes()).to_hex())
}

/// Called with actual parsed entries and the complete generated warning list,
/// before any display caps. Blank entries cannot shift another rule's identity.
pub fn build(
    entries: impl IntoIterator<Item = (usize, String)>,
    warnings: &[String],
) -> RuleSourceDiagnostics {
    RuleSourceDiagnostics {
        version: VERSION.into(),
        rules: entries
            .into_iter()
            .filter(|(_, rule)| !rule.is_empty())
            .map(|(ordinal, rule)| {
                let prefix = format!("Rule {ordinal}:");
                RuleDiagnostic {
                    source_rule_ordinal: ordinal,
                    displayed_rule_digest: displayed_rule_digest(&rule),
                    displayed_rule: Some(rule),
                    warnings: warnings
                        .iter()
                        .filter(|warning| warning.starts_with(&prefix))
                        .cloned()
                        .collect(),
                }
            })
            .collect(),
    }
}

pub fn from_document(document: &str) -> Option<RuleSourceDiagnostics> {
    let mut records = document
        .lines()
        .filter_map(|line| line.strip_prefix(DOCUMENT_PREFIX));
    let value = records.next()?.strip_suffix('`')?;
    if records.next().is_some() {
        return None;
    }
    let parsed: RuleSourceDiagnostics = serde_json::from_str(value).ok()?;
    valid_mapping(&parsed).then_some(parsed)
}

fn valid_mapping(mapping: &RuleSourceDiagnostics) -> bool {
    let mut ordinals = std::collections::HashSet::new();
    mapping.version == VERSION && mapping.rules.iter().all(|entry| {
        entry.source_rule_ordinal > 0
            && ordinals.insert(entry.source_rule_ordinal)
            && entry.displayed_rule.as_ref().is_none_or(|rule| !rule.is_empty() && displayed_rule_digest(rule) == entry.displayed_rule_digest)
            && entry.warnings.iter().all(|warning| warning.starts_with(&format!("Rule {}:", entry.source_rule_ordinal)))
    })
}

/// Recover exact rule boundaries only when the complete stored display block
/// agrees byte-for-byte. Legacy metadata without text cannot prove boundaries.
pub fn exact_document_rules(mapping: Option<&RuleSourceDiagnostics>, document: &str) -> Option<Vec<String>> {
    let mapping = mapping.filter(|mapping| valid_mapping(mapping))?;
    if mapping.rules.len() > 400 { return None; }
    let total = mapping.rules.iter().try_fold(0usize, |total, entry| total.checked_add(entry.displayed_rule.as_ref()?.len()).and_then(|n| n.checked_add(3)))?;
    if total > 128 * 1024 - 32 { return None; }
    let rules: Vec<String> = mapping.rules.iter().map(|entry| entry.displayed_rule.clone()).collect::<Option<_>>()?;
    if rules.is_empty() { return None; }
    let mut block = String::from("## Business Rules\n");
    for rule in &rules {
        if block.len().saturating_add(rule.len()).saturating_add(4) > 128 * 1024 { return None; }
        block.push_str(&format!("- {rule}\n"));
    }
    block.push('\n');
    let mut occurrences = document.match_indices(&block).filter(|(offset, _)| *offset == 0 || document.as_bytes()[*offset - 1] == b'\n');
    occurrences.next()?;
    occurrences.next().is_none().then_some(rules)
}

pub fn associate(mapping: Option<&RuleSourceDiagnostics>, rule: &str) -> RuleAssociation {
    let unknown = || {
        RuleAssociation {
        blocks_outcome: false,
        summary: "Source diagnostic association: association_unknown (legacy, invalid or unmatched exact displayed-rule identity); document-local ordinals are not guessed. Existing warnings still require review; this is not a source-validation pass. Full diagnostics: get_chunk for this document.".into(),
    }
    };
    let Some(mapping) = mapping.filter(|m| valid_mapping(m)) else {
        return unknown();
    };
    let digest = displayed_rule_digest(rule);
    let matching: Vec<_> = mapping
        .rules
        .iter()
        .filter(|entry| entry.displayed_rule_digest == digest && entry.source_rule_ordinal > 0)
        .collect();
    if matching.is_empty() {
        return unknown();
    }
    let mut count = 0usize;
    let mut displayed = Vec::new();
    let mut shortened = 0usize;
    for entry in &matching {
        for warning in &entry.warnings {
            // An invalid attribution cannot be treated as a clean mapping.
            if !warning.starts_with(&format!("Rule {}:", entry.source_rule_ordinal)) {
                return unknown();
            }
            count += 1;
            if displayed.len() < 4 {
                let mut end = warning.len().min(384);
                while !warning.is_char_boundary(end) {
                    end -= 1;
                }
                shortened += usize::from(end < warning.len());
                displayed.push(format!(
                    "original rule {}: {}{}",
                    entry.source_rule_ordinal,
                    &warning[..end],
                    if end < warning.len() {
                        " [shortened]"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    let status = if count > 0 {
        "blocked_pending_source_validation"
    } else {
        "mapped_no_rule_specific_diagnostics_not_verified"
    };
    RuleAssociation {
        blocks_outcome: count > 0,
        summary: format!(
            "Source diagnostic association: {status}; exact displayed-rule digest {digest}; {count} mapped diagnostics, shown {}, omitted {}, shortened {shortened}. {} Original ordinals are attribution only, not matrix case numbers. Full diagnostics: get_chunk for this document. A matching digest or no diagnostics does not verify semantics.",
            displayed.len(),
            count.saturating_sub(displayed.len()),
            displayed.join("; ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_rule_text_preserves_multiline_whitespace_and_rejects_malformed_entries() {
        let rule = "IF ready THEN first\r\n- continuation\n## Data Flow\nlast  ";
        let mapping = build([(2, rule.into())], &["Rule 2: absent ref".into()]);
        let document = format!("## Business Rules\n- {rule}\n\n## Data Flow\nother\n");
        let recovered = exact_document_rules(Some(&mapping), &document).unwrap();
        assert_eq!(recovered, vec![rule]);
        assert!(associate(Some(&mapping), &recovered[0]).blocks_outcome);
        assert!(exact_document_rules(Some(&mapping), &document.replace("last  ", "last")).is_none());
        assert!(exact_document_rules(Some(&mapping), &format!("{document}\n{document}")).is_none());
        let huge_rule = "x".repeat(128 * 1024);
        let huge = build([(1, huge_rule.clone())], &[]);
        assert!(exact_document_rules(Some(&huge), &format!("## Business Rules\n- {huge_rule}\n\n")).is_none());
        let mut invalid = mapping.clone();
        invalid.rules.push(invalid.rules[0].clone());
        assert!(exact_document_rules(Some(&invalid), &document).is_none());
        assert!(associate(Some(&invalid), rule).summary.contains("association_unknown"));
        invalid = mapping.clone();
        invalid.rules[0].displayed_rule = Some("changed".into());
        assert!(exact_document_rules(Some(&invalid), &document).is_none());
        let mut legacy = mapping.clone();
        legacy.rules[0].displayed_rule = None;
        assert!(exact_document_rules(Some(&legacy), &document).is_none());
        // An intact single-line legacy digest still supports exact association.
        assert!(associate(Some(&legacy), rule).blocks_outcome);
    }
    #[test]
    fn complete_diagnostics_keep_exact_identity_despite_skips_and_caps() {
        let bad = "IF condition THEN row.owner.wrong_id [line 4]";
        let clean = "IF condition THEN row.owner.project_id [line 4]";
        let mut warnings: Vec<_> = (0..25)
            .map(|i| format!("Data flow: unrelated {i}"))
            .collect();
        warnings.extend(
            (0..7).map(|i| format!("Rule 3: reference {i} absent {}", "detail ".repeat(100))),
        );
        let map = build(
            [(1, "".into()), (2, clean.into()), (3, bad.into())],
            &warnings,
        );
        assert_eq!(map.rules.len(), 2);
        let association = associate(Some(&map), bad);
        assert!(association.blocks_outcome);
        assert!(
            association
                .summary
                .contains("7 mapped diagnostics, shown 4, omitted 3, shortened 4")
        );
        assert!(association.summary.len() < 2400);
        assert!(!associate(Some(&map), clean).blocks_outcome);
        assert!(
            associate(Some(&map), &format!("{bad} "))
                .summary
                .contains("association_unknown")
        );
        assert!(associate(None, bad).summary.contains("association_unknown"));
        assert_ne!(
            displayed_rule_digest("a\r\nb"),
            displayed_rule_digest("a\nb")
        );
    }
    #[test]
    fn unknown_versions_duplicate_headers_and_wrong_ordinals_are_not_guessed() {
        let mut mapping = build([(1, "rule".into())], &["Rule 1: ref absent".into()]);
        mapping.rules[0].source_rule_ordinal = 2;
        assert!(
            associate(Some(&mapping), "rule")
                .summary
                .contains("association_unknown")
        );
        mapping.version = "future".into();
        assert!(
            associate(Some(&mapping), "rule")
                .summary
                .contains("association_unknown")
        );
        let mapping = build([(1, "rule".into())], &[]);
        let line = format!(
            "{DOCUMENT_PREFIX}{}`\n",
            serde_json::to_string(&mapping).unwrap()
        );
        assert!(from_document(&line).is_some());
        assert!(from_document(&format!("{line}{line}")).is_none());
    }
}
