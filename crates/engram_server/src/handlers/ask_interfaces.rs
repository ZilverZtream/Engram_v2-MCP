//! Direct VB interface declarations for explicit implementer questions.
use crate::services::ask_engine::{
    evidence::{Authority, EvidenceItem, EvidenceKind},
    resolver::scope_full_prefix,
    status::{AnswerStatus, ProviderReport, ProviderStatus},
};
use engram_graph::{EdgeKind, GraphStore, Node};
use engram_index::SourceSnapshotGuard;
use regex::Regex;
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::Read,
    path::Path,
    sync::{Arc, LazyLock},
    time::Duration,
};

const MAX_IMPLEMENTERS: usize = 12;
const SCOPE: &str = "Direct indexed VB Implements declarations, verified against the selected source files. Inherited implementations, registration and runtime dispatch are outside this observation.";
static IMPLEMENTERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:who implements|which classes implement|implementers of)\s+(?:interface\s+)?[`']?([a-z_][a-z_0-9]*(?:\.[a-z_][a-z_0-9]*)*)").expect("implementer question syntax")
});

pub(super) struct Evidence {
    pub items: Vec<EvidenceItem>,
    pub provider: ProviderReport,
    ambiguous: bool,
}

impl Evidence {
    pub fn qualify(&self, status: AnswerStatus) -> AnswerStatus {
        if !matches!(status, AnswerStatus::Answered | AnswerStatus::Partial) {
            return status;
        }
        if self.ambiguous {
            AnswerStatus::Ambiguous
        } else if self.provider.status != ProviderStatus::Hit || self.provider.truncated {
            AnswerStatus::Partial
        } else {
            status
        }
    }
}

fn name_matches(node: &Node, wanted: &str) -> bool {
    if wanted.contains('.') {
        node.name.eq_ignore_ascii_case(wanted)
            || format!("{}.{}", node.namespace, node.name).eq_ignore_ascii_case(wanted)
    } else {
        node.name
            .rsplit('.')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
    }
}

fn admit(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    selected: &BTreeMap<String, u64>,
    scopes: &[String],
    node: &Node,
) -> Option<(String, String)> {
    let path = node.file_path.as_str();
    if !path.to_ascii_lowercase().ends_with(".vb")
        || (!scopes.is_empty()
            && !scopes
                .iter()
                .any(|scope| scope_full_prefix(path, scope).is_some()))
    {
        return None;
    }
    let revision = *selected.get(path)?;
    let file = graph.get_node(project, &format!("file:{path}")).ok()??;
    if revision != node.generation
        || revision != file.generation
        || node.start_line == 0
        || node.end_line < node.start_line
    {
        return None;
    }
    let mut content = String::new();
    std::fs::File::open(engram_core::safe_join(root, path).ok()?)
        .ok()?
        .take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut content)
        .ok()?;
    if content.len() > 2 * 1024 * 1024 || content.lines().nth(node.end_line as usize - 1).is_none()
    {
        return None;
    }
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    (file.metadata.as_ref()?.get("file_hash")?.as_str()? == hash).then_some((content, hash))
}

fn implementation_line<'a>(
    content: &'a str,
    start: u32,
    end: u32,
    interface: &Node,
) -> Option<(u32, &'a str)> {
    let short = interface.name.rsplit('.').next()?;
    content
        .lines()
        .enumerate()
        .take(end as usize)
        .skip(start as usize - 1)
        .map(|(index, raw)| {
            (
                index as u32 + 1,
                raw.split('\'').next().unwrap_or("").trim(),
            )
        })
        .find(|(_, line)| {
            let Some((keyword, names)) = line.split_once(char::is_whitespace) else {
                return false;
            };
            keyword.eq_ignore_ascii_case("Implements")
                && names.split(',').any(|name| {
                    let name = name
                        .split('(')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_start_matches("Global.");
                    name_matches(interface, name) || name.eq_ignore_ascii_case(short)
                })
        })
}

fn collect(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    view: &SourceSnapshotGuard,
    scopes: &[String],
    wanted: &str,
    generation: u64,
    out: &mut Evidence,
) -> anyhow::Result<()> {
    let selected = view
        .selected_file_revisions()
        .ok_or_else(|| anyhow::anyhow!("Selected source unavailable"))?;
    let short = wanted.rsplit('.').next().unwrap_or(wanted);
    let declarations = graph.query_nodes(project, Some("interface"), Some(short), None, 49)?;
    out.provider.truncated = declarations.len() == 49;
    let admitted = declarations
        .iter()
        .take(48)
        .filter(|node| name_matches(node, wanted))
        .filter_map(|node| {
            admit(graph, project, root, selected, scopes, node).map(|(_, hash)| (node, hash))
        })
        .take(2)
        .collect::<Vec<_>>();
    if admitted.len() != 1 {
        out.ambiguous = admitted.len() > 1;
        let detail = if out.ambiguous {
            format!(
                "Multiple admitted declarations for {wanted}, including {} and {}. Ask with a qualified name.",
                admitted[0].0.file_path, admitted[1].0.file_path
            )
        } else {
            format!("No admitted VB declaration for {wanted} in the selected source scope.")
        };
        out.provider.note = Some(format!("{detail} {SCOPE}"));
        return Ok(());
    }
    let (interface, contract_hash) = &admitted[0];
    let candidates = graph.find_incoming_edges(
        project,
        Some(EdgeKind::Implements),
        &interface.node_id,
        MAX_IMPLEMENTERS + 1,
    )?;
    out.provider.truncated |= candidates.len() > MAX_IMPLEMENTERS;
    out.provider.available = Some(candidates.len());
    for (id, _) in candidates.into_iter().take(MAX_IMPLEMENTERS) {
        out.provider.examined += 1;
        let Some(class) = graph
            .get_node(project, &id)?
            .filter(|node| node.node_type == "class")
        else {
            continue;
        };
        let Some((source, hash)) = admit(graph, project, root, selected, scopes, &class) else {
            out.provider.truncated = true;
            continue;
        };
        let Some((line, declaration)) =
            implementation_line(&source, class.start_line, class.end_line, interface)
        else {
            out.provider.truncated = true;
            continue;
        };
        if declaration.len() > 1024 {
            out.provider.truncated = true;
            continue;
        }
        out.items.push(EvidenceItem {
            memory_record: None, evidence_id: format!("vb_implementer_{}", out.items.len()+1),
            document_id: None, document_namespace: Some("memory".into()),
            source_verification: Some(format!("SELECTED_SOURCE_FILE_HASH:{hash}")),
            kind: EvidenceKind::GraphRelation, authority: Authority::CurrentCode,
            path: Some(class.file_path.as_str().into()), lines: Some((line,line)), symbol_id: Some(id),
            title: Some(format!("{} implements {}", class.name, interface.name)),
            content: json!({"relation":"implements_interface","declaration":declaration,"class":class.name,
                "interface":{"name":interface.name,"path":interface.file_path.as_str(),"line":interface.start_line,"source_blake3":contract_hash},
                "source_blake3":hash,"scope":SCOPE}).to_string(),
            generation: Some(generation), commit: None, timestamp: None, confidence: 1.0, relevance: 1.0,
            extraction_method: "verified_vb_implements".into(), warnings: vec![SCOPE.into()],
            provider: "vb_implementers".into(), score: None, directness: Some(1.0),
        });
    }
    Ok(())
}

pub(super) async fn gather(
    graph: Arc<GraphStore>,
    project: &str,
    root: &str,
    view: Option<Arc<SourceSnapshotGuard>>,
    scopes: &[String],
    question: &str,
    generation: u64,
    deadline: Duration,
) -> Option<Evidence> {
    let wanted = IMPLEMENTERS.captures(question)?.get(1)?.as_str().to_owned();
    let result = || Evidence {
        items: Vec::new(),
        ambiguous: false,
        provider: ProviderReport {
            provider: "vb_implementers".into(),
            status: ProviderStatus::Empty,
            count: 0,
            note: Some(SCOPE.into()),
            examined: 0,
            available: None,
            truncated: false,
            proof: None,
        },
    };
    let project = project.to_owned();
    let root = root.to_owned();
    let scopes = scopes.to_vec();
    let worker = tokio::task::spawn_blocking(move || {
        let mut out = result();
        let read = view
            .ok_or_else(|| anyhow::anyhow!("Selected source unavailable"))
            .and_then(|view| {
                collect(
                    &graph,
                    &project,
                    Path::new(&root),
                    &view,
                    &scopes,
                    &wanted,
                    generation,
                    &mut out,
                )
            });
        if let Err(error) = read {
            out.provider.status = ProviderStatus::Failed;
            out.provider.note = Some(error.to_string());
        } else if !out.items.is_empty() {
            out.provider.status = ProviderStatus::Hit;
        }
        out.provider.count = out.items.len();
        out
    });
    let mut unavailable = result();
    Some(match tokio::time::timeout(deadline, worker).await {
        Ok(Ok(evidence)) => evidence,
        Ok(Err(error)) => {
            unavailable.provider.status = ProviderStatus::Failed;
            unavailable.provider.note = Some(error.to_string());
            unavailable
        }
        Err(_) => {
            unavailable.provider.status = ProviderStatus::TimedOut;
            unavailable.provider.note = Some("VB declaration lookup reached its deadline".into());
            unavailable
        }
    })
}
