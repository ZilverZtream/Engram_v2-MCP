//! Authored guidance for the source files actually returned by a request trace.
use crate::services::project_policy;
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};

fn collect_paths(value: &Value, loaded: &BTreeSet<&str>, paths: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "page" | "codebehind" | "controller_file" | "file" | "file_path" | "source_path" | "source_file" | "target_file") {
                    if let Some(path) = value.as_str().filter(|path| loaded.contains(path)) {
                        paths.insert(path.to_owned());
                    }
                } else {
                    collect_paths(value, loaded, paths);
                }
            }
        }
        Value::Array(items) => {
            for item in items { collect_paths(item, loaded, paths); }
        }
        _ => (),
    }
}

pub(super) fn attach(root: &Path, report: &mut Value, loaded: &BTreeSet<&str>) -> anyhow::Result<()> {
    let mut paths = BTreeSet::new();
    collect_paths(report, loaded, &mut paths);
    let mut policy = project_policy::resolve(root, &paths.into_iter().collect::<Vec<_>>(), &[]);
    policy.notes.push("Policy scope covers only returned paths in the loaded selected source inventory, not every scanned file or all files a future edit may touch. Recheck guidance for the final edit scope. Policy is not proof of code compliance or runtime execution.".into());
    report["project_policy_sources"] = serde_json::to_value(policy)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trace_guidance_tracks_returned_source_scope_and_live_instruction_edits() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".github/instructions")).unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "Preserve authored exceptions.\n").unwrap();
        let write_topic = |name: &str, scope: &str, body: &str| {
            std::fs::write(temp.path().join(format!(".github/instructions/{name}.instructions.md")),
                format!("---\napplyTo: '{scope}'\n---\n{body}\n")).unwrap();
        };
        write_topic("page", "Site/*.aspx", "Preserve the page contract.");
        write_topic("code", "Site/*.vb", "Preserve event wiring, including manual exceptions.");
        write_topic("other", "Other/**", "Unrelated topic.");
        let loaded = BTreeSet::from(["Site/Orders.aspx", "Site/Orders.aspx.vb", "Other/Unreturned.vb"]);
        let original = json!({"status":"static_candidates", "routing":{"candidates":[{"page":"Site/Orders.aspx"}]},
            "pages":[{"codebehind":"Site/Orders.aspx.vb", "source_members":[{"file":"Missing/Unloaded.vb"}]}],
            "coverage":{"runtime_execution_proven":false}});
        let mut report = original.clone();
        attach(temp.path(), &mut report, &loaded).unwrap();
        let sources = report["project_policy_sources"]["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 3, "Root, markup and code guidance apply; merely scanned files do not expand scope");
        assert!(sources.iter().any(|source| source["content"].as_str().unwrap().contains("manual exceptions")));
        assert!(sources.iter().all(|source| source["path"] != ".github/instructions/other.instructions.md"));
        let before = sources.iter().find(|source| source["path"] == ".github/instructions/code.instructions.md").unwrap()["blake3"].clone();
        write_topic("code", "Site/*.vb", "Updated authored exception, without reindexing.");
        attach(temp.path(), &mut report, &loaded).unwrap();
        let updated = report["project_policy_sources"]["sources"].as_array().unwrap().iter()
            .find(|source| source["path"] == ".github/instructions/code.instructions.md").unwrap();
        assert_ne!(updated["blake3"], before);
        assert!(updated["content"].as_str().unwrap().contains("Updated authored exception"));
        report.as_object_mut().unwrap().remove("project_policy_sources");
        assert_eq!(report, original, "Guidance must not upgrade static trace evidence");
    }
}
