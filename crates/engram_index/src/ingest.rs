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
