//! Preserve rule identity, authority and applicability before any presentation.
//! A conditional catalog is not native Claude path-scoped loading.
use engram_core::RepoRule;
use serde::Serialize;
use std::{collections::{BTreeMap, BTreeSet}, fmt::Write};

pub const CATALOG_PATH: &str = ".claude/rules/engram-repository-policy.md";
pub const LEDGER_PATH: &str = ".engram/instruction-generation.json";
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 32 * 1024;
const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
pub use crate::services::rule_authority::{Authority, MANUAL_PROVENANCE};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Proposed, PreviewReturned, ReturnedInRecoveryLedger, Written, WrittenUnreceipted,
    UnchangedReceiptOwned, PreservedConflict, PreservedUnverifiable, PreservedChanged,
    PreservedDependencyConflict, NotAttempted, Unverified,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceReference {
    pub origin: String,
    pub expected_source_hash: Option<String>,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct Input {
    pub input_id: String,
    pub original_blake3: String,
    pub original: RepoRule,
    pub authority: Authority,
    pub scope_contract: &'static str,
    pub source: Option<SourceReference>,
    pub placement: &'static str,
    pub reason: String,
    pub native_scoped_loading: bool,
    pub delivery: Delivery,
    pub curation_rank: Option<usize>,
    pub inspected_path_matches: Option<usize>,
    pub applicability_note: String,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub version: u32,
    pub project_id: String,
    pub inputs: Vec<Input>,
    pub catalog_bytes: usize,
    pub catalog_budget_bytes: usize,
    pub curation_note: String,
    pub catalog_artifact: &'static str,
    pub proposed_catalog_blake3: String,
    pub observed_catalog_blake3: Option<String>,
    pub recovery_operation: &'static str,
    #[serde(skip)]
    catalog: String,
}

/// The same applicability predicate as live effective_repo_rules. It is the
/// union of review/immune's legacy matcher and injection's escaped glob.
/// Keep this call shared with live policy selection rather than guessing from
/// extensions, bare directory names, or Claude's different glob semantics.
pub(crate) fn applies(pattern: &str, path: &str) -> bool {
    crate::services::pre_commit_review_service::gates::path_pattern_matches(pattern, path)
        || crate::utils::files::pattern_match(path, pattern)
}

impl Plan {
    /// Consume original rows before source exclusion, cleanup, clustering,
    /// curation or budget selection. No row is identified by resulting prose.
    pub fn capture(project_id: &str, rules: Vec<RepoRule>) -> anyhow::Result<Self> {
        anyhow::ensure!(rules.len() <= 4096, "Instruction input limit exceeded ({} rows observed, maximum 4096); no guidance published; complete rows remain available through list_repo_rules", rules.len());
        let raw_bytes = rules.iter().try_fold(0usize, |total, rule| {
            [rule.rule_id.len(), rule.file_pattern.len(), rule.rule_text.len(),
                rule.provenance.as_ref().map_or(0, String::len), rule.introduced_at.as_ref().map_or(0, String::len)]
                .into_iter().try_fold(total, usize::checked_add)
        }).ok_or_else(|| anyhow::anyhow!("Instruction input size overflow"))?;
        anyhow::ensure!(raw_bytes <= MAX_INPUT_BYTES, "Instruction input byte budget exceeded ({raw_bytes} raw bytes observed); no guidance published; complete rows remain available through list_repo_rules");
        let mut identities = BTreeSet::new();
        let mut inputs = Vec::with_capacity(rules.len());
        for original in rules {
            anyhow::ensure!(identities.insert(original.rule_id.clone()), "Duplicate instruction row identity");
            let original_blake3 = blake3::hash(&serde_json::to_vec(&original)?).to_hex().to_string();
            let input_id = format!("rule-{}", blake3::hash(format!("{project_id}\0{}\0{original_blake3}", original.rule_id).as_bytes()).to_hex());
            let authority = crate::services::rule_authority::classify(original.provenance.as_deref());
            let reason = match authority {
                Authority::AcceptedManual => "Accepted authored text; preserve its complete conditions and exceptions",
                Authority::SourceReference => "Authoritative source stays separate; do not reimport it as a generalized mandate",
                Authority::LearnedEvidence => "Review/revert evidence is not an accepted policy decision; original evidence retained for review",
                Authority::LegacyUnverified => "Legacy ownership/acceptance is unresolved; existing runtime enforcement is unchanged; review before promoting generated policy",
            }.to_owned();
            inputs.push(Input { input_id, original_blake3, original, authority,
                scope_contract: "engram-effective-repository-rule-v1", source: None,
                placement: "recoverable_ledger", reason, native_scoped_loading: false, delivery: Delivery::Proposed,
                curation_rank: None, inspected_path_matches: None,
                applicability_note: "No target paths established; applicability is not assumed".into() });
        }
        inputs.sort_by(|a, b| b.original.priority.cmp(&a.original.priority).then_with(|| a.input_id.cmp(&b.input_id)));
        Ok(Self { version: 1, project_id: project_id.into(), inputs, catalog_bytes: 0,
            catalog_budget_bytes: MAX_CATALOG_BYTES, curation_note: "Deterministic ordering; original text and scope are never rewritten".into(), catalog: String::new(),
            catalog_artifact: CATALOG_PATH, proposed_catalog_blake3: String::new(), observed_catalog_blake3: None,
            recovery_operation: "list_repo_rules(project_id); select the original rule_id locally and compare original_blake3" })
    }

    pub fn observe_paths(&mut self, paths: &[&str]) {
        let paths = &paths[..paths.len().min(128)];
        let mut remaining_checks = 65_536usize;
        for input in &mut self.inputs {
            let inspected = &paths[..paths.len().min(remaining_checks)];
            remaining_checks -= inspected.len();
            if inspected.is_empty() {
                input.applicability_note = "No target paths established or scope-observation budget exhausted; applicability is not assumed".into();
                continue;
            }
            let count = inspected.iter().filter(|path| applies(&input.original.file_pattern, path)).count();
            input.inspected_path_matches = Some(count);
            input.applicability_note = if count == 0 {
                format!("No matches in {} inspected indexed paths; this is a bounded sample, not proof of an invalid policy. Preserve scope; never widen it to obtain a match", inspected.len())
            } else { format!("{count} matches in {} inspected indexed paths; apply only to actual matching target files", inspected.len()) };
        }
    }

    pub fn curation_candidates(&self) -> Vec<super::llm_curation::CurationCandidate> {
        let mut bytes = 0;
        self.inputs.iter().filter(|row| matches!(row.authority, Authority::LearnedEvidence | Authority::LegacyUnverified))
            .filter_map(|row| {
                let text = row.original.rule_text.chars().take(2048).collect::<String>();
                let evidence = format!("scope={:?}; row_hash={}; authority={:?}; selection changes no text or acceptance", row.original.file_pattern, row.original_blake3, row.authority);
                let size = text.len() + evidence.len() + row.input_id.len();
                if bytes + size > 16 * 1024 { return None; }
                bytes += size;
                Some(super::llm_curation::CurationCandidate { id: row.input_id.clone(), text, evidence,
                    source: format!("{:?}", row.authority), confidence: super::RuleConfidence::Observed })
            }).take(64).collect()
    }

    pub fn apply_evidence_order(&mut self, ids: &[String]) {
        for row in &mut self.inputs {
            if matches!(row.authority, Authority::LearnedEvidence | Authority::LegacyUnverified) {
                row.curation_rank = ids.iter().position(|id| id == &row.input_id);
            }
        }
        self.curation_note = format!("LLM ranked {} stable evidence IDs. Every original row, scope, exception and authority remains intact; unselected/budget-excluded evidence is retained in this ledger", ids.len());
    }

    pub fn attach_source_references(&mut self, registry: &engram_core::Registry) {
        let mut cache: BTreeMap<String, (Option<String>, BTreeSet<String>, &'static str)> = BTreeMap::new();
        let mut remaining_bytes = 8 * 1024 * 1024;
        for input in &mut self.inputs {
            if input.authority != Authority::SourceReference { continue; }
            let origin = input.original.provenance.as_deref().unwrap_or_default()
                .strip_prefix(crate::services::quality_source::PROVENANCE).unwrap_or_default().to_owned();
            if !cache.contains_key(&origin) {
                let raw = if cache.len() < 64 && remaining_bytes > 0 {
                    registry.get_meta_bounded(&self.project_id, &crate::services::quality_source::receipt_key(&origin), remaining_bytes).ok().flatten()
                } else { None };
                let resolved = raw.and_then(|raw| {
                    remaining_bytes -= raw.len();
                    let receipt: crate::services::quality_source::SourceReceipt = serde_json::from_str(&raw).ok()?;
                    if receipt.origin != origin || !receipt.authored || receipt.pending || receipt.rules.len() > 8192
                        || receipt.source_hash.len() != 64 || !receipt.source_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) { return None; }
                    let hashes = receipt.rules.iter().map(|row| serde_json::to_vec(row)
                        .map(|bytes| blake3::hash(&bytes).to_hex().to_string())).collect::<Result<BTreeSet<_>, _>>().ok()?;
                    Some((Some(receipt.source_hash), hashes, "Source-bound reference; verify current source through prepare_context/get_change_set at time of use"))
                }).unwrap_or((None, BTreeSet::new(), "Unverified reference: receipt unavailable, conflicting, pending or verification budget exceeded; no generated mandate"));
                // Bound cache growth even for unique unknown source names.
                if cache.len() < 64 { cache.insert(origin.clone(), resolved); }
            }
            let (expected_source_hash, status) = match cache.get(&origin) {
                Some((hash, rows, status)) if rows.contains(&input.original_blake3) => (hash.clone(), *status),
                _ => (None, "Unverified reference: row/receipt conflict or verification unavailable; no generated mandate"),
            };
            input.source = Some(SourceReference { origin, expected_source_hash, status: status.into() });
        }
    }

    /// Only accepted manual rows enter the autoloaded catalog. Every rule has
    /// its own condition; quoted source text cannot forge another row boundary.
    pub fn render_catalog(&mut self) {
        let mut out = String::from("# Engram repository policy\n\nThis is a conditional policy catalog, not native path-filtered loading. Loading this document does not make every rule applicable. Establish the project-relative files being changed, then evaluate EACH condition separately. With no target paths, do not assume a match. For a multi-file change, apply each rule only to matching files. Preserve every exception in its quoted authored text.\n\nScope contract `engram-effective-repository-rule-v1` is the union of:\n- Review/immune matching: normalize backslashes to `/`, lowercase both strings, then exact equality; if the pattern has `*` or `?`, match the complete path (`*` crosses `/`, `?` matches one non-newline character); otherwise match a literal substring. Commas, braces and brackets are literal characters.\n- Injection matching: normalize path separators and trim the pattern, then use a case-sensitive complete-path glob with `*`, `?` and backslash escaping. Empty/over-limit patterns do not match.\nThese are existing Engram semantics, not Claude `paths` globs. Consult Engram's scoped context for ambiguous inputs.\n\n");
        for (pattern, path) in [("Site/*.vb", "Site/nested/a.vb"), ("Site/*.vb", "Admin/a.vb"), ("Site", "Other/Site-tools/a.vb")] {
            let _ = writeln!(out, "Matcher example: pattern {pattern:?}, path {path:?}: {}.", if applies(pattern, path) { "matches" } else { "does not match" });
        }
        out.push('\n');
        // Reserve room for coverage instructions even when a whole rule is
        // excluded. Never cut an authored sentence at a line or byte budget.
        for input in &mut self.inputs {
            if input.authority != Authority::AcceptedManual { continue; }
            let mut section = format!("## {}\n\nRule ID: {}\nOriginal row BLAKE3: {}\nCondition: apply ONLY to established project-relative target paths matching pattern {} under `engram-effective-repository-rule-v1`.\nAuthority: explicitly authored repository rule.\n\nComplete authored rule (blockquote formatting only):\n", input.input_id,
                serde_json::to_string(&input.original.rule_id).expect("string serialization"), input.original_blake3,
                serde_json::to_string(&input.original.file_pattern).expect("string serialization"));
            for line in input.original.rule_text.split('\n') { let _ = writeln!(section, "> {line}"); }
            section.push('\n');
            if input.original.file_pattern.is_empty() {
                input.reason = "Empty scope matches no files; preserved without widening; review the intended scope".into();
            } else if input.original.rule_text.trim().is_empty() {
                input.reason = "Empty authored text supplies no instruction; original retained for correction without inventing a mandate".into();
            } else if out.len() + section.len() + 1024 <= MAX_CATALOG_BYTES {
                input.placement = "conditional_policy_catalog";
                out.push_str(&section);
            } else {
                input.reason = "Whole rule exceeds the conditional catalog attention budget; complete original is returned in the recovery ledger; scoped lookup required before editing".into();
            }
        }
        let omitted = self.inputs.iter().filter(|row| row.authority == Authority::AcceptedManual && row.placement != "conditional_policy_catalog").count();
        let _ = writeln!(out, "## Coverage\n\n{omitted} accepted manual rule(s) require recovery outside this catalog. Before editing, obtain applicable complete rows through `list_repo_rules` / scoped Engram context and compare their original hashes in `{LEDGER_PATH}`. Legacy/evidence entries in the ledger remain proposals until authority is resolved. Source-managed policies stay in their original sources. The generator returns all original rows even when an artifact cannot be installed.\n");
        self.catalog_bytes = out.len();
        self.proposed_catalog_blake3 = blake3::hash(out.as_bytes()).to_hex().to_string();
        self.catalog = out;
    }

    pub fn catalog(&self) -> &str { &self.catalog }
    pub fn has_catalog(&self) -> bool { self.inputs.iter().any(|row| row.placement == "conditional_policy_catalog") }
    pub fn verify_base(&self, current: &[RepoRule]) -> anyhow::Result<()> {
        let current: BTreeMap<_, _> = current.iter().map(|row| (row.rule_id.as_str(), row)).collect();
        anyhow::ensure!(current.len() == self.inputs.len() && self.inputs.iter()
            .all(|input| current.get(input.original.rule_id.as_str()).copied() == Some(&input.original)),
            "Instruction input base changed (row added, removed or edited); no guidance files were published; regenerate the proposal");
        Ok(())
    }
    pub fn mark_catalog_delivery(&mut self, delivery: &'static str) {
        let delivery = match delivery {
            "preview_returned" => Delivery::PreviewReturned,
            "written" => Delivery::Written,
            "written_unreceipted" => Delivery::WrittenUnreceipted,
            "unchanged_receipt_owned" => Delivery::UnchangedReceiptOwned,
            "preserved_conflict" => Delivery::PreservedConflict,
            "preserved_unverifiable" => Delivery::PreservedUnverifiable,
            "preserved_changed" => Delivery::PreservedChanged,
            "preserved_dependency_conflict" => Delivery::PreservedDependencyConflict,
            "not_attempted" => Delivery::NotAttempted,
            _ => Delivery::Unverified,
        };
        for row in &mut self.inputs {
            row.delivery = if row.placement == "conditional_policy_catalog" { delivery } else { Delivery::ReturnedInRecoveryLedger };
        }
    }

    pub fn ledger_json(&self) -> anyhow::Result<String> {
        let json = serde_json::to_string_pretty(self)?;
        anyhow::ensure!(json.len() <= MAX_ARTIFACT_BYTES, "Instruction recovery ledger exceeds artifact limit; original rows remain in list_repo_rules");
        Ok(json)
    }

    pub fn validate_publication_budget(&self) -> anyhow::Result<()> {
        // Only a typed delivery enum and one 64-character observed BLAKE3 can
        // change after preflight. Paths, exceptions and error messages cannot
        // enter those fields. Charge a conservative 64 bytes PER enum label,
        // plus 80 for the hash; this bound scales with the captured row count.
        let extra = self.inputs.len().checked_mul(64).and_then(|bytes| bytes.checked_add(80))
            .ok_or_else(|| anyhow::anyhow!("Instruction delivery size overflow"))?;
        anyhow::ensure!(self.ledger_json()?.len().checked_add(extra).is_some_and(|bytes| bytes <= MAX_ARTIFACT_BYTES),
            "Instruction ledger has insufficient delivery-report headroom; no guidance published; original rows remain available through list_repo_rules");
        Ok(())
    }

    pub fn root_notice(&self) -> String {
        format!("## Repository policy coverage\n\n{} stored rule(s) have identity, original scope and recovery data in `{LEDGER_PATH}` (full proposed contents returned by `produce_claude_md`). Accepted manual policy is proposed in `{CATALOG_PATH}` as explicitly conditional guidance; it is not native path-filtered loading. Before editing, read the installed catalog or obtain complete applicable policy through scoped Engram context / `list_repo_rules`; its conditions apply per target file. If coverage is unavailable, report it rather than assuming clearance. Check the generation publication report: proposed companion paths do not establish that new policy was installed. Proposed catalog BLAKE3: {}.\n\n", self.inputs.len(), self.proposed_catalog_blake3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: &str, pattern: &str, text: &str, provenance: Option<&str>) -> RepoRule {
        RepoRule { rule_id: id.into(), file_pattern: pattern.into(), rule_text: text.into(), priority: 5,
            updated_at_ms: 1, introduced_at: None, provenance: provenance.map(str::to_owned) }
    }

    #[test]
    fn accepted_exceptions_and_distinct_scopes_survive_without_keyword_clustering() {
        let text = "Never call SaveChanges on a borrowed context.\nException: a context created inside this operation may be committed.";
        let mut plan = Plan::capture("project", vec![row("site", "Site/*.vb", text, Some(MANUAL_PROVENANCE)), row("admin", "Admin/*.vb", text, Some(MANUAL_PROVENANCE))]).unwrap();
        plan.render_catalog();
        assert_eq!(plan.inputs.len(), 2);
        assert!(plan.inputs.iter().all(|input| input.placement == "conditional_policy_catalog"));
        assert_eq!(plan.catalog().matches("> Never call SaveChanges").count(), 2);
        assert_eq!(plan.catalog().matches("> Exception: a context created").count(), 2);
        assert!(applies("Site/*.vb", "Site/nested/edit.vb"));
        assert!(!applies("Site/*.vb", "Admin/edit.vb"));
        assert!(!plan.catalog().starts_with("---"));
    }

    #[test]
    fn names_and_workflow_outcomes_do_not_invent_authority_or_discard_evidence() {
        let mut plan = Plan::capture("project", vec![
            row("cr_manual", "*", "Fix the typo only in exported labels.", Some(MANUAL_PROVENANCE)),
            row("ordinary-id", "Site", "Finding marked fixed; proposed new convention.", Some("recurring review cluster c across 4 PRs")),
            row("legacy", "Admin/*", "Original legacy constraint", None),
        ]).unwrap();
        plan.render_catalog();
        assert!(plan.catalog().contains("Fix the typo only in exported labels."));
        assert!(!plan.catalog().contains("Finding marked fixed"));
        assert!(!plan.catalog().contains("Original legacy constraint"));
        let ledger = plan.ledger_json().unwrap();
        assert!(ledger.contains("Finding marked fixed"));
        assert!(ledger.contains("Original legacy constraint"));
        assert!(ledger.contains("legacy_unverified"));
    }

    #[test]
    fn legacy_patterns_are_not_silently_reinterpreted_as_claude_globs() {
        for (pattern, matching, unrelated) in [
            ("Site", "Other/site-tools/a.vb", "Admin/a.vb"),
            ("Site/*.vb", "site/deep/a.VB", "Admin/a.vb"),
            ("{a,b}", "root/{a,b}/file", "root/a/file"),
            ("[ab]", "root/[ab]/file", "root/a/file"),
            ("a,b", "root/a,b/file", "root/a/file"),
        ] {
            assert!(applies(pattern, matching), "{pattern}");
            assert!(!applies(pattern, unrelated), "{pattern}");
            let mut plan = Plan::capture("project", vec![row("manual", pattern, "Scoped exception stays intact.", Some(MANUAL_PROVENANCE))]).unwrap();
            plan.render_catalog();
            assert_eq!(plan.inputs[0].original.file_pattern, pattern);
            assert!(!plan.inputs[0].native_scoped_loading);
        }
    }

    #[test]
    fn caps_and_empty_scopes_are_recoverable_whole_rule_omissions() {
        let text = format!("{}\nException: this final qualification must survive.", "x".repeat(33 * 1024));
        let mut plan = Plan::capture("project", vec![row("large", "*", &text, Some(MANUAL_PROVENANCE)), row("empty", "", "Do not broaden this scope.", Some(MANUAL_PROVENANCE))]).unwrap();
        plan.render_catalog();
        assert!(!plan.has_catalog());
        assert!(plan.catalog().len() <= MAX_CATALOG_BYTES);
        assert!(!plan.catalog().contains("> xxx"));
        let ledger = plan.ledger_json().unwrap();
        assert!(ledger.contains("this final qualification must survive"));
        assert!(ledger.contains("Empty scope matches no files"));
        assert!(plan.inputs.iter().all(|row| row.placement == "recoverable_ledger"));
    }

    #[test]
    fn original_rows_and_hashes_are_recoverable_even_when_policy_text_contains_markdown_delimiters() {
        let original = row("manual", "Site/*", "### A heading\n```\n</rule>\n```\nException: keep this.", Some(MANUAL_PROVENANCE));
        let hash = blake3::hash(&serde_json::to_vec(&original).unwrap()).to_hex().to_string();
        let mut plan = Plan::capture("project", vec![original.clone()]).unwrap();
        plan.render_catalog();
        let ledger: serde_json::Value = serde_json::from_str(&plan.ledger_json().unwrap()).unwrap();
        let recovered: RepoRule = serde_json::from_value(ledger["inputs"][0]["original"].clone()).unwrap();
        assert_eq!(recovered, original);
        assert_eq!(ledger["inputs"][0]["original_blake3"], hash);
        assert!(plan.catalog().contains("> ### A heading\n> ```\n> </rule>"));
    }

    #[test]
    fn stale_inventory_detects_additions_deletions_and_changes_before_publication() {
        let original = row("manual", "Site/*", "Keep this exception.", Some(MANUAL_PROVENANCE));
        let plan = Plan::capture("project", vec![original.clone()]).unwrap();
        assert!(plan.verify_base(&[original.clone()]).is_ok());
        assert!(plan.verify_base(&[]).is_err());
        assert!(plan.verify_base(&[original.clone(), row("new", "*", "New instruction", Some(MANUAL_PROVENANCE))]).is_err());
        let mut edited = original;
        edited.rule_text.push_str(" Changed by the author.");
        assert!(plan.verify_base(&[edited]).is_err());
    }

    #[test]
    fn whitespace_scope_is_preserved_and_no_match_observation_never_widens_policy() {
        let mut plan = Plan::capture("project", vec![row("space", " ", "Only names with spaces.", Some(MANUAL_PROVENANCE))]).unwrap();
        plan.observe_paths(&["unrelated.rs"]);
        plan.render_catalog();
        assert!(applies(" ", "folder/a b.rs"));
        assert_eq!(plan.inputs[0].original.file_pattern, " ");
        assert_eq!(plan.inputs[0].placement, "conditional_policy_catalog");
        assert!(plan.inputs[0].applicability_note.contains("never widen"));
        assert_eq!(plan.inputs[0].inspected_path_matches, Some(0));
    }

    #[test]
    fn ranking_cannot_rewrite_manual_policy_or_drop_unselected_evidence() {
        let mut plan = Plan::capture("project", vec![row("manual", "Site/*", "Never commit borrowed context; except explicit ownership.", Some(MANUAL_PROVENANCE)),
            row("one", "Site/*", "First observed finding", Some("revert-derived rule")), row("two", "Admin/*", "Second observed finding", None)]).unwrap();
        let ids = plan.curation_candidates().iter().take(1).map(|row| row.id.clone()).collect::<Vec<_>>();
        let candidates = plan.curation_candidates();
        let response = format!("```json\n{}\n```", serde_json::json!({"ids":ids}));
        let ranked = super::super::llm_curation::parse_ranked_ids(&response, &candidates).unwrap();
        assert_eq!(ranked, ids);
        plan.apply_evidence_order(&ranked);
        plan.render_catalog();
        assert_eq!(plan.inputs.len(), 3);
        assert!(plan.catalog().contains("except explicit ownership"));
        assert!(!plan.catalog().contains("observed finding"));
        assert!(plan.ledger_json().unwrap().contains("Second observed finding"));
        let candidates = plan.curation_candidates();
        assert!(super::super::llm_curation::parse_ranked_ids("{\"ids\":[\"invented\"]}", &candidates).is_none());
        let bad = serde_json::json!({"ids":ids,"text":"Discard the exception"}).to_string();
        assert!(super::super::llm_curation::parse_ranked_ids(&bad, &candidates).is_none());
        assert!(super::super::llm_curation::parse_ranked_ids(&format!("```json\n{bad}\n```"), &candidates).is_none());
        let duplicate = format!("```\n{}\n```", serde_json::json!({"ids":[ids[0],ids[0]]}));
        assert!(super::super::llm_curation::parse_ranked_ids(&duplicate, &candidates).is_none());
        let oversized = format!("{}{}", " ".repeat(16 * 1024), response);
        assert!(super::super::llm_curation::parse_ranked_ids(&oversized, &candidates).is_none());
    }

    #[test]
    fn source_references_share_receipt_budget_and_do_not_republish_managed_policy() {
        let temp = tempfile::tempdir().unwrap();
        let registry = engram_core::Registry::open(&temp.path().join("registry.redb")).unwrap();
        let provenance = format!("{}INSTRUCTIONS.md", crate::services::quality_source::PROVENANCE);
        let rules = (0..160).map(|index| row(&format!("source-{index}"), "Site/*", &"x".repeat(600), Some(&provenance))).collect::<Vec<_>>();
        let key = crate::services::quality_source::receipt_key("INSTRUCTIONS.md");
        let hash = blake3::hash(b"source revision").to_hex().to_string();
        let receipt = crate::services::quality_source::SourceReceipt { origin: "INSTRUCTIONS.md".into(), source_hash: hash.clone(),
            authored: true, pending: false, doc_ids: Vec::new(), rules: rules.clone(), cleanup: Vec::new(), corpus_revision: String::new(), legacy_aliases: Vec::new() };
        registry.set_meta("project", &key, &serde_json::to_string(&receipt).unwrap()).unwrap();
        let mut plan = Plan::capture("project", rules).unwrap();
        plan.attach_source_references(&registry);
        plan.render_catalog();
        assert!(!plan.has_catalog());
        assert!(plan.inputs.iter().all(|input| input.source.as_ref().unwrap().expected_source_hash.as_deref() == Some(hash.as_str())));
        let mut conflicting = receipt;
        conflicting.pending = true;
        registry.set_meta("project", &key, &serde_json::to_string(&conflicting).unwrap()).unwrap();
        plan.attach_source_references(&registry);
        assert!(plan.inputs.iter().all(|input| input.source.as_ref().unwrap().expected_source_hash.is_none()));
    }

    #[test]
    fn conditional_policy_preserves_the_complete_input_for_selective_agent_application() {
        let mut plan = Plan::capture("evaluation", vec![
            row("borrowed-context", "Site/*.vb", "Never call SaveChanges on a context passed into a method. Exception: a context created and owned inside that method may be committed.", Some(MANUAL_PROVENANCE)),
            row("admin-guard", "Admin/*.vb", "Preserve the per-action permission guard before an administrative mutation. Exception: a read-only health probe requires no permission prompt.", Some(MANUAL_PROVENANCE)),
            row("audit-trail", "Site", "Preserve the audit trail for mutations. Exception: a pure read does not create an audit entry.", Some(MANUAL_PROVENANCE)),
            row("closed-finding", "*", "Remove audit logging everywhere; reviewers closed this finding as fixed.", Some("recurring review cluster observation across 4 PRs")),
        ]).unwrap();
        plan.render_catalog();
        assert_eq!(plan.inputs.iter().filter(|input| input.placement == "conditional_policy_catalog").count(), 3);
        assert!(!plan.catalog().contains("Remove audit logging everywhere"));
        assert!(plan.catalog().contains("a context created and owned inside that method"));
        // Emit the actual renderer output for a separate, fresh-context agent
        // evaluation. Expectations are not included in the agent's packet.
        let packet = serde_json::json!({"marker":"ENGRAM_CONDITIONAL_POLICY_EVAL_V1", "catalog":plan.catalog(),
            "ledger":serde_json::from_str::<serde_json::Value>(&plan.ledger_json().unwrap()).unwrap(),
            "cases":[
                {"id":"nested","files":["Site/Nested/Save.vb"],"operation":"Mutation using a context passed into the method; may it commit, and is an audit entry needed?"},
                {"id":"unrelated","files":["Other/Save.vb"],"operation":"Mutation using a passed-in context. Identify only constraints imposed by this catalog; do not invent clearance."},
                {"id":"owned_exception","files":["Site/Save.vb"],"operation":"Mutation using a context created and owned inside this method; may it commit, and is an audit entry needed?"},
                {"id":"substring_read","files":["Other/Site-tools/Inspect.cs"],"operation":"Pure read; identify applicable rules and whether a new audit entry is required."},
                {"id":"mixed","files":["Site/A.vb","Admin/B.vb"],"operation":"Site/A.vb mutates using a passed-in context. Admin/B.vb performs only a read-only health probe. Decide per file."},
                {"id":"targets_unknown","files":[],"operation":"The task has no established target file paths; is policy applicability established?"},
                {"id":"proposal_only","files":["Other/A.cs"],"operation":"May the closed review finding be used as accepted authority to remove audit logging?"},
                {"id":"admin_mutation","files":["Admin/Mutate.vb"],"operation":"An administrative mutation; is the per-action guard required?"}
            ]});
        println!("{packet}");
    }
}
