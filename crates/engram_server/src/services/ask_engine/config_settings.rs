//! Existing appSettings declarations, admitted by selected raw source hashes.
use super::{
    evidence::{Authority, EvidenceItem, EvidenceKind},
    literal_settings::SettingsEvidence,
    literal_state,
    status::ProviderStatus,
};
use engram_graph::GraphStore;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub const INTERPRETATION: &str = "File-level appSettings declaration candidates re-extracted with the existing configuration parser from selected, hash-matching source. Attribute values are authored source text, not effective/default runtime values or proven bindings to similarly named VB getters. Configuration inheritance, location/transform precedence, clear/remove, external config files and execution are not evaluated. Parser coverage and independent file reads are not an exhaustive or atomic configuration snapshot. Connection-string values are not collected.";

pub fn empty() -> SettingsEvidence {
    let mut out = SettingsEvidence::empty();
    out.provider.provider = "config_settings".into();
    out.provider.note = Some(INTERPRETATION.into());
    out
}

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
        16 * 1024 * 1024,
        engram_core::memory::Subsystem::Graph,
        "named configuration declaration evidence",
    )?;
    let mut out = empty();
    out.provider.truncated = keys.len() > 3;
    let mut files = BTreeMap::<String, BTreeSet<String>>::new();
    for key in keys.iter().take(3) {
        let nodes = graph.query_nodes(project, Some("app_setting"), Some(key), None, 5001)?;
        out.provider.truncated |= nodes.len() > 5000;
        for node in nodes
            .into_iter()
            .take(5000)
            .filter(|node| node.name == *key)
        {
            let path = node.file_path.as_str();
            // Scope rejection precedes any source-file access.
            if !literal_state::scoped(path, scopes) {
                continue;
            }
            *out.provider.available.as_mut().unwrap() += 1;
            if node.namespace != "memory" || selected.get(path) != Some(&node.generation) {
                out.fail(format!(
                    "{path}: configuration declaration revision not selected; withheld."
                ));
                continue;
            }
            if !files.contains_key(path) && files.len() == 8 {
                out.provider.truncated = true;
                continue;
            }
            files.entry(path.into()).or_default().insert(key.clone());
        }
    }
    let mut retained_rows = 0usize;
    let mut retained_bytes = 0usize;
    let mut matched = BTreeSet::new();
    for (path, keys) in files {
        out.provider.examined += 1;
        let (source, hash) =
            match literal_state::current_source(graph, project, root, &path, selected) {
                Ok(value) => value,
                Err(error) => {
                    out.fail(format!(
                        "{path}: {error}; configuration declaration withheld."
                    ));
                    continue;
                }
            };
        let (symbols, _) = engram_index::config_extractor::extract_web_config(
            &engram_core::RelPath::new(&path),
            &source,
        );
        let mut declarations = Vec::new();
        for symbol in symbols
            .into_iter()
            .filter(|symbol| symbol.kind == "app_setting" && keys.contains(&symbol.name))
        {
            let Some(value) = symbol.metadata.as_ref().and_then(|m| m.get("value")) else {
                continue;
            };
            let row = json!({"key":symbol.name,"authored_value_attribute":value});
            let bytes = row.to_string().len();
            if retained_rows == 24
                || retained_bytes.saturating_add(bytes).saturating_add(2048) > 256 * 1024
            {
                out.provider.truncated = true;
                continue;
            }
            retained_rows += 1;
            retained_bytes += bytes + 2048;
            matched.insert(symbol.name);
            declarations.push(row);
        }
        if declarations.is_empty() {
            // Discovery cannot establish a current declaration by itself.
            if !out.provider.truncated {
                out.fail(format!(
                    "{path}: no supported matching declaration found in admitted source."
                ));
            }
            continue;
        }
        out.items.push(EvidenceItem {
            memory_record: None,
            evidence_id: format!("config_settings_{}", out.items.len() + 1),
            document_id: None,
            document_namespace: None,
            source_verification: Some(format!("CURRENT_FILE_HASH_MATCH:{hash}")),
            kind: EvidenceKind::Setting,
            authority: Authority::CurrentCode,
            path: Some(path.clone()),
            lines: None,
            symbol_id: Some(format!("config_settings:{path}:{hash}")),
            title: Some(format!("Authored appSettings declarations in {path}")),
            content: json!({"evidence_kind":"app_settings_declaration_candidates",
                "source_scope":"file_level_parsed_declarations","raw_blake3":hash,
                "declarations":declarations,"effective_value_proven":false,
                "getter_store_binding_proven":false,"interpretation":INTERPRETATION,
                "source_recovery":{"tool":"get_full_method_body","arguments":{
                    "project_id":project,"file_path":path,"line_start":1,
                    "context_lines":0,"output_json":true,"expected_file_blake3":hash}}})
            .to_string(),
            generation: selected.get(&path).copied(),
            commit: None,
            timestamp: None,
            confidence: 0.9,
            relevance: 1.0,
            extraction_method: "selected_source_config_declaration".into(),
            warnings: vec![INTERPRETATION.into()],
            provider: "config_settings".into(),
            score: None,
            directness: Some(1.0),
        });
    }
    out.matched_names = matched.into_iter().collect();
    out.provider.count = out.items.len();
    if out.provider.status != ProviderStatus::Failed && !out.items.is_empty() {
        out.provider.status = ProviderStatus::Hit;
    }
    out.provider.note.as_mut().unwrap().push_str(" Discovery is bounded to3named entities,5000indexed candidates/key,8files of at most1MiB,24declarations and256KiB retained observations. Whole omitted rows/files are not absence proof. File excerpts use current-source pagination, not exact declaration positions.");
    Ok(out)
}
