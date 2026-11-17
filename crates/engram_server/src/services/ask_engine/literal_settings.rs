//! Source-bound literal getter observations, without inference about values or stores.
use super::{
    evidence::{Authority, EvidenceItem, EvidenceKind},
    status::{AnswerStatus, ProviderReport, ProviderStatus},
};
use engram_graph::GraphStore;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

const INTERPRETATION: &str = "Complete-literal, same-line VB getter source observations. Getter names do not establish return types, physical backing-store identity, runtime values, event reachability or execution. Equal keys in different stores remain independent. Computed/multiline keys and unsupported getter syntax are unexamined; independent source reads are not an atomic filesystem snapshot.";

pub struct SettingsEvidence {
    pub items: Vec<EvidenceItem>,
    pub provider: ProviderReport,
    pub matched_names: Vec<String>,
}
impl SettingsEvidence {
    pub fn empty() -> Self {
        Self {
            items: vec![],
            matched_names: vec![],
            provider: ProviderReport {
                provider: "literal_settings".into(),
                status: ProviderStatus::Empty,
                count: 0,
                note: Some(INTERPRETATION.into()),
                examined: 0,
                available: Some(0),
                truncated: false,
                proof: None,
            },
        }
    }
    pub fn fail(&mut self, message: String) {
        self.provider.status = ProviderStatus::Failed;
        self.provider.note = Some(format!(
            "{} {message}",
            self.provider.note.as_deref().unwrap_or(INTERPRETATION)
        ));
    }
    pub fn qualify_status(&self, status: AnswerStatus) -> AnswerStatus {
        if matches!(status, AnswerStatus::Answered | AnswerStatus::Partial)
            && (self.provider.provider == "config_settings"
                || self.provider.status != ProviderStatus::Hit || self.provider.truncated)
        {
            AnswerStatus::Partial
        } else {
            status
        }
    }
}
fn scoped(path: &str, scopes: &[String]) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    scopes.is_empty()
        || scopes.iter().any(|scope| {
            let scope = scope
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_lowercase();
            path == scope
                || path.starts_with(&format!("{scope}/"))
                || path.contains(&format!("/{scope}/"))
        })
}

/// Look up exact named keys in the index, then re-extract their current admitted
/// source. This does not scan the repository or create settings catalog entries.
pub fn collect(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    keys: &[String],
    selected: &BTreeMap<String, u64>,
    scopes: &[String],
    budget: &engram_core::memory::MemoryBudget,
) -> anyhow::Result<SettingsEvidence> {
    let _reservation = engram_core::memory::AllocationGuard::try_new(
        budget,
        32 * 1024 * 1024,
        engram_core::memory::Subsystem::Graph,
        "named-setting source evidence",
    )?;
    let mut out = SettingsEvidence::empty();
    let mut files = BTreeMap::<String, BTreeSet<String>>::new();
    out.provider.truncated = keys.len() > 3;
    let mut matched = BTreeSet::new();
    for key in keys.iter().take(3) {
        let nodes = graph.query_nodes(project, Some("setting_key"), Some(key), None, 5001)?;
        out.provider.truncated |= nodes.len() > 5000;
        for node in nodes.iter().take(5000).filter(|node| {
            node.name == *key
                || node
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("literal_setting_key"))
                    .and_then(Value::as_str)
                    == Some(key.as_str())
        }) {
            let path = node.file_path.as_str();
            if !path.to_ascii_lowercase().ends_with(".vb") || !scoped(path, scopes) {
                continue;
            }
            *out.provider.available.as_mut().unwrap() += 1;
            if selected.get(path) != Some(&node.generation) {
                out.fail(format!(
                    "{path}: literal-key graph revision not selected; observation withheld."
                ));
                continue;
            }
            if !files.contains_key(path) && files.len() == 16 {
                out.provider.truncated = true;
                continue;
            }
            files.entry(path.into()).or_default().insert(key.clone());
        }
    }
    let mut rows_retained = 0;
    let mut retained_bytes = 0;
    for (path, queries) in files {
        out.provider.examined += 1;
        let result = (|| -> anyhow::Result<(String, String)> {
            let file = graph
                .get_node(project, &format!("file:{path}"))?
                .ok_or_else(|| anyhow::anyhow!("file fingerprint unavailable"))?;
            anyhow::ensure!(
                selected.get(&path) == Some(&file.generation),
                "file revision not selected"
            );
            let hash = file
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("file_hash"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("raw file fingerprint unavailable"))?;
            let mut bytes = Vec::new();
            std::fs::File::open(engram_core::safe_join(root, &path)?)?
                .take(2 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 2 * 1024 * 1024, "source exceeds2MiB/file");
            anyhow::ensure!(
                blake3::hash(&bytes).to_hex().as_str() == hash,
                "source changed since selected extraction"
            );
            Ok((String::from_utf8(bytes)?, hash.into()))
        })();
        let (source, hash) = match result {
            Ok(result) => result,
            Err(error) => {
                out.fail(format!(
                    "{path}: {error:#}; current setting observations withheld."
                ));
                continue;
            }
        };
        let mut observations = Vec::new();
        let mut seen = BTreeSet::new();
        for key in queries {
            let (rows, omitted) = engram_index::vb_extractor::literal_settings::source_observations(
                &source,
                24usize.saturating_sub(rows_retained),
                Some(&key),
            );
            out.provider.truncated |= omitted;
            for row in rows {
                if !seen.insert((row.line, row.name.clone(), row.getter.clone())) {
                    continue;
                }
                let line = source
                    .lines()
                    .nth(row.line.saturating_sub(1) as usize)
                    .unwrap_or_default();
                if line.len() > 4096 || retained_bytes + line.len() + 2048 > 256 * 1024 {
                    out.provider.truncated = true;
                    continue;
                }
                retained_bytes += line.len() + 2048;
                matched.insert(key.clone());
                rows_retained += 1;
                observations.push(json!({"key":row.key,"store":row.store,"getter":row.getter,"line":row.line,"source_line":line}));
            }
        }
        if observations.is_empty() {
            continue;
        }
        let first = observations
            .iter()
            .filter_map(|row| row["line"].as_u64())
            .min()
            .unwrap() as u32;
        let last = observations
            .iter()
            .filter_map(|row| row["line"].as_u64())
            .max()
            .unwrap() as u32;
        out.items.push(EvidenceItem {
            evidence_id: format!("literal_settings_{}", out.items.len() + 1), document_id: None, document_namespace: None,
            source_verification: Some(format!("SELECTED_SOURCE_FILE_HASH:{hash}")),
            kind: EvidenceKind::Setting, authority: Authority::CurrentCode, path: Some(path.clone()), lines: Some((first,last)),
            symbol_id: Some(format!("literal_settings:{path}:{hash}")), title: Some(format!("Literal setting observations: {}", observations.iter().filter_map(|row|row["key"].as_str()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", "))),
            content: json!({"evidence_kind":"literal_setting_getter_observations","raw_blake3":hash,"observations":observations,"interpretation":INTERPRETATION}).to_string(),
            generation: selected.get(&path).copied(), commit: None, timestamp: None, confidence: 0.9, relevance: 1.0,
            extraction_method: "selected_source_literal_getter".into(), warnings: vec![INTERPRETATION.into()],
            provider: "literal_settings".into(), score: None, directness: Some(1.0),
        });
    }
    out.matched_names = matched.into_iter().collect();
    out.provider.count = out.items.len();
    if out.provider.status != ProviderStatus::Failed && !out.items.is_empty() {
        out.provider.status = ProviderStatus::Hit;
    }
    if out.provider.truncated {
        out.provider.note.as_mut().unwrap().push_str(" Coverage capped at3named entities,5000indexed observations/key,16files,24source observations,4096bytes/complete source line and256KiB retained observations; omitted facts are not absence proof.");
    }
    Ok(out)
}
