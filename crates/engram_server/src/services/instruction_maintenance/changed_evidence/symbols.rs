//! Direct name dependencies select maintenance review, never policy authority.
use super::super::manifest::symbols::{self, DeclarationIndex};
use super::super::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) struct Discovery {
    index: DeclarationIndex,
    evidence: Vec<Evidence>,
    pub omissions: Vec<Value>,
}

impl Discovery {
    pub fn new(
        root: &Path,
        repo: &git2::Repository,
        tree: &git2::Tree<'_>,
        changed: &[String],
    ) -> Self {
        let mut remaining = PACKET_LIMIT;
        let mut evidence = Vec::new();
        let mut omissions = Vec::new();
        for path in changed {
            let extension = Path::new(path)
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(
                extension.as_str(),
                "vb" | "js" | "jsx" | "ts" | "tsx" | "sql" | "config"
            ) {
                continue;
            }
            let mut next_remaining = remaining;
            let captured = (|| -> Result<_> {
                let before = committed(repo, tree, path, &mut next_remaining, FILE_LIMIT)?;
                let after = disk(root, path, &mut next_remaining, FILE_LIMIT)?;
                Ok((before, after))
            })();
            match captured {
                Ok((before, after)) => {
                    remaining = next_remaining;
                    evidence.push(Evidence { path: path.clone(), before, after });
                }
                Err(error) => omissions.push(json!({"path":path,"reason":"changed_symbol_source_unavailable","detail":format!("{error:#}")})),
            }
        }
        let index = symbols::declarations(&[], &evidence, &BTreeMap::new());
        for row in &index.inventory {
            if row.omitted_candidates > 0
                || matches!(
                    row.status,
                    "unsupported_symbol_extractor"
                        | "symbol_extractor_unavailable"
                        | "symbol_source_budget_exceeded"
                )
            {
                omissions.push(json!({"path":row.path,"version":row.version,"reason":"changed_symbol_inventory_incomplete","inventory":row}));
            }
        }
        Self {
            index,
            evidence,
            omissions,
        }
    }

    pub fn matches(&self, instruction: &Instruction, omissions: &mut Vec<Value>) -> Vec<Value> {
        let mut issues = Vec::new();
        let references = symbols::references(
            &self.index,
            std::slice::from_ref(instruction),
            &self.evidence,
            &BTreeMap::new(),
            &mut issues,
        );
        for issue in issues {
            omissions.push(json!({"path":instruction.path,"reason":"changed_symbol_reference_coverage","issue":issue}));
        }
        // Automatic inverse selection requires an explicit authored code
        // mention. Ordinary prose can coincide with degraded VB candidates
        // named "for" or "is"; that is not a useful maintenance dependency.
        // The manifest's existing prose observations remain available.
        let indices = references
            .iter()
            .enumerate()
            .filter(|(_, reference)| reference.form.starts_with("inline_code"))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let mut matches = Vec::new();
        for change in symbols::changes(&references, &indices) {
            if matches!(
                change.status,
                "same_observed_candidate_bytes"
                    | "body_coverage_unavailable_for_vb_candidates"
                    | "unresolved_in_both_collected_versions"
            ) {
                continue;
            }
            // Multiple name candidates stay ambiguous, but unchanged body sets
            // do not create work solely because an unrelated method changed.
            if change.status == "ambiguous_candidate_comparison"
                && !change.before_candidates.is_empty()
                && change.before_candidates.len() == change.after_candidates.len()
                && change.before_candidates.iter().all(|old| {
                    old.span_kind != "regex_declaration_line_only"
                        && change.after_candidates.iter().any(|new| {
                            old.path == new.path
                                && old.qualified_name == new.qualified_name
                                && old.evidence_blake3 == new.evidence_blake3
                        })
                })
            {
                continue;
            }
            if matches.len() >= 16 {
                omissions.push(json!({"path":instruction.path,"reason":"changed_symbol_match_limit","limit":16}));
                break;
            }
            let proofs = references
                .iter()
                .filter(|reference| {
                    reference.literal == change.literal && reference.form.starts_with("inline_code")
                })
                .collect::<Vec<_>>();
            matches.push(json!({"kind":"symbol_reference","symbol_change":change,"references":proofs,
                "interpretation":"Observed name/declaration evidence for maintenance review only. Ambiguity, qualification and unavailable body coverage remain as reported. This does not establish a compiler binding, semantic effect, accepted policy change or authority outside the instruction's declared scope."}));
        }
        matches
    }
}
