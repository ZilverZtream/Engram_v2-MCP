//! Guidance for returned source, separate from source/boundary/freshness proof.
use crate::services::project_policy;
use serde_json::Value;
use std::{collections::BTreeSet,path::Path};

pub(super) fn attach(root: &Path,paths: &[String],body: String,output_json: bool,limit: Option<usize>) -> anyhow::Result<String> {
    let paths=paths.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
    let mut policy=project_policy::resolve(root,&paths,&[]);
    policy.notes.push("Policy scope covers the returned main source file and actually returned caller bodies, not other caller leads or every file a future edit may touch. Recheck guidance for the final edit scope. Authored guidance does not establish method-boundary accuracy, source freshness, implementation compliance or runtime execution.".into());
    let output=if output_json {
        let mut report:Value=serde_json::from_str(&body)?;
        anyhow::ensure!(report.is_object(),"Method/source evidence must retain its structured source and freshness fields");
        report["project_policy_sources"]=serde_json::to_value(policy)?;
        serde_json::to_string_pretty(&report)?
    } else {format!("{body}{}",policy.render_markdown())};
    if let Some(limit)=limit {
        anyhow::ensure!(output.len()<=limit,
            "Method/source evidence with complete authored guidance exceeds the existing {}-byte reply limit ({} bytes required); select a smaller complete source window. No source or policy was silently clipped.",limit,output.len());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn direct_source_policy_preserves_original_json_and_tracks_only_returned_files_with_live_exceptions() {
        let owned=tempfile::tempdir().unwrap();std::fs::create_dir_all(owned.path().join(".github/instructions")).unwrap();
        let root="Preserve manual exceptions.\n";std::fs::write(owned.path().join("AGENTS.md"),root).unwrap();
        for (name,scope,content) in [("method","Site/Method.vb","Keep manual approval."),("caller","Site/Caller.vb","Keep the caller exception."),("other","Other/**","Unreturned caller lead.")] {
            std::fs::write(owned.path().join(format!(".github/instructions/{name}.instructions.md")),format!("---\napplyTo: '{scope}'\n---\n{content}\n")).unwrap();
        }
        let original=json!({"file_path":"Site/Method.vb","source_code":"Public Sub Save()\r\nEnd Sub\r\n","source_verification":"BOUNDARIES_UNVERIFIED","source_file_hash":"original-hash","caller_bodies":[{"file_path":"Site/Caller.vb","source_code":"Save()"}],"freshness":{"status":"INDEX_BEHIND"}});
        let paths=vec!["Site/Method.vb".into(),"Site/Caller.vb".into()];
        let mut result:Value=serde_json::from_str(&attach(owned.path(),&paths,original.to_string(),true,None).unwrap()).unwrap();
        let sources=result["project_policy_sources"]["sources"].as_array().unwrap();assert_eq!(sources.len(),3);
        assert!(sources.iter().all(|row| row["path"]!=".github/instructions/other.instructions.md"));
        let before=sources.iter().find(|row| row["path"]==".github/instructions/method.instructions.md").unwrap()["blake3"].clone();
        result.as_object_mut().unwrap().remove("project_policy_sources");assert_eq!(result,original);
        let updated="---\napplyTo: 'Site/Method.vb'\n---\nPreserve the accepted override exception.\n";
        std::fs::write(owned.path().join(".github/instructions/method.instructions.md"),updated).unwrap();
        let changed:Value=serde_json::from_str(&attach(owned.path(),&paths,original.to_string(),true,None).unwrap()).unwrap();
        let observed=changed["project_policy_sources"]["sources"].as_array().unwrap().iter().find(|row| row["path"]==".github/instructions/method.instructions.md").unwrap();
        assert_ne!(observed["blake3"],before);assert_eq!(observed["content"],updated);
        let markdown=attach(owned.path(),&paths,"Original source and freshness\n".into(),false,None).unwrap();assert!(markdown.starts_with("Original source and freshness\n") && markdown.contains(updated));
    }

    #[test]
    fn complete_source_and_policy_must_fit_the_existing_excerpt_limit() {
        let owned=tempfile::tempdir().unwrap();let guide="Preserve this complete exception.\n".repeat(80);std::fs::write(owned.path().join("AGENTS.md"),&guide).unwrap();
        let paths=vec!["Site/Method.vb".into()];
        let body=json!({"file_path":"Site/Method.vb","windows":[{"source_code":"line one\nline two\n"}],"freshness":{"status":"CURRENT"}}).to_string();
        assert!(attach(owned.path(),&paths,body.clone(),true,Some(1024)).is_err());
        let complete:Value=serde_json::from_str(&attach(owned.path(),&paths,body,true,Some(32768)).unwrap()).unwrap();
        assert_eq!(complete["windows"][0]["source_code"],"line one\nline two\n");
        assert_eq!(complete["project_policy_sources"]["sources"][0]["content"],guide);
    }
}
