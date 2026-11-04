use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB limit for source files

/// Check if a file appears to be binary by looking for null bytes in the first 8 KB.
///
/// SAFETY: This performs synchronous I/O. It is called from within `spawn_blocking`
/// (via Rayon `par_iter` in `index_files`) so it does not block the Tokio async
/// executor. Do NOT call this from an async context without wrapping in spawn_blocking.
pub fn is_binary(path: &Path) -> bool {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        // Fail-closed: unreadable files should not be treated as safe text files.
        Err(_) => return true,
    };
    let mut buffer = [0; 8192];
    use std::io::Read;
    let n = match file.read(&mut buffer) {
        Ok(n) => n,
        Err(_) => return true,
    };
    // Check for null bytes
    buffer[..n].contains(&0)
}

pub fn iter_files(root: &Path, exts: &[&str]) -> Vec<PathBuf> {
    use std::collections::BTreeSet;

    let mut out = Vec::new();
    let allowed_exts: BTreeSet<String> = exts
        .iter()
        .map(|x| x.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    let walker = builder.build();

    for r in walker {
        let Ok(entry) = r else { continue };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        if entry.path_is_symlink() {
            // Avoid indexing through symlinked files to prevent duplicate/cross-root ingestion.
            continue;
        }
        let p = entry.into_path();
        if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            let lower = ext.to_ascii_lowercase();
            if allowed_exts.contains(&lower) {
                out.push(p);
            }
        }
    }
    out
}

/// Enumerate an authoritative source set. Unlike the best-effort advisory
/// walker, an incomplete walk must never become a deletion list. Limits are
/// checked while collecting paths, before the overflowing candidate is kept.
pub fn iter_files_checked(
    root: &Path,
    exts: &[&str],
    max_files: Option<u64>,
    max_bytes: Option<u64>,
) -> anyhow::Result<Vec<PathBuf>> {
    iter_files_checked_excluding(root, exts, max_files, max_bytes, &[])
}

/// Prune owned, project-relative directories before enumerating their contents.
/// Excluded entries never reach authoritative ignore validation or source
/// budgets. The ignore library may still probe the excluded root's own ignore
/// metadata while maintaining its traversal stack.
pub fn iter_files_checked_excluding(
    root: &Path,
    exts: &[&str],
    max_files: Option<u64>,
    max_bytes: Option<u64>,
    excluded_directories: &[PathBuf],
) -> anyhow::Result<Vec<PathBuf>> {
    use anyhow::Context;
    use std::collections::BTreeSet;

    let mut excluded = Vec::with_capacity(excluded_directories.len());
    for relative in excluded_directories {
        anyhow::ensure!(
            !relative.as_os_str().is_empty()
                && relative.components().all(|component| matches!(component, std::path::Component::Normal(_))),
            "Excluded source directories must be nonempty project-relative paths"
        );
        excluded.push(root.join(relative));
    }
    validate_source_root(root)?;
    // The ignore crate intentionally suppresses ignore-file I/O failures.
    // Explicit admission is needed in addition to checking walker errors.
    let canonical = root.canonicalize()?;
    let mut git_boundary = None;
    for parent in canonical.ancestors() {
        if has_git_boundary(parent)? {
            git_boundary = Some(parent.to_path_buf());
            break;
        }
    }
    for parent in canonical.ancestors().skip(1) {
        let git_rules_apply = git_boundary
            .as_ref()
            .is_some_and(|boundary| parent.starts_with(boundary));
        validate_directory_ignore_rules(parent, git_rules_apply)?;
    }
    let mut global_checked = false;
    let mut git_scope_by_depth = Vec::new();
    let allowed_exts: BTreeSet<String> = exts
        .iter()
        .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    let mut files = Vec::new();
    let mut bytes = 0_u64;
    builder.filter_entry(move |entry| !excluded.iter().any(|directory| entry.path().starts_with(directory)));
    for result in builder.build() {
        let entry =
            result.with_context(|| format!("Incomplete source scan of {}", root.display()))?;
        if let Some(error) = entry.error() {
            anyhow::bail!("Incomplete source scan of {}: {error}", root.display());
        }
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            let depth = entry.depth();
            let inherited_git = if depth == 0 {
                git_boundary.is_some()
            } else {
                *git_scope_by_depth
                    .get(depth - 1)
                    .context("Missing parent source scan scope")?
            };
            let git_rules_apply = inherited_git || has_git_boundary(entry.path())?;
            git_scope_by_depth.truncate(depth);
            git_scope_by_depth.push(git_rules_apply);
            validate_directory_ignore_rules(entry.path(), git_rules_apply)?;
            if git_rules_apply && !global_checked {
                validate_global_ignore_rules()?;
                global_checked = true;
            }
        }
        if entry.path_is_symlink() || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| allowed_exts.contains(&ext.to_ascii_lowercase()))
        {
            continue;
        }
        let count = u64::try_from(files.len())?
            .checked_add(1)
            .context("Source file count overflow")?;
        if let Some(limit) = max_files {
            anyhow::ensure!(count <= limit, "Too many files: {count} > limit {limit}");
        }
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("Cannot inspect source file {}", path.display()))?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Source candidate changed type during scan: {}",
            path.display()
        );
        bytes = bytes
            .checked_add(metadata.len())
            .context("Source byte total overflow")?;
        if let Some(limit) = max_bytes {
            anyhow::ensure!(
                bytes <= limit,
                "Project byte budget exceeded: candidate files total {bytes} bytes > limit {limit} bytes"
            );
        }
        files.push(entry.into_path());
    }
    validate_source_root(root)?;
    Ok(files)
}

pub fn validate_source_root(root: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    anyhow::ensure!(
        std::fs::metadata(root)
            .with_context(|| format!("Cannot inspect source root {}", root.display()))?
            .is_dir(),
        "Source root is not a directory: {}",
        root.display()
    );
    Ok(())
}

fn validate_optional_ignore_file(path: &Path) -> anyhow::Result<()> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(path.parent().unwrap_or(path));
    if let Some(error) = builder.add(path) {
        if !error
            .io_error()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        {
            anyhow::bail!(
                "Cannot read complete source ignore rules {}: {error}",
                path.display()
            );
        }
    }
    Ok(())
}

fn has_git_boundary(directory: &Path) -> anyhow::Result<bool> {
    for name in [".git", ".jj"] {
        match std::fs::metadata(directory.join(name)) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn validate_directory_ignore_rules(directory: &Path, git_rules_apply: bool) -> anyhow::Result<()> {
    validate_optional_ignore_file(&directory.join(".ignore"))?;
    if !git_rules_apply {
        return Ok(());
    }
    validate_optional_ignore_file(&directory.join(".gitignore"))?;
    let dotgit = directory.join(".git");
    let metadata = match std::fs::metadata(&dotgit) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let gitdir = if metadata.is_dir() {
        dotgit
    } else {
        let line = read_git_pointer(&dotgit)?;
        let path = line
            .strip_prefix("gitdir: ")
            .ok_or_else(|| anyhow::anyhow!("Invalid Git directory pointer {}", dotgit.display()))?;
        directory.join(path)
    };
    validate_optional_ignore_file(&gitdir.join("info/exclude"))?;
    let common = gitdir.join("commondir");
    match std::fs::metadata(&common) {
        Ok(_) => {
            let path = read_git_pointer(&common)?;
            validate_optional_ignore_file(&gitdir.join(path).join("info/exclude"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn read_git_pointer(path: &Path) -> anyhow::Result<String> {
    use std::io::{BufRead, Read};
    let mut line = String::new();
    std::io::BufReader::new(std::fs::File::open(path)?.take(8193)).read_line(&mut line)?;
    anyhow::ensure!(
        !line.trim().is_empty() && line.len() <= 8192,
        "Invalid or oversized Git pointer {}",
        path.display()
    );
    Ok(line.trim_end().to_owned())
}

fn validate_global_ignore_rules() -> anyhow::Result<()> {
    use std::io::Read;
    // Match the ignore crate's config discovery, checking read errors before
    // its resolver can quietly fall back to another global ignore file.
    #[allow(deprecated)]
    let home = std::env::home_dir();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".config")));
    let configs = [
        home.map(|path| path.join(".gitconfig")),
        xdg.map(|path| path.join("git/config")),
    ];
    for config in configs.into_iter().flatten() {
        match std::fs::File::open(&config) {
            Ok(mut file) => {
                std::io::copy(&mut file.by_ref(), &mut std::io::sink())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => anyhow::bail!(
                "Cannot read source ignore config {}: {error}",
                config.display()
            ),
        }
    }
    if let Some(path) = ignore::gitignore::gitconfig_excludes_path() {
        validate_optional_ignore_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod authoritative_scan_tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        // Isolate ignore discovery from the checkout that owns the fixture.
        std::fs::create_dir(root.path().join(".git")).unwrap();
        root
    }

    #[test]
    fn missing_or_regular_file_root_is_not_an_empty_source_set() {
        let root = root();
        let file = root.path().join("source.rs");
        std::fs::write(&file, "fn source() {}\n").unwrap();
        let missing = iter_files_checked(&root.path().join("missing"), &["rs"], None, None);
        let regular = iter_files_checked(&file, &["rs"], None, None);
        root.close().unwrap();
        assert!(missing.is_err());
        assert!(regular.is_err());
    }

    #[test]
    fn complete_empty_and_filtered_source_sets_are_valid() {
        let root = root();
        std::fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(root.path().join("ignored.rs"), "ignored").unwrap();
        std::fs::write(root.path().join("note.txt"), "unsupported").unwrap();
        let empty = iter_files_checked(root.path(), &["rs"], Some(0), Some(0));
        std::fs::write(root.path().join("actual.RS"), "source").unwrap();
        let selected = iter_files_checked(root.path(), &[".rs"], Some(1), Some(6));
        let expected = root.path().join("actual.RS");
        root.close().unwrap();
        assert!(empty.unwrap().is_empty());
        assert_eq!(selected.unwrap(), vec![expected]);
    }

    #[test]
    fn configured_limits_reject_the_corpus_instead_of_returning_a_prefix() {
        let root = root();
        std::fs::write(root.path().join("one.rs"), "12345").unwrap();
        std::fs::write(root.path().join("two.rs"), "67890").unwrap();
        let files = iter_files_checked(root.path(), &["rs"], Some(1), None);
        let bytes = iter_files_checked(root.path(), &["rs"], None, Some(9));
        let exact = iter_files_checked(root.path(), &["rs"], Some(2), Some(10));
        root.close().unwrap();
        assert!(files.is_err());
        assert!(bytes.is_err());
        assert_eq!(exact.unwrap().len(), 2);
    }

    #[test]
    fn irrelevant_parent_git_rules_do_not_block_an_inner_repository() {
        let outer = root();
        let inner = outer.path().join("inner");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        std::fs::write(inner.join("source.rs"), "source").unwrap();
        // Git rules above the inner repository boundary are not consulted.
        std::fs::write(outer.path().join(".gitignore"), [0xff]).unwrap();
        let permitted = iter_files_checked(&inner, &["rs"], None, None);
        // Parent .ignore rules still apply across that boundary.
        std::fs::write(outer.path().join(".ignore"), [0xff]).unwrap();
        let rejected = iter_files_checked(&inner, &["rs"], None, None);
        outer.close().unwrap();
        assert_eq!(permitted.unwrap().len(), 1);
        assert!(rejected.is_err());
    }

    #[cfg(windows)]
    #[test]
    fn unreadable_ignore_rules_do_not_authorize_a_partial_source_set() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = root();
        let rules = root.path().join(".ignore");
        std::fs::write(&rules, "ignored.rs\n").unwrap();
        std::fs::write(root.path().join("ignored.rs"), "ignored").unwrap();
        std::fs::write(root.path().join("actual.rs"), "source").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&rules)
            .unwrap();
        let partial = iter_files_checked(root.path(), &["rs"], None, None);
        drop(lock);
        let recovered = iter_files_checked(root.path(), &["rs"], None, None);
        root.close().unwrap();
        assert!(
            partial.is_err(),
            "inaccessible ignore rules make selection incomplete"
        );
        assert_eq!(recovered.unwrap().len(), 1);
    }
}
