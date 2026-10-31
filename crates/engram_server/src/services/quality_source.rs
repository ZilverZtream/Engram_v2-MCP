//! Publication receipts separate current policy from retained historical evidence.
use crate::state::AppState;
use engram_core::registry::RepoRule;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::Read, path::Path};

pub(crate) const PROVENANCE: &str = "quality-gate source: ";
pub(crate) const CORPUS_REVISION: &str = "quality_corpus_revision";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SourceReceipt {
    pub origin: String,
    pub source_hash: String,
    pub authored: bool,
    pub pending: bool,
    pub doc_ids: Vec<String>,
    pub rules: Vec<RepoRule>,
    #[serde(default)]
    pub cleanup: Vec<String>,
    #[serde(default)]
    pub corpus_revision: String,
}

pub(crate) fn receipt_key(origin: &str) -> String {
    format!("quality_source:{}", blake3::hash(origin.as_bytes()).to_hex())
}

pub(crate) fn receipt(state: &AppState, pid: &str, origin: &str) -> anyhow::Result<Option<SourceReceipt>> {
    state.registry.get_meta(pid, &receipt_key(origin))?.map(|raw| {
        let parsed: SourceReceipt = serde_json::from_str(&raw)?;
        anyhow::ensure!(parsed.origin == origin, "quality source receipt identity mismatch");
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
                let r = receipt(self.state, self.pid, origin)
                    .map_err(|_| "unverified: source receipt unavailable")?
                    .ok_or("unverified: no source ownership receipt; review legacy import")?;
                if r.pending { return Err("unverified: source import interrupted or in progress; retry import"); }
                if r.authored {
                    let path = engram_core::safe_join(self.root, origin)
                        .map_err(|_| "unverified: policy source cannot be safely resolved")?;
                    let mut file = std::fs::File::open(path)
                        .map_err(|_| "unavailable: policy source is missing or unreadable")?;
                    let size = file.metadata().map_err(|_| "unverified: policy metadata unreadable")?.len();
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
    let mut verifier = SourceVerifier::new(state, pid, Path::new(&project.directory));
    let mut out = EffectiveRepoRules { rules: Vec::new(), notes: Vec::new() };
    let corpus_revision = state.registry.get_meta(pid, CORPUS_REVISION)?.unwrap_or_default();
    for rule in state.registry.list_repo_rules(pid)? {
        if paths.is_some_and(|paths| !paths.iter().any(|p|
            crate::services::pre_commit_review_service::gates::path_pattern_matches(&rule.file_pattern, p)
                || crate::utils::files::pattern_match(p, &rule.file_pattern))) {
            continue;
        }
        if let Some(origin) = rule.provenance.as_deref().and_then(|p| p.strip_prefix(PROVENANCE)) {
            let status = match verifier.source(origin) {
                Ok(r) if r.rules.iter().any(|owned| same_rule(owned, &rule)) => {
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
        } else if rule.provenance.as_deref() == Some("quality-gate finding")
            && !out.notes.iter().any(|n| n.contains("legacy quality-rule")) {
            out.notes.push("INCOMPLETE policy coverage: legacy quality-rule ownership/freshness is unverified; review migration; legacy enforcement retained".into());
        }
        out.rules.push(rule);
    }
    Ok(out)
}
