//! Exact legacy state mentions reuse indexed relationships and current source evidence.
use super::{
    evidence::{Authority, EvidenceItem, EvidenceKind},
    status::{AnswerStatus, ProviderReport, ProviderStatus},
};
use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::Path, sync::LazyLock};

pub(super) const INTERPRETATION: &str = "Indexed legacy state reader/writer candidates whose containing source matches the held selected revision and raw file hash. The excerpt is a literal-key window inside the indexed declaration, not a verified access-site or compiler binding. Static relationships do not prove runtime execution, stored values or shared session identity. Dynamic keys and unsupported extraction remain unknown; independent reads are not an atomic filesystem snapshot.";
const FILE_SCOPE: &str = "File-level indexed relationship only: no access-site line, method attribution or source excerpt is supplied. Recover the hash-bound file to locate and inspect its accesses; the candidate alone is incomplete location evidence.";
static MENTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)\b(Session|ViewState|Application|Cache)\s*[\(\[]\s*(?:"([^"\r\n]{1,512})"|'([^'\r\n]{1,512})')\s*[\)\]]"#,
).expect("literal legacy state mentions")
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateQuery {
    pub state_type: &'static str,
    pub key: String,
}

/// Keep one extra mention so callers can disclose the three-key selection cap.
pub fn queries(question: &str) -> Vec<StateQuery> {
    let mut out = Vec::new();
    for capture in MENTIONS.captures_iter(question) {
        let state_type = match capture[1].to_ascii_lowercase().as_str() {
            "session" => "Session",
            "viewstate" => "ViewState",
            "application" => "Application",
            _ => "Cache",
        };
        let key = capture.get(2).or_else(|| capture.get(3)).unwrap().as_str();
        let query = StateQuery {
            state_type,
            key: key.into(),
        };
        if !out.contains(&query) {
            out.push(query);
        }
        if out.len() == 4 {
            break;
        }
    }
    out
}

pub struct StateEvidence {
    pub items: Vec<EvidenceItem>,
    pub provider: ProviderReport,
    pub matched_names: Vec<String>,
    pub file_scoped_count: usize,
}
impl StateEvidence {
    pub fn empty() -> Self {
        Self {
            items: vec![],
            matched_names: vec![],
            file_scoped_count: 0,
            provider: ProviderReport {
                provider: "legacy_state".into(),
                status: ProviderStatus::Empty,
                count: 0,
                note: Some(INTERPRETATION.into()),
                examined: 0,
                available: None,
                truncated: false,
                proof: None,
            },
        }
    }
    pub fn fail(&mut self, detail: String) {
        self.provider.status = ProviderStatus::Failed;
        self.provider.note = Some(format!("{INTERPRETATION} {detail}"));
    }
    pub fn qualify_status(&self, status: AnswerStatus) -> AnswerStatus {
        if self.provider.provider == "browser_state" && matches!(status, AnswerStatus::Answered) {
            return AnswerStatus::Partial;
        }
        if matches!(status, AnswerStatus::Answered | AnswerStatus::Partial)
            && (self.provider.status != ProviderStatus::Hit
                || self.provider.truncated
                || self.file_scoped_count > 0)
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

fn current_source(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    path: &str,
    selected: &BTreeMap<String, u64>,
) -> Result<(String, String), String> {
    let read = || -> anyhow::Result<(String, String)> {
        let file = graph
            .get_node(project, &format!("file:{path}"))?
            .ok_or_else(|| anyhow::anyhow!("file fingerprint unavailable"))?;
        anyhow::ensure!(
            file.namespace == "memory" && selected.get(path) == Some(&file.generation),
            "file revision not selected"
        );
        let expected = file
            .metadata
            .as_ref()
            .and_then(|m| m.get("file_hash"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("raw file hash unavailable"))?;
        let mut bytes = Vec::new();
        engram_core::safe_open_read(root, path)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 1024 * 1024, "source exceeds1MiB/file");
        let hash = blake3::hash(&bytes).to_hex().to_string();
        anyhow::ensure!(hash == expected, "source changed since selected extraction");
        Ok((String::from_utf8(bytes)?, hash))
    };
    read().map_err(|error| format!("{error:#}"))
}

/// Selected graph facts only; no repository inventory or new state extraction.
pub fn collect(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    requested: &[StateQuery],
    selected: &BTreeMap<String, u64>,
    scopes: &[String],
    budget: &engram_core::memory::MemoryBudget,
) -> anyhow::Result<StateEvidence> {
    let _reservation = engram_core::memory::AllocationGuard::try_new(
        budget,
        16 * 1024 * 1024,
        engram_core::memory::Subsystem::Graph,
        "literal state source evidence",
    )?;
    let mut out = StateEvidence::empty();
    let mut files = BTreeMap::<String, Result<(String, String), String>>::new();
    let mut positions = BTreeMap::<String, usize>::new();
    let mut missing = 0usize;
    out.provider.truncated = requested.len() > 3;
    for query in requested.iter().take(3) {
        let state_id = engram_core::ids::NodeId::state(query.state_type, &query.key).0;
        for (kind, role) in [
            (EdgeKind::WritesState, "writer"),
            (EdgeKind::ReadsState, "reader"),
        ] {
            let incoming =
                graph.find_incoming_edges(project, Some(kind.clone()), &state_id, 129)?;
            out.provider.truncated |= incoming.len() > 128;
            let endpoints: Vec<_> = incoming
                .into_iter()
                .take(128)
                .map(|(source, _)| (source, kind.clone(), state_id.clone()))
                .collect();
            for edge in graph.get_edges_by_endpoints(project, &endpoints)? {
                out.provider.examined += 1;
                let Some(node) = graph.get_node(project, &edge.source_id)? else {
                    missing += 1;
                    continue;
                };
                let path = node.file_path.as_str();
                if !scoped(path, scopes) {
                    continue;
                }
                if node.namespace != "memory"
                    || edge.namespace != "memory"
                    || selected.get(path) != Some(&node.generation)
                    || edge.generation != node.generation
                {
                    missing += 1;
                    continue;
                }
                if !files.contains_key(path) {
                    if files.len() == 8 {
                        out.provider.truncated = true;
                        continue;
                    }
                    files.insert(
                        path.into(),
                        current_source(graph, project, root, path, selected),
                    );
                }
                let Ok((source, hash)) = &files[path] else {
                    missing += 1;
                    continue;
                };
                let lines: Vec<_> = source.lines().collect();
                let file_level = node.node_type == "file" && node.node_id == format!("file:{path}");
                if !file_level
                    && (node.start_line == 0
                        || node.end_line < node.start_line
                        || node.end_line as usize > lines.len())
                {
                    missing += 1;
                    continue;
                }
                let (start, end) = if file_level {
                    (0, lines.len())
                } else {
                    (node.start_line as usize - 1, node.end_line as usize)
                };
                let Some(offset) = lines[start..end]
                    .iter()
                    .position(|line| line.contains(&query.key))
                else {
                    missing += 1;
                    continue;
                };
                let identity = format!("{}:{}:{}", query.state_type, query.key, node.node_id);
                if let Some(position) = positions.get(&identity) {
                    let item = &mut out.items[*position];
                    let mut content: Value = serde_json::from_str(&item.content)?;
                    content["roles"].as_array_mut().unwrap().push(json!(role));
                    item.content = content.to_string();
                    continue;
                }
                if out.items.len() == 12 {
                    out.provider.truncated = true;
                    continue;
                }
                let low = if file_level {
                    0
                } else {
                    (start + offset).saturating_sub(2).max(start)
                };
                let high = if file_level {
                    0
                } else {
                    (start + offset + 4).min(end)
                };
                let window = lines[low..high].join("\n");
                let excerpt_truncated = window.chars().count() > 2048;
                let excerpt: String = window.chars().take(2048).collect();
                if !out.matched_names.contains(&query.key) {
                    out.matched_names.push(query.key.clone());
                }
                out.file_scoped_count += usize::from(file_level);
                let warnings = if file_level {
                    vec![INTERPRETATION.into(), FILE_SCOPE.into()]
                } else {
                    vec![INTERPRETATION.into()]
                };
                positions.insert(identity.clone(), out.items.len());
                out.items.push(EvidenceItem {
                    evidence_id: format!("legacy_state_{}", out.items.len() + 1),
                    document_id: None, document_namespace: None,
                    source_verification: Some(format!("CURRENT_FILE_HASH_MATCH:{hash}")),
                    kind: EvidenceKind::GraphRelation, authority: Authority::CurrentCode,
                    path: Some(path.into()), lines: (!file_level).then_some((low as u32 + 1, high as u32)),
                    symbol_id: Some(format!("legacy_state:{identity}")),
                    title: Some(format!("{}(\"{}\") indexed state candidate: {}", query.state_type, query.key, node.name)),
                    content: json!({"evidence_kind":"legacy_state_access_candidate","state_type":query.state_type,
                        "state_key":query.key,"roles":[role],"indexed_symbol_id":node.node_id,"indexed_declaration":node.name,
                        "source_scope":if file_level {"file"} else {"indexed_declaration"},
                        "declaration_lines":if file_level {Value::Null} else {json!([node.start_line,node.end_line])},"raw_blake3":hash,
                        "source_excerpt":if file_level {Value::Null} else {json!(excerpt)},"excerpt_truncated":excerpt_truncated,
                        "source_recovery":{"tool":"get_full_method_body","arguments":{"project_id":project,
                            "file_path":path,"line_start":low+1,"context_lines":0,"output_json":true,"expected_file_blake3":hash}},
                        "interpretation":INTERPRETATION}).to_string(),
                    generation: Some(node.generation), commit: None, timestamp: None,
                    confidence: 0.85, relevance: 1.0, extraction_method: "selected_source_indexed_state".into(),
                    warnings, provider: "legacy_state".into(), score: None, directness: Some(1.0),
                });
            }
        }
    }
    out.provider.count = out.items.len();
    if !out.items.is_empty() {
        out.provider.status = ProviderStatus::Hit;
    } else if missing > 0 {
        out.provider.status = ProviderStatus::Stale;
    }
    out.provider.truncated |= missing > 0;
    out.provider.note = Some(format!(
        "{INTERPRETATION} Unavailable or mismatched candidates: {missing}. File-level candidates without localized access evidence: {}. Coverage is bounded to3literal keys,128indexed candidates/direction/key,8source files of at most1MiB each and12retained declaration/key facts; missing observations are not absence proof.",
        out.file_scoped_count
    ));
    Ok(out)
}
