//! Shared explicit-file or bounded project SQL inputs for existing reports.
use crate::utils::files::discover_files_recursive;
use engram_core::memory::{AllocationGuard, MemoryBudget, Subsystem};
use engram_core::safe_join;
use rmcp::ErrorData as McpError;
use std::{io, path::Path};
use tokio::io::AsyncReadExt;

const MAX_SQL_BYTES: u64 = 64 * 1024 * 1024;

pub(super) struct SqlSources {
    pub files: Vec<(String, String)>,
    // Retain each reservation while the corresponding source is in use.
    _reservations: Vec<AllocationGuard>,
}

impl SqlSources {
    // Borrow the complete owner so blocking parser closures retain the guards.
    pub fn as_slice(&self) -> &[(String, String)] {
        &self.files
    }
}

fn read_result<T>(
    result: io::Result<T>,
    path: &str,
    explicit: bool,
) -> Result<Option<T>, McpError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if explicit => Err(McpError::invalid_params(
            format!("Cannot read SQL file '{path}': {error}"),
            None,
        )),
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(path, %error, "failed to read SQL file");
            }
            Ok(None)
        }
    }
}

pub(super) async fn load(
    project: &str,
    selected: Option<&str>,
    memory: &MemoryBudget,
) -> Result<SqlSources, McpError> {
    let project = Path::new(project);
    let explicit = selected.is_some();
    let paths = if let Some(path) = selected {
        vec![path.to_owned()]
    } else {
        discover_files_recursive(project, &[".sql"], 2000).await
    };
    let mut sources = SqlSources {
        files: Vec::with_capacity(paths.len()),
        _reservations: Vec::new(),
    };
    let mut remaining = MAX_SQL_BYTES;
    for path in paths {
        let full = match safe_join(project, &path) {
            Ok(full) => full,
            Err(error) if explicit => {
                return Err(McpError::invalid_params(
                    format!("Path validation: {error}"),
                    None,
                ));
            }
            Err(_) => continue,
        };
        let Some(file) = read_result(tokio::fs::File::open(full).await, &path, explicit)? else {
            continue;
        };
        let Some(metadata) = read_result(file.metadata().await, &path, explicit)? else {
            continue;
        };
        let declared_bytes = metadata.len();
        if declared_bytes > remaining {
            let scope = if explicit {
                "Select a smaller source file."
            } else {
                "Select one SQL file with sql_file_path."
            };
            return Err(McpError::invalid_params(
                format!("SQL source scan exceeds 64 MiB at '{path}'. {scope}"),
                None,
            ));
        }
        // Read an extra byte to detect growth without treating a prefix as EOF.
        let read_cap = declared_bytes + 1;
        let reservation = AllocationGuard::try_new(
            memory,
            read_cap * 2,
            Subsystem::ParseBuffer,
            "SQL report source buffers",
        )
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let mut bytes = Vec::with_capacity(read_cap as usize);
        let read = file.take(read_cap).read_to_end(&mut bytes).await;
        if read_result(read, &path, explicit)?.is_none() {
            continue;
        }
        if bytes.len() as u64 > declared_bytes {
            return Err(McpError::invalid_params(
                format!("SQL file '{path}' grew while being read; retry with stable source."),
                None,
            ));
        }
        let decoded = String::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        let Some(content) = read_result(decoded, &path, explicit)? else {
            continue;
        };
        remaining -= content.len() as u64;
        sources.files.push((path, content));
        sources._reservations.push(reservation);
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_sql_source_keeps_exact_content_and_excludes_other_project_sql() {
        let owned = tempfile::tempdir().unwrap();
        let first = "CREATE PROCEDURE dbo.First AS SELECT 1;\r\n";
        std::fs::write(owned.path().join("first.sql"), first).unwrap();
        std::fs::write(owned.path().join("other.sql"), "SELECT 2;\n").unwrap();
        let project = owned.path().to_str().unwrap();
        let memory = MemoryBudget::new(1024 * 1024);
        assert_eq!(
            load(project, Some("first.sql"), &memory)
                .await
                .unwrap()
                .files,
            vec![("first.sql".to_string(), first.to_string())]
        );
        let all = load(project, None, &memory).await.unwrap();
        assert_eq!(all.files.len(), 2);
        assert!(
            all.files
                .iter()
                .any(|(path, content)| path == "first.sql" && content == first)
        );
        assert!(
            load(project, Some("../outside.sql"), &memory)
                .await
                .is_err()
        );
    }
}
