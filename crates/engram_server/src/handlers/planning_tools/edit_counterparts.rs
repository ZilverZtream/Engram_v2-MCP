//! Structural companions have independent evidence even when Git treats the output as a hub.
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

pub(super) fn collect(
    root: &Path,
    edits: &[(String, String)],
    current: &[String],
    edited: &HashSet<String>,
) -> BTreeSet<(String, String, String)> {
    let mut result = BTreeSet::new();
    let Ok(root) = root.canonicalize() else {
        return result;
    };
    let indexed: std::collections::HashMap<_, _> = current
        .iter()
        .map(|p| (super::edit_path_key(p), p))
        .collect();
    let outputs = crate::services::typescript_outputs::build_outputs(&root, current, edits);
    for (source, path) in edits {
        let lower = path.to_ascii_lowercase();
        // The config and existing on-disk bundle establish the relationship.
        // A large generated file may intentionally be absent from the code index.
        for (_, output) in outputs.iter().filter(|(anchor, _)| anchor == source) {
            if !edited.contains(&super::edit_path_key(output)) {
                result.insert((
                    source.clone(),
                    output.clone(),
                    "compiled output of an entry point referencing the edited source".to_owned(),
                ));
            }
        }
        let mut add = |candidate: &str, reason: &str| {
            let key = super::edit_path_key(candidate);
            if !edited.contains(&key)
                && let Some(actual) = indexed.get(&key)
            {
                result.insert((source.clone(), (*actual).clone(), reason.to_owned()));
            }
        };
        let page = lower
            .strip_suffix(".vb")
            .or_else(|| lower.strip_suffix(".cs"))
            .unwrap_or(&lower);
        if [".aspx", ".ascx", ".master"]
            .iter()
            .any(|ext| page.ends_with(ext))
        {
            for ext in [".designer.vb", ".designer.cs"] {
                add(&format!("{page}{ext}"), "generated WebForms designer");
            }
        }
    }
    super::resource_keys::assess(&root, edits, current, edited, &mut result);
    result
}
