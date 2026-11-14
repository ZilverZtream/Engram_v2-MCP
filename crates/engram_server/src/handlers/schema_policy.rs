//! Authored guidance follows returned, admitted declaration and code evidence.
use crate::services::project_policy;
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};

fn citations(value: &Value, admitted: &BTreeSet<String>, paths: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(path) = object.get("citation").and_then(|c| c.get("path")).and_then(Value::as_str)
                && admitted.contains(path) {
                paths.insert(path.into());
            }
            for value in object.values() { citations(value, admitted, paths); }
        }
        Value::Array(rows) => for row in rows { citations(row, admitted, paths); },
        _ => (),
    }
}

pub(super) fn attach(root: &Path, report: &mut Value, admitted: &BTreeSet<String>) -> anyhow::Result<()> {
    let mut paths = BTreeSet::new();
    citations(report, admitted, &mut paths);
    for lead in report.pointer("/code_reference_leads/results").and_then(Value::as_array).into_iter().flatten() {
        if let Some(path) = lead.get("file").and_then(Value::as_str).filter(|path| admitted.contains(*path)) {
            paths.insert(path.into());
        }
    }
    let mut policy = project_policy::resolve(root, &paths.into_iter().collect::<Vec<_>>(), &[]);
    policy.notes.push("Policy scope covers returned admitted SQL declaration citations and verified code-reference leads, not every scanned SQL file or every path a future edit may touch. Recheck guidance for the final edit scope. Authored policy is not proof of implementation compliance or deployed database behavior.".into());
    report["project_policy_sources"] = serde_json::to_value(policy)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_guidance_covers_returned_declarations_and_verified_code_leads_and_refreshes_without_reindex() {
        let owned=tempfile::tempdir().unwrap();
        std::fs::create_dir_all(owned.path().join(".github/instructions")).unwrap();
        std::fs::write(owned.path().join("AGENTS.md"),"Preserve manual exceptions.\n").unwrap();
        for (name,scope,content) in [("sql","db/Orders/**","Keep existing database defaults."),("code","Site/*.vb","Preserve approval checks."),("other","db/Other/**","Unrelated SQL area.")] {
            std::fs::write(owned.path().join(format!(".github/instructions/{name}.instructions.md")),format!("---\napplyTo: '{scope}'\n---\n{content}\n")).unwrap();
        }
        let admitted=["db/Orders/Order.sql","db/Orders/SaveOrder.sql","db/Other/Unreturned.sql","Site/Order.vb"].map(str::to_owned).into_iter().collect();
        let original=json!({"results":[{"table":{"citation":{"path":"db/Orders/Order.sql"}},"procedures":[{"object":{"citation":{"path":"db/Orders/SaveOrder.sql"}}}]}],
            "code_reference_leads":{"results":[{"file":"Site/Order.vb"},{"file":"Missing/Unadmitted.vb"}]},
            "coverage":{"database_execution":false,"path":"db/Other/Unreturned.sql"}});
        let mut report=original.clone();attach(owned.path(),&mut report,&admitted).unwrap();
        let sources=report["project_policy_sources"]["sources"].as_array().unwrap();
        assert_eq!(sources.len(),3);
        assert!(sources.iter().all(|row| row["path"]!=".github/instructions/other.instructions.md"));
        let before=sources.iter().find(|row| row["path"]==".github/instructions/sql.instructions.md").unwrap()["blake3"].clone();
        std::fs::write(owned.path().join(".github/instructions/sql.instructions.md"),"---\napplyTo: 'db/Orders/**'\n---\nKeep the explicitly accepted default exception.\n").unwrap();
        report.as_object_mut().unwrap().remove("project_policy_sources");
        attach(owned.path(),&mut report,&admitted).unwrap();
        let updated=report["project_policy_sources"]["sources"].as_array().unwrap().iter().find(|row| row["path"]==".github/instructions/sql.instructions.md").unwrap();
        assert_ne!(updated["blake3"],before);
        assert!(updated["content"].as_str().unwrap().contains("accepted default exception"));
        report.as_object_mut().unwrap().remove("project_policy_sources");assert_eq!(report,original);
    }
}
