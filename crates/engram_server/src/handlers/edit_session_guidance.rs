//! Compare task-opening guidance with fresh observations of the same scope.
//! Hash observations neither persist policy bodies nor certify compliance.
use crate::services::project_policy::PolicyContext;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Observation {
    version: u32,
    paths: Vec<String>,
    source_coverage: String,
    reference_coverage: String,
    references_omitted: usize,
    sources: Vec<Source>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Source {
    path: String,
    blake3: Option<String>,
    kind: String,
}

fn path_key(path: &str) -> String {
    let path = path.replace('\\', "/");
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path
    }
}

pub(super) fn observe(policy: &PolicyContext, paths: &[String]) -> Observation {
    let mut sources = BTreeMap::new();
    let mut add = |path: &str, hash: Option<&str>, kind: &str| {
        sources.entry(path_key(path)).or_insert_with(|| Source {
            path: path.into(),
            blake3: hash.map(str::to_owned),
            kind: kind.into(),
        });
    };
    for source in &policy.sources {
        add(&source.path, Some(&source.blake3), "direct_guidance");
    }
    for source in &policy.linked_sources {
        add(
            &source.path,
            Some(&source.blake3),
            "linked_document_evidence",
        );
    }
    for source in &policy.omissions {
        add(&source.path, source.blake3.as_deref(), "omitted_guidance");
    }
    for reference in &policy.references {
        if let Some(path) = &reference.path {
            add(
                path,
                reference.target_blake3.as_deref(),
                "referenced_document_candidate",
            );
        }
    }
    Observation {
        version: 1,
        paths: paths.to_vec(),
        source_coverage: policy.coverage.clone(),
        reference_coverage: policy.reference_coverage.clone(),
        references_omitted: policy.references_omitted,
        sources: sources.into_values().collect(),
    }
}

pub(super) fn summary(observation: &Observation) -> Value {
    json!({"version":observation.version,"source_count":observation.sources.len(),
        "unfingerprinted_source_count":observation.sources.iter().filter(|s| s.blake3.is_none()).count(),
        "source_coverage":observation.source_coverage,"reference_coverage":observation.reference_coverage,
        "references_omitted":observation.references_omitted,
        "interpretation":"Hash-only opening observations are retained in this edit lifecycle for comparison of the same planned paths at completion. Supplied and linked candidate sources remain distinct from accepted policy; unresolved or omitted inputs do not become verified by recording them."})
}

pub(super) fn compare(opening: &Value, current: Option<&Observation>) -> Value {
    let Some(current) = current else {
        return json!({"status":"not_requested_no_edits","comparison_performed":false,
            "instruction_compliance_verified":false,"changes":[],"change_count":0});
    };
    let Ok(before) = serde_json::from_value::<Observation>(opening.clone()) else {
        return json!({"status":"unavailable_opening_observation","comparison_performed":false,
            "instruction_compliance_verified":false,"changes":[],"change_count":0,
            "comparison_paths":current.paths,"current_observation":summary(current),
            "interpretation":"This session has no supported opening guidance observation. Current guidance is supplied separately; no claim about changes since opening can be made."});
    };
    if before.version != 1 || before.paths != current.paths {
        return json!({"status":"unavailable_comparable_opening_scope","comparison_performed":false,
            "instruction_compliance_verified":false,"changes":[],"change_count":0});
    }
    let old: BTreeMap<_, _> = before
        .sources
        .iter()
        .map(|row| (path_key(&row.path), row))
        .collect();
    let new: BTreeMap<_, _> = current
        .sources
        .iter()
        .map(|row| (path_key(&row.path), row))
        .collect();
    let keys: std::collections::BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let mut changes = Vec::new();
    for key in keys {
        let prior = old.get(key).copied();
        let after = new.get(key).copied();
        let change = match (prior, after) {
            (Some(a), Some(b)) if a.blake3 == b.blake3 => continue,
            (Some(a), Some(b)) if a.blake3.is_some() && b.blake3.is_some() => "hash_changed",
            (Some(_), Some(_)) => "fingerprint_availability_changed",
            (None, Some(_)) => "newly_observed",
            (Some(_), None) => "no_longer_observed",
            (None, None) => continue,
        };
        changes.push(json!({"path":after.or(prior).unwrap().path,"change":change,
            "before_blake3":prior.and_then(|r| r.blake3.as_deref()),
            "current_blake3":after.and_then(|r| r.blake3.as_deref()),
            "before_kind":prior.map(|r| r.kind.as_str()),"current_kind":after.map(|r| r.kind.as_str())}));
    }
    json!({"status":if changes.is_empty(){"no_changes_in_observed_source_hashes"}else{"source_changes_observed"},
        "comparison_performed":true,"comparison_paths":before.paths,
        "change_count":changes.len(),"changes":changes,
        "opening_observation":summary(&before),"current_observation":summary(current),
        "instruction_compliance_verified":false,
        "interpretation":"Revisit affected assumptions before committing when guidance observations changed. Both observations select the original planned paths; guidance for additional edited paths is supplied separately and is not mistaken for a change to the opening scope. Newly/no-longer observed means selection changed, not proven file creation/deletion or policy acceptance. Linked bodies are evidence candidates, not automatically authoritative policy. Unchanged observed hashes do not establish complete coverage, semantic equivalence, compliance, approval or a filesystem transaction. Read current complete qualifications and resolve omissions; this comparison never rewrites authored guidance."})
}
