//! Shared explicit-file or bounded project SQL inputs for existing reports.
use crate::utils::files::discover_files_recursive;
use engram_core::safe_join;
use rmcp::ErrorData as McpError;
use std::path::Path;

pub(super) async fn load(project: &str, selected: Option<&str>) -> Result<Vec<(String,String)>,McpError> {
    if let Some(path) = selected {
        let full = safe_join(Path::new(project),path)
            .map_err(|error|McpError::invalid_params(format!("Path validation: {error}"),None))?;
        let content = tokio::fs::read_to_string(full).await
            .map_err(|error|McpError::invalid_params(format!("Cannot read SQL file '{path}': {error}"),None))?;
        return Ok(vec![(path.to_owned(),content)]);
    }
    let discovered = discover_files_recursive(Path::new(project),&[".sql"],2000).await;
    let mut files = Vec::with_capacity(discovered.len());
    for path in discovered {
        let Ok(full) = safe_join(Path::new(project),&path) else {continue;};
        match tokio::fs::read_to_string(&full).await {
            Ok(content) => files.push((path,content)),
            Err(error) if error.kind()==std::io::ErrorKind::NotFound => {},
            Err(error) => tracing::warn!("failed to read SQL file {:?}: {}",full,error),
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_sql_source_keeps_exact_content_and_excludes_other_project_sql() {
        let owned = tempfile::tempdir().unwrap();
        let first = "CREATE PROCEDURE dbo.First AS SELECT 1;\r\n";
        std::fs::write(owned.path().join("first.sql"),first).unwrap();
        std::fs::write(owned.path().join("other.sql"),"SELECT 2;\n").unwrap();
        let project = owned.path().to_str().unwrap();
        assert_eq!(load(project,Some("first.sql")).await.unwrap(),vec![("first.sql".to_string(),first.to_string())]);
        let all = load(project,None).await.unwrap();assert_eq!(all.len(),2);
        assert!(all.iter().any(|(path,content)|path=="first.sql" && content==first));
        assert!(load(project,Some("../outside.sql")).await.is_err());
    }
}
