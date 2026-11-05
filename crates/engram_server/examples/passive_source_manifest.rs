//! Metadata-only evidence for a controlled source refresh. Uses production
//! eligibility; hashes bytes without printing or retaining source contents.
use anyhow::{Context, Result, ensure};
use engram_core::{Config, RelPath};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs::File, io::{Read, Write}, path::{Path, PathBuf}};

#[cfg(windows)]
fn file_identity(file: &File) -> Result<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    ensure!(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "File identity unavailable: {}", std::io::Error::last_os_error());
    Ok(format!("{:08x}:{:08x}{:08x}", info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok(format!("{:x}:{:x}", meta.dev(), meta.ino()))
}

fn regular_source(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(meta.is_file() && !meta.file_type().is_symlink(), "Candidate is not a regular file");
    #[cfg(windows)] {
        use std::os::windows::fs::MetadataExt;
        ensure!(meta.file_attributes() & 0x400 == 0, "Candidate is a reparse file");
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(args.len() == 8, "Use --root PATH --config JSON --project-type TYPE --output JSON");
    let mut flags = BTreeMap::new();
    for pair in args.chunks_exact(2) {
        ensure!(["--root", "--config", "--project-type", "--output"].contains(&pair[0].as_str()), "Unknown flag");
        ensure!(flags.insert(pair[0].as_str(), pair[1].as_str()).is_none(), "Duplicate flag");
    }
    let root = PathBuf::from(*flags.get("--root").context("Missing root")?);
    let output = PathBuf::from(*flags.get("--output").context("Missing output")?);
    let config = Config::load_from_path(Path::new(flags["--config"]))?;
    let canonical_root = root.canonicalize()?;
    let storage = config.data_dir.canonicalize()?;
    ensure!(!canonical_root.starts_with(&storage), "Source cannot be runtime storage");
    ensure!(!output.parent().context("Output parent")?.canonicalize()?.starts_with(&canonical_root),
        "Evidence must be outside the source tree");
    let excluded = storage.strip_prefix(&canonical_root).ok()
        .map(|relative| vec![relative.to_path_buf()]).unwrap_or_default();
    let extensions = engram_server::utils::files::exts_for_project_type(flags["--project-type"]);
    let mut paths = engram_index::ingest::iter_files_checked_excluding(
        &root, &extensions, config.max_project_files, config.max_project_bytes, &excluded,
    )?;
    ensure!(paths.len() <= 100_000, "Passive manifest exceeds its evidence count bound");
    paths.sort();
    let mut rows = BTreeMap::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    for path in paths {
        regular_source(&path)?;
        ensure!(path.canonicalize()?.starts_with(&canonical_root), "Candidate resolves outside source root");
        let relative = RelPath::from_relative(&root, &path).context("Non-relative source candidate")?;
        let mut source = File::open(&path)?;
        let before = source.metadata()?;
        let identity = file_identity(&source)?;
        let mut blake = blake3::Hasher::new();
        let mut sha = Sha256::new();
        let mut bytes = 0u64;
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 { break; }
            bytes += count as u64;
            total += count as u64;
            ensure!(total <= 16 * 1024 * 1024 * 1024, "Passive hash evidence exceeds16GiB bound");
            blake.update(&buffer[..count]);
            sha.update(&buffer[..count]);
        }
        let after = source.metadata()?;
        regular_source(&path)?;
        let current = File::open(&path)?;
        let current_meta = current.metadata()?;
        ensure!(bytes == before.len() && bytes == after.len() && before.modified()? == after.modified()?,
            "Candidate changed while hashing: {}", relative.as_str());
        ensure!(file_identity(&current)? == identity && current_meta.len() == bytes
            && current_meta.modified()? == before.modified()?
            && path.canonicalize()?.starts_with(&canonical_root),
            "Candidate pathname changed while hashing: {}", relative.as_str());
        let modified_ns = before.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos().to_string();
        ensure!(rows.insert(relative.as_str().to_owned(), json!({"bytes": bytes, "modified_ns": modified_ns,
            "file_identity": identity,
            "blake3": blake.finalize().to_hex().to_string(), "sha256": format!("{:x}", sha.finalize()),
            "above_ingest10MiB_limit": bytes > 10 * 1024 * 1024})).is_none(), "Normalized source path collision");
    }
    let report = json!({"status":"PASS_PASSIVE_PRODUCTION_DISCOVERY_HASH_MANIFEST",
        "root":root,"project_type":flags["--project-type"],"extensions":extensions,
        "max_project_files":config.max_project_files,"max_project_bytes":config.max_project_bytes,
        "excluded_storage_directories":excluded,"candidate_files":rows.len(),"candidate_bytes":total,
        "scope":"Production checked-walker candidates with checked file/path stability, not an atomic filesystem snapshot or successful ingest admission. Later10MiB/binary/unreadable/parser skips must be accounted separately. Source contents/declarations are neither printed nor retained.",
        "files":rows});
    let encoded = serde_json::to_vec(&report)?;
    ensure!(encoded.len() <= 32 * 1024 * 1024, "Manifest exceeds32MiB evidence bound");
    let mut file = File::options().create_new(true).write(true).open(output)?;
    file.write_all(&encoded)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    println!("{}", json!({"candidate_files":report["candidate_files"],"candidate_bytes":total,
        "source_contents_retained":false}));
    Ok(())
}
