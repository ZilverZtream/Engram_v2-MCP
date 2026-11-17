//! Browser-key questions reuse existing selected-source state relationships.
use super::{
    evidence::{Authority, EvidenceItem, EvidenceKind},
    literal_state::{self, StateEvidence, StateQuery},
    status::ProviderStatus,
};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const INTERPRETATION: &str = "Indexed browser-storage reader/writer candidates whose source matches the held selected revision and raw file hash. Key-specific excerpts are literal-key windows in indexed declarations, not verified access-site or compiler bindings. Clear operations are separately source-admitted store-wide candidates. Project membership does not prove a shared browser origin, tab/session, script inclusion, built-in binding, execution order or stored value. Dynamic keys and unsupported extraction remain unknown; source reads and graph observations are not an atomic snapshot.";
static MENTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)\b(localStorage|sessionStorage)(?:\s*\.\s*(?:getItem|setItem|removeItem)\s*\(\s*|\s+(?:key\s+)?)(?:"([^"\r\n]{1,512})"|'([^'\r\n]{1,512})'|`([^`\r\n]{1,512})`)"#
).expect("explicit browser-storage keys")
});

/// The fourth distinct mention is retained only to disclose the three-key cap.
pub fn queries(question: &str) -> Vec<StateQuery> {
    let mut out = Vec::new();
    for capture in MENTIONS.captures_iter(question) {
        let query = StateQuery {
            state_type: if capture[1].eq_ignore_ascii_case("localStorage") {
                "LocalStorage"
            } else {
                "SessionStorage"
            },
            key: capture
                .get(2)
                .or_else(|| capture.get(3))
                .or_else(|| capture.get(4))
                .unwrap()
                .as_str()
                .into(),
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

/// Preserve the existing source admission and bounds while distinguishing the
/// browser provider and its uncertainty from the unchanged legacy contract.
pub fn configure(evidence: &mut StateEvidence) -> anyhow::Result<()> {
    evidence.provider.provider = "browser_state".into();
    evidence.provider.note = evidence
        .provider
        .note
        .as_ref()
        .map(|note| note.replacen(literal_state::INTERPRETATION, INTERPRETATION, 1));
    for (index, item) in evidence.items.iter_mut().enumerate() {
        let mut content: Value = serde_json::from_str(&item.content)?;
        content["evidence_kind"] = json!("browser_storage_access_candidate");
        content["interpretation"] = json!(INTERPRETATION);
        content["runtime_execution_proven"] = json!(false);
        content["shared_browser_store_proven"] = json!(false);
        item.title = Some(format!(
            "{} key {:?}: indexed candidate {}",
            content["state_type"].as_str().unwrap_or_default(),
            content["state_key"].as_str().unwrap_or_default(),
            content["indexed_declaration"].as_str().unwrap_or_default()
        ));
        item.content = content.to_string();
        item.evidence_id = format!("browser_state_{}", index + 1);
        item.symbol_id = item
            .symbol_id
            .as_ref()
            .map(|id| id.replacen("legacy_state:", "browser_state:", 1));
        item.provider = "browser_state".into();
        for warning in &mut item.warnings {
            *warning = warning.replacen(literal_state::INTERPRETATION, INTERPRETATION, 1);
        }
    }
    Ok(())
}

pub fn clear_unavailable(evidence: &mut StateEvidence, query: &StateQuery, detail: &str) {
    evidence.provider.truncated = true;
    let previous = evidence
        .provider
        .note
        .take()
        .unwrap_or_else(|| INTERPRETATION.into());
    evidence.provider.note = Some(format!(
        "{previous} {} store-wide clear evidence unavailable for {:?}: {detail}",
        query.state_type, query.key
    ));
}

pub fn append_clear(
    evidence: &mut StateEvidence,
    project: &str,
    query: &StateQuery,
    clear: Value,
) -> anyhow::Result<()> {
    let candidates = clear["candidates"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Clear reader returned no candidates array"))?;
    let mut omitted = 0;
    let retained_before = evidence.items.len();
    for row in candidates {
        if evidence.items.len() == 16 {
            omitted += 1;
            continue;
        }
        let path = row["file"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Clear candidate lacks source path"))?;
        let line = row["line"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Clear candidate lacks source line"))?;
        let hash = row["source_blake3"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Clear candidate lacks source hash"))?;
        evidence.items.push(EvidenceItem {
            evidence_id: format!("browser_state_{}", evidence.items.len()+1),
            document_id: None, document_namespace: None,
            source_verification: Some(format!("CURRENT_FILE_HASH_MATCH:{hash}")),
            kind: EvidenceKind::GraphRelation, authority: Authority::CurrentCode,
            path: Some(path.into()), lines: Some((u32::try_from(line)?, u32::try_from(line)?)),
            symbol_id: Some(format!("browser_clear:{}:{}:{}", query.state_type, query.key, row["node_id"].as_str().unwrap_or_default())),
            title: Some(format!("{} clear(): possible store-wide effect on {:?}", query.state_type, query.key)),
            content: json!({"evidence_kind":"browser_storage_clear_candidate","state_type":query.state_type,
                "state_key":query.key,"roles":["store_wide_clear"],"candidate":row,"interpretation":INTERPRETATION,
                "runtime_execution_proven":false,"shared_browser_store_proven":false,
                "source_recovery":{"tool":"get_full_method_body","arguments":{"project_id":project,
                    "file_path":path,"line_start":line,"context_lines":0,"output_json":true,"expected_file_blake3":hash}}}).to_string(),
            generation: row["physical_revision"].as_u64(), commit: None, timestamp: None,
            confidence: 0.85, relevance: 1.0, extraction_method: "selected_source_browser_clear".into(),
            warnings: vec![INTERPRETATION.into()], provider: "browser_state".into(), score: None, directness: Some(1.0),
        });
    }
    if evidence.items.len() > retained_before && !evidence.matched_names.contains(&query.key) {
        evidence.matched_names.push(query.key.clone());
    }
    evidence.provider.examined += candidates.len();
    evidence.provider.count = evidence.items.len();
    evidence.provider.truncated |= clear["truncated"] == true || omitted > 0;
    if !evidence.items.is_empty() {
        evidence.provider.status = ProviderStatus::Hit;
    }
    let previous = evidence
        .provider
        .note
        .take()
        .unwrap_or_else(|| INTERPRETATION.into());
    evidence.provider.note = Some(format!(
        "{previous} {} clear candidates for {:?}: {}; omitted by combined16-fact limit:{omitted}. Clear coverage/gaps: {}. A clear candidate is not a literal-key access or shared-origin proof.",
        query.state_type,
        query.key,
        candidates.len(),
        clear["gaps"]
    ));
    Ok(())
}
