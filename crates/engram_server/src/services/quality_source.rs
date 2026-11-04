//! Publication receipts separate current policy from retained historical evidence.
use crate::state::AppState;
use engram_core::registry::RepoRule;
use std::{collections::HashMap, io::Read, path::Path};

pub(crate) use crate::services::rule_authority::SOURCE_PROVENANCE as PROVENANCE;
use crate::services::rule_authority::{Authority, classify};
pub(crate) const CORPUS_REVISION: &str = "quality_corpus_revision";

pub(crate) use engram_core::registry::RuleSourceReceipt as SourceReceipt;

pub(crate) fn receipt_key(origin: &str) -> String {
    format!("quality_source:{}", blake3::hash(origin.as_bytes()).to_hex())
}

pub(crate) fn receipt(state: &AppState, pid: &str, origin: &str) -> anyhow::Result<Option<SourceReceipt>> {
    receipt_from_registry(&state.registry, pid, origin)
}

pub(crate) fn receipt_from_registry(registry: &engram_core::Registry, pid: &str, origin: &str) -> anyhow::Result<Option<SourceReceipt>> {
    registry.get_meta_bounded(pid, &receipt_key(origin), 4 * 1024 * 1024)?.map(|raw| {
        let parsed: SourceReceipt = serde_json::from_str(&raw)?;
        anyhow::ensure!(parsed.origin == origin, "quality source receipt identity mismatch");
        parsed.validate(&receipt_key(origin))?;
        Ok(parsed)
    }).transpose()
}

pub(crate) fn same_rule(a: &RepoRule, b: &RepoRule) -> bool {
    a == b
}

/// One verifier per request: no background scanning or repeated reads per gate.
pub(crate) struct SourceVerifier<'a> {
    state: &'a AppState,
    pid: &'a str,
    root: &'a Path,
    remaining: usize,
    checked: HashMap<String, Result<SourceReceipt, &'static str>>,
}

impl<'a> SourceVerifier<'a> {
    pub fn new(state: &'a AppState, pid: &'a str, root: &'a Path) -> Self {
        Self { state, pid, root, remaining: 8 * 1024 * 1024, checked: HashMap::new() }
    }

    fn source(&mut self, origin: &str) -> Result<&SourceReceipt, &'static str> {
        if !self.checked.contains_key(origin) {
            let checked = (|| {
                if self.checked.len() >= 64 { return Err("unverified: source verification budget exhausted"); }
                if self.remaining == 0 { return Err("unverified: source verification byte budget exhausted"); }
                let limit = self.remaining.min(4 * 1024 * 1024);
                let remaining = &mut self.remaining;
                let parsed = self.state.registry.map_meta_bounded(self.pid, &receipt_key(origin), limit, |bytes| {
                    *remaining = remaining.saturating_sub(bytes.len());
                    Ok(serde_json::from_slice::<SourceReceipt>(bytes))
                })
                    .map_err(|_| "unverified: source receipt unavailable")?
                    .ok_or("unverified: no source ownership receipt; review legacy import")?;
                let r = parsed
                    .map_err(|_| "unverified: source receipt unreadable")?;
                if r.origin != origin { return Err("unverified: source receipt identity mismatch"); }
                r.validate(&receipt_key(origin)).map_err(|_| "unverified: source receipt ownership constraints are invalid")?;
                if r.pending { return Err("unverified: source import interrupted or in progress; retry import"); }
                if r.authored {
                    if !self.root.is_absolute() { return Err("unverified: project has no absolute filesystem root"); }
                    let mut file = engram_core::safe_open_read(self.root, origin)
                        .map_err(|_| "unavailable: policy source cannot be safely opened")?;
                    let metadata = file.metadata().map_err(|_| "unverified: policy metadata unreadable")?;
                    if !metadata.is_file() { return Err("unverified: policy source is not a regular file"); }
                    let size = metadata.len();
                    if size > 4 * 1024 * 1024 || size >= self.remaining as u64 {
                        return Err("unverified: policy source exceeds verification budget");
                    }
                    let mut bytes = Vec::new();
                    file.by_ref().take((self.remaining.min(4 * 1024 * 1024) + 1) as u64)
                        .read_to_end(&mut bytes).map_err(|_| "unverified: policy source read failed")?;
                    self.remaining = self.remaining.saturating_sub(bytes.len());
                    if bytes.len() > 4 * 1024 * 1024 || self.remaining == 0 {
                        return Err("unverified: policy source exceeds verification budget");
                    }
                    if blake3::hash(&bytes).to_hex().as_str() != r.source_hash {
                        return Err("stale: policy source changed since import; re-ingest it");
                    }
                }
                Ok(r)
            })();
            // The cache itself is bounded, including repeated unknown origins.
            if self.checked.len() >= 64 { return Err("unverified: source verification budget exhausted"); }
            self.checked.insert(origin.to_owned(), checked);
        }
        self.checked.get(origin).expect("inserted above").as_ref().map_err(|e| *e)
    }

    pub fn document_status(&mut self, doc_id: &str, content: &str) -> &'static str {
        let mut lines = content.lines();
        let Some(origin) = lines.next().and_then(|l| l.strip_prefix("Quality-rule source: ")) else {
            return "unverified: legacy/advisory evidence without source receipt";
        };
        let hash = lines.next().and_then(|l| l.strip_prefix("Source BLAKE3: "));
        match self.source(origin) {
            Err(status) => status,
            Ok(r) if !r.doc_ids.iter().any(|id| id == doc_id) => "retired: document is outside the published source receipt",
            Ok(r) if r.authored && hash != Some(r.source_hash.as_str()) => "stale: document is from a previous policy revision",
            Ok(r) if r.authored => "current: policy hash and published source receipt match at read time",
            Ok(_) => "historical: published finding evidence; export file freshness is not required",
        }
    }
}

pub struct EffectiveRepoRules {
    pub rules: Vec<RepoRule>,
    pub notes: Vec<String>,
}

/// Stored manual/legacy rules stay inspectable through list_repo_rules. Managed
/// policy is authoritative only while both its row and source are verified.
pub fn effective_repo_rules(state: &AppState, pid: &str, paths: Option<&[&str]>) -> anyhow::Result<EffectiveRepoRules> {
    let project = state.registry.get_project(pid)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
    let rules = state.registry.list_repo_rules(pid)?;
    effective_repo_rules_from_rows(state, pid, &project, rules, paths)
}

pub(crate) fn effective_repo_rules_from_rows(state: &AppState, pid: &str,
    project: &engram_core::ProjectRecord, mut rules: Vec<RepoRule>, paths: Option<&[&str]>) -> anyhow::Result<EffectiveRepoRules> {
    // Keep budget-dependent source verification identical for normal callers
    // and exact inventories, regardless of their initial row-map ordering.
    rules.sort_by(|a, b| a.file_pattern.cmp(&b.file_pattern).then_with(|| a.rule_id.cmp(&b.rule_id)));
    let mut verifier = SourceVerifier::new(state, pid, Path::new(&project.directory));
    let mut out = EffectiveRepoRules { rules: Vec::new(), notes: Vec::new() };
    let mut unresolved_authority = 0usize;
    let corpus_revision = state.registry.get_meta_bounded(pid, CORPUS_REVISION, 4096)?.unwrap_or_default();
    for rule in rules {
        if paths.is_some_and(|paths| !paths.iter().any(|p|
            crate::services::produce_claude_md_service::instruction_plan::applies(&rule.file_pattern, p))) {
            continue;
        }
        if let Some(origin) = rule.provenance.as_deref().and_then(|p| p.strip_prefix(PROVENANCE)) {
            let status = match verifier.source(origin) {
                Ok(r) if r.rules.iter().any(|owned| same_rule(owned, &rule)) => {
                    if r.doc_ids.is_empty() {
                        let note = format!("INCOMPLETE searchable policy coverage: {origin}: ownership is recorded but no searchable evidence was published; import source to restore search coverage; verified registry constraints remain active");
                        if !out.notes.contains(&note) { out.notes.push(note); }
                    }
                    if r.corpus_revision != corpus_revision {
                        let note = format!("INCOMPLETE searchable policy coverage: {origin}: corpus was cleared after this import; re-ingest source to restore search evidence; verified registry constraints remain active");
                        if !out.notes.contains(&note) { out.notes.push(note); }
                    }
                    None
                },
                Ok(_) => Some("unverified: stored rule differs from its ownership receipt"),
                Err(status) => Some(status),
            };
            if let Some(status) = status {
                let note = format!("INCOMPLETE policy coverage: {origin}: {status}; source-managed rule withheld");
                if !out.notes.contains(&note) { out.notes.push(note); }
                continue;
            }
        } else if classify(rule.provenance.as_deref()) == Authority::LegacyUnverified {
            unresolved_authority += 1;
        }
        out.rules.push(rule);
    }
    if unresolved_authority > 0 {
        out.notes.push(format!(
            "INCOMPLETE policy coverage: {unresolved_authority} retained repository rules have unspecified or unrecognized ownership provenance; existing enforcement retained. Source ownership and policy acceptance are not inferred; inspect original rows and source evidence before adoption."
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod policy_authority_tests {
    use super::*;
    use engram_core::{Config, ProjectRecord};

    #[test]
    fn uncertain_authority_is_visible_without_changing_enforcement_or_claiming_source_ownership() {
        let temporary = tempfile::tempdir().unwrap();
        let cfg = Config { data_dir: temporary.path().join("data"),
            allowed_roots: vec![temporary.path().to_path_buf()],
            embedding_backend: "fts_only".into(), ..Default::default() };
        let (state, _events) = AppState::new(cfg).unwrap();
        state.registry.put_project(&ProjectRecord {
            project_id: "authority".into(), project_name: "Authority behavior".into(),
            project_type: "general".into(), directory: temporary.path().to_str().unwrap().into(),
            created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
        }).unwrap();
        let rule = |id: &str, scope: &str, provenance: Option<&str>| RepoRule {
            rule_id: id.into(), file_pattern: scope.into(), rule_text: format!("Keep the complete accepted conditions for {id}."),
            priority: 7, updated_at_ms: 123, introduced_at: Some("2026-01-02".into()),
            provenance: provenance.map(str::to_owned),
        };
        let manual = rule("qg-copilot:copilot-instructions.md:9", "**/*.vb", Some("manual"));
        let unknown = rule("unknown", "**/*.vb", None);
        let rules = [unknown.clone(), rule("unrecognized", "**/*.vb", Some("unrecognized author")),
            rule("old-finding", "**/*.vb", Some("quality-gate finding")),
            rule("unrelated", "**/*.css", None), manual.clone(),
            rule("review", "**/*.vb", Some("recurring review cluster 7")),
            rule("managed", "**/*.vb", Some("quality-gate source: rules/current.md"))];
        for row in &rules { state.registry.put_repo_rule("authority", row).unwrap(); }
        let before = state.registry.list_repo_rules("authority").unwrap();
        let identity_before = state.registry.source_stat_cache_state("authority").unwrap();
        let effective = effective_repo_rules(&state, "authority", Some(&["src/work.vb"])).unwrap();
        let mut retained = effective.rules.iter().map(|r| r.rule_id.as_str()).collect::<Vec<_>>();
        retained.sort();
        assert_eq!(retained, vec!["old-finding", "qg-copilot:copilot-instructions.md:9", "review", "unknown", "unrecognized"]);
        for row in &effective.rules { assert!(before.contains(row), "Every retained enforcement field remains unchanged"); }
        assert_eq!(effective.notes.iter().filter(|n| n.contains("3 retained repository rules") && n.contains("enforcement retained")).count(), 1);
        assert!(effective.notes.iter().any(|n| n.contains("source-managed rule withheld")), "Unverified source claims must still be withheld");
        let all = effective_repo_rules(&state, "authority", None).unwrap();
        assert!(all.notes.iter().any(|n| n.contains("4 retained repository rules")));
        assert!(all.notes.iter().all(|n| !n.contains("applicable rules")), "Unscoped inventory must not imply applicability to every change");
        let unrelated = effective_repo_rules(&state, "authority", Some(&["README.md"])).unwrap();
        assert!(unrelated.rules.is_empty() && unrelated.notes.is_empty());
        let plan = crate::services::produce_claude_md_service::instruction_plan::Plan::capture("authority", vec![manual, unknown]).unwrap();
        assert_eq!(plan.inputs.iter().find(|i| i.original.rule_id.starts_with("qg-")).unwrap().authority, Authority::AcceptedManual);
        assert_eq!(plan.inputs.iter().find(|i| i.original.rule_id == "unknown").unwrap().authority, Authority::LegacyUnverified);
        assert_eq!(state.registry.list_repo_rules("authority").unwrap(), before);
        assert_eq!(state.registry.source_stat_cache_state("authority").unwrap(), identity_before);
        assert_eq!(state.registry.get_meta("authority", CORPUS_REVISION).unwrap(), None);
        drop(state);
        temporary.close().unwrap();
    }
}
