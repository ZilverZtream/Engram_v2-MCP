//! Windows, quiescent-source Registry acquisition. redb 2.6 has no
//! ReadOnlyDatabase: its writable open/abort runs ONLY against InMemoryBackend.
//! No Registry/AppState construction, source-file walk, adoption or disk DB fork.
//! Output is sensitive raw evidence, never compatibility/ownership permission.
use anyhow::{Context, Result, bail, ensure};
use redb::{Database, ReadableTable, StorageBackend, TableDefinition};
use serde::Serialize;
use std::{fs::{File, OpenOptions}, io::{Read, Seek, SeekFrom, Write}, path::{Path, PathBuf},
    sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}}, time::{Duration, Instant}};

const PROJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("projects");
const RULES: TableDefinition<&str, &[u8]> = TableDefinition::new("repo_rules");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const MAX_DB: u64 = 256 * 1024 * 1024;
const MAX_RAW: usize = 8 * 1024 * 1024;
const MAX_VALUE: usize = 4 * 1024 * 1024;
const MAX_ROWS: usize = 8192;
const MAX_OUTPUT: usize = 32 * 1024 * 1024;
// Mirrors the exact existing keys in registry/{source_head,reindex,rule_ownership}.
// File source_index_version/extraction identity live in Graph, not Registry.
const KEYS: &[&str] = &["active_generation", "source_head_revision_v1", "source_head_required_v1",
    "source_snapshots_required_v1", "source_legacy_absence_attested_v1", "source_identity_verified_v1",
    "source_identity_epoch_v1", "quality_corpus_revision", "reindex_requirements_v1"];

fn check(deadline: Instant) -> Result<()> {
    ensure!(Instant::now() < deadline, "Inspection deadline exceeded; no evidence output published");
    Ok(())
}

/// Bounds SDK requests before its existing backend allocates; every write is RAM.
struct MemoryOnly {
    inner: redb::backends::InMemoryBackend,
    read_bytes: AtomicU64,
    deadline: Instant,
}
impl std::fmt::Debug for MemoryOnly {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SDK logging may format its backend. Never expose the Registry buffer.
        formatter.write_str("BoundedMemoryOnlyRegistryFork")
    }
}
impl MemoryOnly {
    fn admit(&self) -> std::io::Result<()> {
        if Instant::now() >= self.deadline { return Err(std::io::Error::other("Memory inspection deadline exceeded")); }
        Ok(())
    }
}
impl StorageBackend for MemoryOnly {
    fn len(&self) -> std::io::Result<u64> { self.admit()?; self.inner.len() }
    fn read(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.admit()?;
        let end = offset.checked_add(len as u64).ok_or_else(|| std::io::Error::other("Memory read range overflow"))?;
        // The locked SDK backend uses unchecked offset+len before indexing.
        // Refuse corrupt ranges before its allocation/access, not after panic.
        if len > 8 * 1024 * 1024 || end > self.inner.len()? {
            return Err(std::io::Error::other("Memory inspection read range/budget exceeded"));
        }
        let inspected = self.read_bytes.fetch_add(len as u64, Ordering::Relaxed);
        if inspected.checked_add(len as u64).is_none_or(|n| n > 512 * 1024 * 1024) {
            return Err(std::io::Error::other("Memory inspection read budget exceeded"));
        }
        self.inner.read(offset, len)
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.admit()?;
        if len > MAX_DB { return Err(std::io::Error::other("Memory database length budget exceeded")); }
        self.inner.set_len(len)
    }
    fn sync_data(&self, _: bool) -> std::io::Result<()> { self.admit() }
    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        self.admit()?;
        let end = offset.checked_add(data.len() as u64).ok_or_else(|| std::io::Error::other("Memory write range overflow"))?;
        if end > MAX_DB || end > self.inner.len()? {
            return Err(std::io::Error::other("Memory database write range/budget exceeded"));
        }
        self.inner.write(offset, data)
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::{fs::{MetadataExt, OpenOptionsExt}, io::AsRawHandle};
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Time { low: u32, high: u32 }
    #[repr(C)]
    #[derive(Default)]
    struct Info { attributes: u32, creation: Time, access: Time, write: Time, volume: u32,
        size_high: u32, size_low: u32, links: u32, index_high: u32, index_low: u32 }
    #[link(name = "kernel32")]
    unsafe extern "system" { fn GetFileInformationByHandle(handle: *mut std::ffi::c_void, info: *mut Info) -> i32; }
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub(super) struct Identity { volume: u32, file_index: u64, pub(super) bytes: u64, last_write_filetime: u64 }
    pub(super) fn identity(file: &File) -> Result<Identity> {
        let mut info = Info::default();
        // SAFETY: live File owns this handle; repr(C) exactly matches the Win32
        // BY_HANDLE_FILE_INFORMATION DWORD/FILETIME layout and output lifetime.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        ensure!(info.attributes & 0x400 == 0, "Reparse database refused");
        Ok(Identity { volume: info.volume, file_index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
            bytes: (u64::from(info.size_high) << 32) | u64::from(info.size_low),
            last_write_filetime: (u64::from(info.write.high) << 32) | u64::from(info.write.low) })
    }
    pub(super) fn no_reparse(path: &Path) -> Result<()> {
        for ancestor in path.ancestors() {
            ensure!(std::fs::symlink_metadata(ancestor)?.file_attributes() & 0x400 == 0,
                "Reparse input/output ancestor refused");
        }
        Ok(())
    }
    pub(super) fn source(path: &Path) -> Result<File> {
        // FILE_SHARE_READ only: the OS rejects an existing/new write or delete
        // handle. Kept until all SDK memory owners and final verification close.
        Ok(OpenOptions::new().read(true).share_mode(1).open(path)
            .context("Cannot acquire read-only deny-write/delete guard; quiesce the actual writer first")?)
    }
    pub(super) fn output(path: &Path) -> Result<File> {
        Ok(OpenOptions::new().write(true).create_new(true).share_mode(0).open(path)?)
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes { out.push(DIGITS[(byte >> 4) as usize] as char); out.push(DIGITS[(byte & 15) as usize] as char); }
    out
}
#[derive(Serialize)]
struct Row { table: &'static str, key_hex: String, present: bool, bytes: usize,
    blake3: Option<String>, encoding: &'static str, raw_hex: Option<String> }
struct Capture { rows: Vec<Row>, raw_bytes: usize, digest: blake3::Hasher, deadline: Instant }
impl Capture {
    fn push(&mut self, table: &'static str, key: &str, value: Option<&[u8]>) -> Result<()> {
        check(self.deadline)?;
        ensure!(self.rows.len() < MAX_ROWS && key.len() <= 2048, "Inventory row/key budget exceeded; no partial inventory published");
        let bytes = value.map_or(0, <[u8]>::len);
        ensure!(bytes <= MAX_VALUE, "Inventory value exceeds4MiB; no partial inventory published");
        let next = self.raw_bytes.checked_add(key.len()).and_then(|n| n.checked_add(bytes))
            .context("Raw inventory size overflow")?;
        ensure!(next <= MAX_RAW, "Raw inventory exceeds8MiB; no partial inventory published");
        // Admit before allocating hex copies. Domain + lengths bind exact keys,
        // bytes and absences; no JSON/schema normalization or guessed provenance.
        for part in [table.as_bytes(), key.as_bytes()] {
            self.digest.update(&(part.len() as u64).to_be_bytes()); self.digest.update(part);
        }
        self.digest.update(&[u8::from(value.is_some())]);
        self.digest.update(&(bytes as u64).to_be_bytes());
        if let Some(raw) = value { self.digest.update(raw); }
        self.raw_bytes = next;
        self.rows.push(Row { table, key_hex: hex(key.as_bytes()), present: value.is_some(), bytes,
            blake3: value.map(|raw| blake3::hash(raw).to_hex().to_string()), encoding: "hex",
            raw_hex: value.map(hex) });
        Ok(())
    }
}

fn collect(db: &Database, deadline: Instant) -> Result<Capture> {
    let tx = db.begin_read()?;
    let projects = tx.open_table(PROJECTS)?;
    let rules = tx.open_table(RULES)?;
    let meta = tx.open_table(META)?;
    let mut capture = Capture { rows: Vec::new(), raw_bytes: 0, digest: blake3::Hasher::new(), deadline };
    capture.digest.update(b"engram:closed-registry-selected-raw:v1\0");
    for (ordinal, entry) in projects.iter()?.enumerate() {
        check(deadline)?;
        ensure!(ordinal < 128, "Project count exceeds128; no partial inventory published");
        let (key, value) = entry?;
        let project = key.value();
        ensure!(!project.is_empty() && project.chars().all(|ch| !matches!(ch, '\0' | '\r' | '\n')) && project.len() <= 1024,
            "Ambiguous project key; no associated metadata scope guessed");
        capture.push("projects", project, Some(value.value()))?;
        let prefix = format!("{project}\0");
        for (ordinal, entry) in rules.range(prefix.as_str()..)?.enumerate() {
            let (key, value) = entry?;
            if !key.value().starts_with(&prefix) { break; }
            ensure!(ordinal < 2048, "Project rule count exceeds2048; no partial inventory published");
            capture.push("repo_rules", key.value(), Some(value.value()))?;
        }
        let prefix = format!("{project}\0quality_source:");
        for (ordinal, entry) in meta.range(prefix.as_str()..)?.enumerate() {
            let (key, value) = entry?;
            if !key.value().starts_with(&prefix) { break; }
            ensure!(ordinal < 256, "Project receipt count exceeds256; no partial inventory published");
            capture.push("meta", key.value(), Some(value.value()))?;
        }
        for suffix in KEYS {
            let key = format!("{project}\0{suffix}");
            let value = meta.get(key.as_str())?;
            capture.push("meta", &key, value.as_ref().map(|v| v.value()))?;
        }
    }
    let sequence = "\0reindex_revision_sequence_v1";
    let value = meta.get(sequence)?;
    capture.push("meta", sequence, value.as_ref().map(|v| v.value()))?;
    Ok(capture)
}

struct Encoded(Vec<u8>);
impl Write for Encoded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().checked_add(bytes.len()).is_none_or(|n| n > MAX_OUTPUT) {
            return Err(std::io::Error::other("Encoded inventory exceeds32MiB"));
        }
        self.0.extend_from_slice(bytes); Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[cfg(windows)]
fn acquire(input: &Path, output: &Path) -> Result<()> {
    acquire_memory_mode(input, output, false)
}

#[cfg(windows)]
fn acquire_memory_mode(input: &Path, output: &Path, allow_memory_recovery: bool) -> Result<()> {
    ensure!(input.is_absolute() && output.is_absolute(), "Supply explicit absolute database/output paths");
    windows::no_reparse(input)?;
    let parent = output.parent().context("Output parent missing")?;
    windows::no_reparse(parent)?;
    let input = input.canonicalize()?;
    let parent = parent.canonicalize()?;
    let db_parent = input.parent().context("Database parent missing")?;
    ensure!(!parent.starts_with(db_parent) && !db_parent.starts_with(&parent),
        "Output must be in a separate existing private evidence tree, outside the database tree");
    let leaf = output.file_name().and_then(|name| name.to_str()).context("Output name must be valid UTF8")?;
    let output = engram_core::security::safe_join(&parent, leaf)?;
    ensure!(!output.try_exists()?, "Existing output is preserved; use a fresh evidence filename");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut source = windows::source(&input)?;
    ensure!(source.metadata()?.is_file(), "Database must be an existing regular file");
    let before = windows::identity(&source)?;
    ensure!(before.bytes > 0 && before.bytes <= MAX_DB, "Source database exceeds256MiB or is empty; no memory fork allocated");
    let memory = MemoryOnly { inner: redb::backends::InMemoryBackend::new(), read_bytes: AtomicU64::new(0), deadline };
    memory.set_len(before.bytes)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut source_hash = blake3::Hasher::new();
    let mut position = 0_u64;
    while position < before.bytes {
        check(deadline)?;
        let count = usize::try_from((before.bytes - position).min(buffer.len() as u64))?;
        source.read_exact(&mut buffer[..count])?;
        source_hash.update(&buffer[..count]); memory.write(position, &buffer[..count])?;
        position += count as u64;
    }
    ensure!(source.read(&mut buffer[..1])? == 0, "Source grew during guarded acquisition");
    let source_blake3 = source_hash.finalize().to_hex().to_string();
    let mut builder = Database::builder();
    builder.set_cache_size(16 * 1024 * 1024);
    let repair_observed = Arc::new(AtomicBool::new(false));
    let callback_observed = repair_observed.clone();
    builder.set_repair_callback(move |session| {
        callback_observed.store(true, Ordering::SeqCst);
        if !allow_memory_recovery { session.abort(); }
    });
    let db = builder.create_with_backend(memory).map_err(|error| anyhow::anyhow!(
        "Memory-only redb open refused; source file untouched; memory recovery allowed={}, repair callback observed={}: {error}",
        allow_memory_recovery, repair_observed.load(Ordering::SeqCst)))?;
    let capture = collect(&db, deadline)?;
    // The writable SDK's open/abort/close touches only its memory copy. Close it
    // before final original-file verification and output publication.
    drop(db);
    source.seek(SeekFrom::Start(0))?;
    let mut after_hash = blake3::Hasher::new();
    let mut checked = 0_u64;
    loop {
        check(deadline)?;
        let count = source.read(&mut buffer)?;
        if count == 0 { break; }
        checked = checked.checked_add(count as u64).context("Verification byte overflow")?;
        ensure!(checked <= before.bytes, "Source length changed during acquisition");
        after_hash.update(&buffer[..count]);
    }
    ensure!(checked == before.bytes && after_hash.finalize().to_hex().as_str() == source_blake3
        && windows::identity(&source)? == before, "Source identity/bytes/mtime changed; no output published");
    let rebound = windows::source(&input)?;
    ensure!(windows::identity(&rebound)? == before, "Source pathname no longer identifies the guarded file");
    drop(rebound);
    let snapshot = capture.digest.finalize().to_hex().to_string();
    let report = serde_json::json!({"format":"engram-closed-registry-selected-raw-v1", "source_db":input,
        "source_identity":before,"source_blake3":source_blake3,"source_unchanged_verified":true,
        "backend":"OS read-only deny-write/delete guard; redb2.6 writable open/abort/recovery ONLY on bounded InMemoryBackend fork",
        "memory_recovery":{"allowed":allow_memory_recovery,
            "repair_callback_observed":repair_observed.load(Ordering::SeqCst),
            "interpretation":"This records the SDK repair callback, not a clean source shutdown or a compatibility/authority decision. Recovery, if allowed and needed, touches only the disposable RAM fork; no source-disk repair or upgrade."},
        "raw_inventory_blake3":snapshot,"raw_inventory_bytes":capture.raw_bytes,"records":capture.rows,
        "bounds":{"source_and_memory_bytes":MAX_DB,"cache_bytes":16777216,"raw_bytes":MAX_RAW,
            "encoded_bytes":MAX_OUTPUT,"projects":128,"records":MAX_ROWS,"per_value_bytes":MAX_VALUE,
            "per_project_rules":2048,"per_project_receipts":256,"seconds":60,"sdk_read_bytes":536870912},
        "coverage":"All PROJECTS records; rules and quality_source: receipts scoped only to those project keys; exact allowlisted existing metadata and explicit absences. Orphaned rows outside PROJECTS scopes, unrelated META, graph/index/native stores, source files and configuration are not exported or certified.",
        "interpretation":"Sensitive raw evidence only, not authority/adoption/source-freshness/compatibility/19-gate approval. No source-disk writes, repair, upgrade, metadata initialization, project scan or source fingerprint guess. Explicitly allowed SDK recovery is confined to the RAM fork. Values including malformed JSON/non-UTF8 are hex with exact BLAKE3. Cooperative deadline and logical byte caps are not an OS RSS/wall-time guarantee."});
    let mut encoded = Encoded(Vec::new());
    serde_json::to_writer(&mut encoded, &report)?;
    check(deadline)?;
    let mut file = windows::output(&output)?;
    // Immediate ownership: on write/sync failure this exact partial output is
    // retained and named, never overwritten/deleted or reported as complete.
    let write = (|| -> Result<()> { file.write_all(&encoded.0)?; file.sync_all()?; Ok(()) })();
    drop(file);
    if let Err(error) = write { bail!("Partial raw output retained at {} for exact review: {error:#}", output.display()); }
    drop(source);
    println!("complete selected raw inventory: records={}, raw_bytes={}, inventory_blake3={}, output={}",
        report["records"].as_array().map_or(0, Vec::len), report["raw_inventory_bytes"], snapshot, output.display());
    Ok(())
}

fn main() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    ensure!((args.len() == 4 || (args.len() == 5 && args[4] == "--allow-memory-recovery"))
        && args[0] == "--db" && args[2] == "--output",
        "Usage: registry_transition_inventory --db ABSOLUTE_CLOSED_REGISTRY --output ABSOLUTE_NEW_PRIVATE_OUTPUT [--allow-memory-recovery]");
    let input = PathBuf::from(&args[1]); let output = PathBuf::from(&args[3]);
    #[cfg(windows)] { if args.len() == 5 { acquire_memory_mode(&input, &output, true) } else { acquire(&input, &output) } }
    #[cfg(not(windows))] { let _ = (input, output); bail!("Windows deny-write/delete guard required; this tool refuses other platforms") }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    fn owned() -> Result<tempfile::TempDir> {
        let parent = PathBuf::from(std::env::var_os("ENGRAM_REGISTRY_INVENTORY_FIXTURE_ROOT")
            .context("Set the explicitly owned fixture parent; no AppData temp fallback")?);
        ensure!(parent.is_absolute() && parent.is_dir(), "Fixture parent must already exist");
        Ok(tempfile::Builder::new().prefix("registry-inventory-").tempdir_in(parent)?)
    }
    fn seed(root: &Path, large: bool) -> Result<(PathBuf, PathBuf)> {
        let dir = root.join("db"); std::fs::create_dir(&dir)?;
        let evidence = root.join("evidence"); std::fs::create_dir(&evidence)?;
        let path = dir.join("registry.redb");
        let mut builder = Database::builder(); builder.set_cache_size(2 * 1024 * 1024);
        let db = builder.create(&path)?;
        let tx = db.begin_write()?;
        {
            tx.open_table(PROJECTS)?.insert("project", b" {\"project_id\":\"project\", \"unknown\":true} \n".as_slice())?;
            let mut rules = tx.open_table(RULES)?;
            rules.insert("project\0raw", b"{broken\xff}".as_slice())?;
            if large { rules.insert("project\0oversized", vec![b'x'; MAX_VALUE + 1].as_slice())?; }
            let mut meta = tx.open_table(META)?;
            meta.insert("project\0quality_source:owned", b"{pending malformed\xfe".as_slice())?;
            meta.insert("project\0active_generation", b"damaged-owned-head".as_slice())?;
            meta.insert("project\0openai_api_key", b"SENSITIVE_UNRELATED_SENTINEL".as_slice())?;
            meta.insert("other\0quality_source:orphan", b"UNREGISTERED_PROJECT_SENTINEL".as_slice())?;
        }
        tx.commit()?; drop(db);
        Ok((path, evidence.join("inventory.json")))
    }
    #[test]
    fn actual_acquisition_preserves_source_bytes_mtime_identity_and_exact_malformed_evidence() -> Result<()> {
        let owned = owned()?;
        let (db, output) = seed(owned.path(), false)?;
        let before = std::fs::read(&db)?;
        let before_info = { let file = File::open(&db)?; windows::identity(&file)? };
        acquire(&db, &output)?;
        ensure!(std::fs::read(&db)? == before);
        let after_info = { let file = File::open(&db)?; windows::identity(&file)? };
        ensure!(before_info == after_info, "Source mtime/file identity/length changed");
        let raw = std::fs::read(&output)?;
        let report: serde_json::Value = serde_json::from_slice(&raw)?;
        let rows = report["records"].as_array().context("Missing actual records")?;
        ensure!(rows.iter().any(|row| row["table"] == "repo_rules" && row["raw_hex"] == "7b62726f6b656eff7d"
            && row["blake3"] == blake3::hash(b"{broken\xff}").to_hex().as_str()), "Exact nonUTF8 malformed rule lost");
        ensure!(rows.iter().any(|row| row["table"] == "meta" && row["raw_hex"] == "7b70656e64696e67206d616c666f726d6564fe"),
            "Exact malformed receipt lost");
        ensure!(rows.iter().any(|row| row["key_hex"] == hex(b"project\0source_head_required_v1") && row["present"] == false),
            "Missing authority was initialized or concealed");
        let encoded = std::str::from_utf8(&raw)?;
        ensure!(!encoded.contains(&hex(b"SENSITIVE_UNRELATED_SENTINEL"))
            && !encoded.contains(&hex(b"UNREGISTERED_PROJECT_SENTINEL")), "Unselected metadata escaped whitelist");
        ensure!(acquire(&db, &output).is_err() && std::fs::read(&output)? == raw, "Existing output overwritten");
        // These exclusive/reopened handles prove the real source/output guards
        // are gone before removing this sole fixture; neither write is issued.
        drop(OpenOptions::new().write(true).share_mode(0).open(&db)?);
        drop(OpenOptions::new().read(true).share_mode(0).open(&output)?);
        owned.close()?; Ok(())
    }
    #[test]
    fn actual_open_writer_is_refused_before_output_and_reader_works_after_writer_closes() -> Result<()> {
        let owned = owned()?;
        let (path, output) = seed(owned.path(), false)?;
        let mut builder = Database::builder(); builder.set_cache_size(2 * 1024 * 1024);
        let guarded = windows::source(&path)?;
        let identity = windows::identity(&guarded)?;
        ensure!(builder.open(&path).is_err(), "New actual redb writer bypassed held deny-write guard");
        ensure!(windows::identity(&guarded)? == identity);
        drop(guarded);
        let writer = builder.open(&path)?;
        let before = std::fs::metadata(&path)?.modified()?;
        let error = acquire(&path, &output).err().context("Actual writer was not refused")?;
        ensure!(format!("{error:#}").contains("quiesce the actual writer first") && !output.exists());
        ensure!(std::fs::metadata(&path)?.modified()? == before);
        { let tx = writer.begin_read()?; ensure!(tx.open_table(RULES)?.get("project\0raw")?.unwrap().value() == b"{broken\xff}"); }
        drop(writer);
        acquire(&path, &output)?;
        owned.close()?; Ok(())
    }
    #[test]
    fn real_oversized_raw_record_refuses_without_output_or_source_changes() -> Result<()> {
        let owned = owned()?;
        let (path, output) = seed(owned.path(), true)?;
        let before = std::fs::read(&path)?;
        let before_info = { let file = File::open(&path)?; windows::identity(&file)? };
        let error = acquire(&path, &output).err().context("Oversized stored record was accepted")?;
        ensure!(format!("{error:#}").contains("Inventory value exceeds4MiB") && !output.exists());
        ensure!(std::fs::read(&path)? == before);
        let after_info = { let file = File::open(&path)?; windows::identity(&file)? };
        ensure!(before_info == after_info);
        drop(OpenOptions::new().write(true).share_mode(0).open(&path)?);
        owned.close()?; Ok(())
    }

    /// One owned fake database/child, not a reusable process framework. Its
    /// directory is retained/accounted if actual child teardown cannot prove exit.
    struct OwnedCrash {
        root: PathBuf,
        parent: PathBuf,
        child: Option<std::process::Child>,
        removed: bool,
    }
    impl OwnedCrash {
        fn new() -> Result<Self> {
            let parent = PathBuf::from(std::env::var_os("ENGRAM_REGISTRY_INVENTORY_FIXTURE_ROOT")
                .context("Set the explicitly owned fixture parent")?).canonicalize()?;
            let nonce = format!("{}-{}", std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
            let root = parent.join(format!("registry-inventory-crash-{nonce}"));
            std::fs::create_dir(&root)?;
            Ok(Self { root, parent, child: None, removed: false })
        }
        fn reap(&mut self) -> Result<()> {
            if let Some(child) = self.child.as_mut() {
                if child.try_wait()?.is_none() { child.kill()?; child.wait()?; }
            }
            drop(self.child.take()); // Actual process handle closed before deletion.
            Ok(())
        }
        fn close(&mut self) -> Result<()> {
            self.reap()?;
            let checked = self.root.canonicalize()?;
            ensure!(checked.parent() == Some(self.parent.as_path())
                && checked.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("registry-inventory-crash-")),
                "Owned crash fixture cleanup escaped its registered parent");
            windows::no_reparse(&self.root)?;
            std::fs::remove_dir_all(&self.root)?;
            self.removed = true;
            Ok(())
        }
    }
    impl Drop for OwnedCrash {
        fn drop(&mut self) {
            if self.removed { return; }
            if let Err(error) = self.close() {
                let pid = self.child.as_ref().map(std::process::Child::id);
                let record = serde_json::json!({"path":self.root,"child_pid":pid,"driver_pid":std::process::id(),
                    "reason":format!("{error:#}"),"cleanup_condition":"Preserve this exact owned fixture until recorded child has exited, handles are closed, and path ownership is reverified."});
                let path = self.parent.join(format!("{}-retained.json", self.root.file_name().unwrap().to_string_lossy()));
                if let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(&path) {
                    let _ = serde_json::to_writer(&mut file, &record); let _ = file.sync_all();
                }
                eprintln!("Owned crash fixture retained: {}; record={}; reason={error:#}", self.root.display(), path.display());
            }
        }
    }

    #[test]
    #[ignore = "owned child role: launched only by the exact crash/recovery parent"]
    fn unclean_registry_child_owned_only() -> Result<()> {
        let owner = PathBuf::from(std::env::var_os("ENGRAM_REGISTRY_CRASH_OWNER").context("Missing owned-child owner path")?);
        let nonce = std::env::var("ENGRAM_REGISTRY_CRASH_NONCE")?;
        let parent = PathBuf::from(std::env::var_os("ENGRAM_REGISTRY_INVENTORY_FIXTURE_ROOT")
            .context("Missing explicit fixture parent")?).canonicalize()?;
        let root = owner.parent().context("Owner parent missing")?.canonicalize()?;
        ensure!(root.parent() == Some(parent.as_path()) && owner.file_name().is_some_and(|n| n == "unclean-child-owner.json")
            && root.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("registry-inventory-crash-")),
            "Child target is not an exact owned fixture");
        let mut file = File::open(&owner)?;
        let mut encoded = Vec::new(); std::io::Read::take(&mut file, 4097).read_to_end(&mut encoded)?; drop(file);
        ensure!(encoded.len() <= 4096, "Owned-child marker bound exceeded");
        let marker: serde_json::Value = serde_json::from_slice(&encoded)?;
        ensure!(marker["nonce"] == nonce && marker["purpose"] == "owned-unclean-registry-fixture-only",
            "Owned-child nonce/purpose mismatch");
        let path = PathBuf::from(marker["db"].as_str().context("Missing owned database path")?).canonicalize()?;
        ensure!(path == root.join("db/registry.redb").canonicalize()?, "Child database escaped owned fixture");
        let mut builder = Database::builder(); builder.set_cache_size(2 * 1024 * 1024);
        let db = builder.open(&path)?;
        let mut tx = db.begin_write()?;
        tx.set_durability(redb::Durability::Immediate);
        tx.set_quick_repair(false);
        {
            tx.open_table(RULES)?.insert("project\0crash-committed", b"{committed-unclean\xff}".as_slice())?;
            let mut meta = tx.open_table(META)?;
            meta.insert("project\0quality_source:crash", b"{committed-receipt\xfe}".as_slice())?;
            // Six MiB of generated unrelated data makes a real nontrivial
            // memory-recovery walk; it must never appear in selected output.
            meta.insert("project\0zz_owned_crash_padding", vec![b'p'; 6 * 1024 * 1024].as_slice())?;
        }
        tx.commit()?;
        let uncommitted = db.begin_write()?;
        uncommitted.open_table(RULES)?.insert("project\0crash-uncommitted", b"POISON_UNCOMMITTED_MUST_NOT_EXPORT".as_slice())?;
        // This second real transaction intentionally has NO commit. Recovery
        // must select the earlier committed snapshot, never this poison row.
        // Deliberately simulate a crash AFTER durable commit, skipping Database
        // Drop. Parent owns/reaps this process and every file; no resource is
        // leaked or forgotten in the parent and no live/source DB is used here.
        std::process::exit(93)
    }

    #[test]
    fn actual_unclean_commit_requires_explicit_ram_recovery_and_never_repairs_the_source() -> Result<()> {
        use std::os::windows::process::CommandExt;
        let mut owned = OwnedCrash::new()?;
        let (path, output) = seed(&owned.root, false)?;
        let nonce = format!("owned-parent-{}", std::process::id());
        let owner = owned.root.join("unclean-child-owner.json");
        {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&owner)?;
            serde_json::to_writer(&mut file, &serde_json::json!({"nonce":nonce,"db":path,
                "purpose":"owned-unclean-registry-fixture-only","parent_pid":std::process::id()}))?;
            file.sync_all()?;
        }
        let child = std::process::Command::new(std::env::current_exe()?)
            .args(["--ignored", "--exact", "tests::unclean_registry_child_owned_only", "--test-threads=1"])
            .env("ENGRAM_REGISTRY_CRASH_OWNER", &owner).env("ENGRAM_REGISTRY_CRASH_NONCE", &nonce)
            .current_dir(&owned.root).creation_flags(0x08000000)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn()?;
        owned.child = Some(child); // Own immediately, before any fallible wait.
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = owned.child.as_mut().unwrap().try_wait()? { break status; }
            ensure!(Instant::now() < deadline, "Owned crash fixture child deadline reached");
            std::thread::sleep(Duration::from_millis(10));
        };
        owned.reap()?;
        ensure!(status.code() == Some(93), "Owned child did not reach actual durable commit/crash: {status}");
        let before = std::fs::read(&path)?;
        ensure!(before.len() >= 6 * 1024 * 1024 && before.len() <= 32 * 1024 * 1024,
            "Crash fixture must be real, bounded, nontrivial data");
        let before_hash = blake3::hash(&before);
        let before_info = { let file = File::open(&path)?; windows::identity(&file)? };
        let error = acquire(&path, &output).err().context("Strict mode accepted an actual unclean source")?;
        ensure!(format!("{error:#}").contains("Database repair aborted") && !output.exists());
        ensure!(blake3::hash(&std::fs::read(&path)?) == before_hash);
        acquire_memory_mode(&path, &output, true)?;
        let report: serde_json::Value = serde_json::from_slice(&std::fs::read(&output)?)?;
        ensure!(report["memory_recovery"]["allowed"] == true
            && report["memory_recovery"]["repair_callback_observed"] == true,
            "Explicit RAM mode did not observe the SDK recovery callback");
        let rows = report["records"].as_array().context("Recovered rows missing")?;
        ensure!(rows.iter().any(|row| row["table"] == "repo_rules"
            && row["raw_hex"] == "7b636f6d6d69747465642d756e636c65616eff7d"
            && row["blake3"] == blake3::hash(b"{committed-unclean\xff}").to_hex().as_str()),
            "Actual committed malformed raw rule not recovered exactly");
        ensure!(rows.iter().any(|row| row["table"] == "meta"
            && row["raw_hex"] == "7b636f6d6d69747465642d72656365697074fe7d"),
            "Actual committed malformed raw receipt not recovered exactly");
        ensure!(!rows.iter().any(|row| row["key_hex"] == "70726f6a6563740063726173682d756e636f6d6d6974746564")
            && !serde_json::to_string(&report)?.contains(&hex(b"POISON_UNCOMMITTED_MUST_NOT_EXPORT")),
            "Memory recovery exposed an actual uncommitted transaction's poison row");
        ensure!(report["source_blake3"] == before_hash.to_hex().as_str()
            && blake3::hash(&std::fs::read(&path)?) == before_hash);
        let after_info = { let file = File::open(&path)?; windows::identity(&file)? };
        ensure!(after_info == before_info, "RAM recovery changed actual source identity/mtime/length");
        let strict_again = output.with_file_name("strict-after-ram.json");
        ensure!(acquire(&path, &strict_again).is_err() && !strict_again.exists(),
            "RAM mode must not make the original source appear repaired to strict mode");
        drop(OpenOptions::new().write(true).share_mode(0).open(&path)?);
        drop(OpenOptions::new().read(true).share_mode(0).open(&output)?);
        owned.close()?;
        Ok(())
    }
}
