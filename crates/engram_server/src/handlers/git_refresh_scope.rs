//! Automatic source refresh must not mistake an enclosing repository for
//! versioned project history. Explicit history requests retain their contract.
use std::path::Path;

pub(crate) fn skip_reason(project: &Path) -> anyhow::Result<Option<&'static str>> {
    // Discovery/provider failures keep the existing history error handling.
    let repo = match git2::Repository::discover(project) {
        Ok(repo) => repo,
        Err(_) => return Ok(None),
    };
    let Some(workdir) = repo.workdir() else { return Ok(None) };
    let project = project.canonicalize()?;
    let workdir = workdir.canonicalize()?;
    if project == workdir {
        return Ok(None);
    }
    let relative = project.strip_prefix(&workdir)?;
    let prefix = format!("{}/", relative.to_string_lossy().replace('\\', "/"));
    // The index is current repository metadata, not a source-file traversal or
    // a commit walk. This also admits deliberately tracked ignored files.
    let index = repo.index()?;
    if index.iter().any(|entry| entry.path.starts_with(prefix.as_bytes())) {
        return Ok(None);
    }
    Ok(Some("no versioned project subtree in discovered parent repository"))
}
