use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB limit for source files

pub fn is_binary(path: &Path) -> bool {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false, // If we can't open, skip or assume not binary? Safe to assume skip.
    };
    let mut buffer = [0; 8192];
    use std::io::Read;
    let n = match file.read(&mut buffer) {
        Ok(n) => n,
        Err(_) => return false,
    };
    // Check for null bytes
    buffer[..n].contains(&0)
}

pub fn iter_files(root: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
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
        let p = entry.into_path();
        if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            let lower = ext.to_lowercase();
            if exts.iter().any(|x| x.trim_start_matches('.') == lower) {
                out.push(p);
            }
        }
    }
    out
}
