//! Durable vector routing, independent of the unchanged source generation.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Mutex, Weak};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OwnedTable {
    pub version: u32,
    pub project: String,
    pub table: String,
    pub creation_token: String,
    pub model_tag: String,
    pub dimension: usize,
}

impl OwnedTable {
    pub fn new(
        project: &str,
        table: &str,
        provider: &Arc<dyn engram_ml::Embedder>,
    ) -> anyhow::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let creation_token = blake3::hash(
            format!(
                "{project}\0{table}\0{}\0{stamp}\0{serial}",
                std::process::id()
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        Ok(Self {
            version: 1,
            project: project.to_owned(),
            table: table.to_owned(),
            creation_token,
            model_tag: provider.model_tag(),
            dimension: provider.dimension(),
        })
    }

    fn validate(&self, project: &str) -> anyhow::Result<()> {
        check_project(project)?;
        anyhow::ensure!(
            self.version == 1
                && self.project == project
                && self.dimension > 0
                && !self.table.is_empty()
                && self.table.len() <= 384
                && self
                    .table
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                && self.creation_token.len() == 64
                && self.creation_token.bytes().all(|c| c.is_ascii_hexdigit())
                && !self.model_tag.is_empty()
                && self.model_tag.len() <= 1024,
            "VEC1: invalid vector ownership descriptor"
        );
        Ok(())
    }

    pub fn metadata(&self) -> HashMap<String, String> {
        [
            ("engram.ownership.version", self.version.to_string()),
            ("engram.ownership.project", self.project.clone()),
            ("engram.ownership.table", self.table.clone()),
            ("engram.ownership.token", self.creation_token.clone()),
            ("engram.embedding.identity", self.model_tag.clone()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
    }

    pub async fn validate_table(&self, table: &lancedb::Table) -> anyhow::Result<()> {
        self.validate(&self.project)?;
        crate::vector::validate_table_schema(table, self.dimension).await?;
        let schema = table.schema().await?;
        anyhow::ensure!(
            self.metadata()
                .iter()
                .all(|(key, value)| schema.metadata().get(key) == Some(value)),
            "VEC1: vector table ownership does not match its receipt; data preserved"
        );
        Ok(())
    }

    pub(super) fn lease_key(&self) -> String {
        format!("{}:{}", self.table, self.creation_token)
    }
}

/// Every provider view of one physical epoch holds this same lease identity.
pub(super) struct PhysicalLease;

#[derive(Default)]
struct LeaseState {
    readers: HashMap<String, Weak<PhysicalLease>>,
    pruning: Option<String>,
    // A failed native read has no dependency-wide join proof. Do not infer
    // quiescence from dropping its future; only a fresh runtime resets this.
    unproven: bool,
}

pub(super) struct PruneLease {
    routes: Arc<Routes>,
    key: String,
}

impl Drop for PruneLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.routes.leases.lock() {
            if state.pruning.as_deref() == Some(self.key.as_str()) {
                state.pruning = None;
            } else {
                state.unproven = true;
            }
            if std::thread::panicking() { state.unproven = true; }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub version: u32,
    pub project: String,
    pub table: String,
    pub model_tag: String,
    pub dimension: usize,
    pub creation_token: String,
    /// Recovery baseline, not a checksum of subsequent ordinary writes.
    pub baseline_rows: usize,
    pub baseline_digest: String,
    pub retired: Vec<OwnedTable>,
}

impl Record {
    pub fn ownership(&self) -> OwnedTable {
        OwnedTable {
            version: 1,
            project: self.project.clone(),
            table: self.table.clone(),
            creation_token: self.creation_token.clone(),
            model_tag: self.model_tag.clone(),
            dimension: self.dimension,
        }
    }
}

pub(super) struct Epoch {
    pub record: Option<Record>,
    pub table: String,
    pub query_embedder: Option<Arc<dyn engram_ml::Embedder>>,
    _lease: Arc<PhysicalLease>,
    _catalog: Arc<snapshots::Catalog>,
}

struct EpochData {
    record: Option<Record>,
    table: String,
    query_embedder: Option<Arc<dyn engram_ml::Embedder>>,
}

/// Per-table failures preserve the affected receipts without preventing a new
/// rebuild below the retirement limit. Route/coordinator failures stay fatal.
#[derive(Debug, thiserror::Error)]
#[error(
    "Vector cleanup deferred for {deferred} retired table(s), collected {collected}: {first_error:#}"
)]
pub(super) struct RetiredCleanupDeferred {
    deferred: usize,
    collected: usize,
    first_error: anyhow::Error,
}

pub(super) struct Routes {
    root: PathBuf,
    lance_root: PathBuf,
    current: Mutex<HashMap<String, Arc<EpochData>>>,
    leases: Mutex<LeaseState>,
    pub(super) read_slots: Arc<tokio::sync::Semaphore>,
    bootstrap: Arc<tokio::sync::Mutex<()>>,
    publication: Mutex<Option<String>>,
    _process_lock: std::fs::File,
    #[cfg(test)]
    fail_activation: std::sync::atomic::AtomicBool,
}

pub(super) fn canonical_table(project: &str) -> String {
    format!("project_{}", project.replace('-', "_"))
}

fn check_project(project: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !project.is_empty()
            && project
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Invalid project identity for vector routing"
    );
    Ok(())
}

impl Routes {
    /// Observe existing routes/weak slots without pinning or retiring anything.
    pub fn retention_observation(&self, project: &str) -> serde_json::Value {
        use serde_json::json;
        let Ok(current) = self.current.try_lock() else { return json!({"status":"busy_or_unavailable"}); };
        let Ok(leases) = self.leases.try_lock() else { return json!({"status":"busy_or_unavailable"}); };
        let data = current.get(project);
        let mut live = 0;
        let mut dead = 0;
        let mut references = 0;
        for lease in leases.readers.values().take(512) {
            let count = lease.strong_count();
            if count == 0 { dead += 1; } else { live += 1; references += count; }
        }
        json!({"status":"observed","cached_route_for_project":data.is_some(),
            "retired_owned_tables":data.and_then(|v|v.record.as_ref()).map(|r|r.retired.len()),
            "route_catalog_cached_projects":current.len(),"live_lease_objects":live,
            "dead_weak_slots":dead,"live_lease_references":references,
            "weak_slots_not_inspected":leases.readers.len().saturating_sub(live+dead),
            "history_prune_active":leases.pruning.is_some(),"native_read_quiescence_unproven":leases.unproven,
            "native_read_slots_available":self.read_slots.available_permits(),
            "scope":"Weak lease counts cover this shared route catalog, not necessarily only the requested project. No Weak is upgraded or removed; dead slots may persist until normal work prunes them."})
    }

    fn reader(
        &self,
        data: Arc<EpochData>,
        catalog: Arc<snapshots::Catalog>,
    ) -> anyhow::Result<Arc<Epoch>> {
        let key = data
            .record
            .as_ref()
            .map(|r| r.ownership().lease_key())
            .unwrap_or_else(|| format!("{}:legacy", data.table));
        let mut leases = self
            .leases
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector lease registry poisoned"))?;
        anyhow::ensure!(leases.pruning.is_none(),
            "Vector history maintenance is active; retry the read");
        leases.readers.retain(|_, weak| weak.strong_count() > 0);
        let lease = if let Some(lease) = leases.readers.get(&key).and_then(Weak::upgrade) {
            lease
        } else {
            let lease = Arc::new(PhysicalLease);
            leases.readers.insert(key, Arc::downgrade(&lease));
            lease
        };
        Ok(Arc::new(Epoch {
            record: data.record.clone(),
            table: data.table.clone(),
            query_embedder: data.query_embedder.clone(),
            _lease: lease,
            _catalog: catalog,
        }))
    }

    pub fn pin(
        &self,
        project: &str,
        provider: &Arc<dyn engram_ml::Embedder>,
        catalog: Arc<snapshots::Catalog>,
    ) -> anyhow::Result<Arc<Epoch>> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector route lock poisoned"))?;
        // Route lookup and physical pin must be atomic with publication. An
        // unpinned old data view cannot escape this lock and race retirement.
        let data = self.data_locked(&mut current, project, provider)?;
        self.reader(data, catalog)
    }

    fn has_readers(&self, owner: &OwnedTable) -> anyhow::Result<bool> {
        let _current = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector route lock poisoned"))?;
        let leases = self
            .leases
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector lease registry poisoned"))?;
        anyhow::ensure!(!leases.unproven, "Native read quiescence is unproven; retired table preserved");
        Ok(leases
            .readers.get(&owner.lease_key())
            .is_some_and(|lease| lease.strong_count() > 0))
    }

    pub(super) fn mark_read_unproven(&self) {
        match self.leases.lock() {
            Ok(mut state) => state.unproven = true,
            Err(poisoned) => poisoned.into_inner().unproven = true,
        }
    }

    // One Session spans this engine root. Gate every physical Epoch,
    // including peers/retired epochs, before optional cache-owner replacement.
    pub(super) fn try_prune(self: &Arc<Self>, owner: &OwnedTable) -> anyhow::Result<PruneLease> {
        let _current = self.current.try_lock().map_err(|_| anyhow::anyhow!("Vector route is busy or unavailable"))?;
        let mut leases = self.leases.try_lock().map_err(|_| anyhow::anyhow!("Vector reader registry is busy or unavailable"))?;
        anyhow::ensure!(!leases.unproven, "Native read quiescence is unproven; history preserved until runtime closes");
        anyhow::ensure!(leases.pruning.is_none(), "Vector history maintenance is already active");
        let key = owner.lease_key();
        anyhow::ensure!(!leases.readers.values().any(|lease| lease.strong_count() > 0),
            "Native readers still hold a physical epoch in this cache owner");
        leases.pruning = Some(key.clone());
        Ok(PruneLease { routes: self.clone(), key })
    }

    fn receipt_path(&self, owner: &OwnedTable) -> anyhow::Result<PathBuf> {
        owner.validate(&owner.project)?;
        Ok(self.root.join("owned").join(format!(
            "{}.json",
            blake3::hash(owner.lease_key().as_bytes()).to_hex()
        )))
    }

    pub fn write_owned(&self, owner: &OwnedTable) -> anyhow::Result<()> {
        let path = self.receipt_path(owner)?;
        std::fs::create_dir_all(path.parent().expect("owned receipt has a parent"))?;
        let mut file = tempfile::NamedTempFile::new_in(path.parent().expect("checked above"))?;
        file.write_all(&serde_json::to_vec(owner)?)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(path).map_err(|e| e.error)?;
        Ok(())
    }

    pub fn read_owned(&self, owner: &OwnedTable) -> anyhow::Result<()> {
        let file = self
            .open_metadata(&self.receipt_path(owner)?)?
            .ok_or_else(|| anyhow::anyhow!("VEC1: owned vector receipt is missing"))?;
        let mut bytes = Vec::new();
        file.take(8193).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 8192, "VEC1: excessive owned vector receipt");
        let actual: OwnedTable = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            &actual == owner,
            "VEC1: vector ownership receipt mismatch; data preserved"
        );
        Ok(())
    }

    pub fn forget_owned(&self, owner: &OwnedTable) -> anyhow::Result<()> {
        let path = self.receipt_path(owner)?;
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
            Ok(_) => self.read_owned(owner)?,
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// Only unwind this operation's persisted bootstrap intent when native
    /// creation has ended and the exact table is known absent. Unknown state,
    /// changed intent/ownership, or any active route keeps the evidence.
    fn discard_absent_bootstrap(
        &self,
        owner: &OwnedTable,
        record: &Record,
        pending: &Path,
    ) -> anyhow::Result<bool> {
        owner.validate(&record.project)?;
        anyhow::ensure!(
            record.table == owner.table
                && record.creation_token == owner.creation_token
                && record.model_tag == owner.model_tag
                && record.dimension == owner.dimension
                && record.baseline_rows == 0
                && record.baseline_digest.is_empty()
                && record.retired.is_empty()
                && pending == self.path(&owner.project)?.with_extension("pending.json"),
            "VEC1: cleanup does not match this fresh bootstrap intent; evidence retained"
        );
        if self.checked_table_location(&owner.table)? {
            return Ok(false);
        }
        match std::fs::symlink_metadata(self.path(&owner.project)?) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => return Ok(false),
        }
        self.read_owned(owner)?;
        let expected = serde_json::to_vec(record)?;
        anyhow::ensure!(
            expected.len() <= 64 * 1024,
            "VEC1: bootstrap intent exceeds cleanup verification bound"
        );
        let mut observed = Vec::new();
        self.open_metadata(pending)?
            .ok_or_else(|| anyhow::anyhow!("VEC1: bootstrap intent is missing; cleanup refused"))?
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut observed)?;
        anyhow::ensure!(
            observed == expected,
            "VEC1: bootstrap intent changed; evidence retained"
        );
        std::fs::remove_file(pending)?;
        self.forget_owned(owner)?;
        Ok(true)
    }

    /// Only a specific local not-found outcome counts as absence. Canonicalize
    /// the actual table, so a junction cannot redirect collection elsewhere.
    pub(super) fn checked_table_location(&self, table: &str) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !table.is_empty()
                && table
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "Invalid vector table name"
        );
        let path = self.lance_root.join(format!("{table}.lance"));
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "VEC1: unexpected vector table location; data preserved"
            ),
        }
        anyhow::ensure!(
            path.canonicalize()? == path,
            "VEC1: vector table resolves outside its recorded location; data preserved"
        );
        Ok(true)
    }

    pub async fn lexical_only(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.bootstrap.clone().lock_owned().await
    }

    pub fn try_lexical_only(&self) -> anyhow::Result<tokio::sync::OwnedMutexGuard<()>> {
        self.bootstrap.clone().try_lock_owned().map_err(|_| {
            anyhow::anyhow!(
                "Vector bootstrap or a lexical-only writer is active; retry bulk ingestion"
            )
        })
    }

    #[cfg(test)]
    pub fn fail_next_activation(&self) {
        self.fail_activation
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn validate_bulk_write(
        &self,
        project: &str,
        provider: &Arc<dyn engram_ml::Embedder>,
    ) -> anyhow::Result<()> {
        // Validation must not install the persistent bulk embedder as a query
        // provider. Query pins alone attach their bounded, checked provider.
        let cached = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector route lock poisoned"))?
            .get(project)
            .and_then(|epoch| epoch.record.clone());
        let record = match cached {
            Some(record) => Some(record),
            None => self.read(project)?,
        }.ok_or_else(|| anyhow::anyhow!("VEC1: initialize a vector store before adding lexical documents through a bulk writer"))?;
        anyhow::ensure!(
            record.model_tag == provider.model_tag()
                && record.dimension == provider.dimension()
                && self
                    .lance_root
                    .join(format!("{}.lance", record.table))
                    .is_dir(),
            "VEC1: bulk writer embedding store is unavailable or mismatched"
        );
        Ok(())
    }

    pub fn new(root: PathBuf, lance_root: PathBuf) -> anyhow::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_SHARE_READ | FILE_SHARE_WRITE; exclude FILE_SHARE_DELETE so
            // cleanup cannot unlink a live lock and replace its file identity.
            options.share_mode(0x00000001 | 0x00000002);
        }
        let process_lock = options.open(lance_root.join(".engram-vector-owner.lock"))?;
        fs2::FileExt::try_lock_exclusive(&process_lock).map_err(|e| {
            anyhow::anyhow!(
                "Vector directory is already bound to another search index or process: {e}"
            )
        })?;
        Ok(Self {
            root,
            lance_root,
            current: Mutex::new(HashMap::new()),
            leases: Mutex::new(LeaseState::default()),
            read_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            bootstrap: Arc::new(tokio::sync::Mutex::new(())),
            publication: Mutex::new(None),
            _process_lock: process_lock,
            #[cfg(test)]
            fail_activation: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn check_root(&self, root: &Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            root == self.lance_root,
            "A search index cannot bind to different vector directories"
        );
        Ok(())
    }

    fn path(&self, project: &str) -> anyhow::Result<PathBuf> {
        check_project(project)?;
        Ok(self.root.join(format!(
            "{}.json",
            blake3::hash(project.as_bytes()).to_hex()
        )))
    }

    fn open_metadata(&self, path: &Path) -> anyhow::Result<Option<std::fs::File>> {
        match std::fs::symlink_metadata(&self.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "VEC1: vector metadata root is not a regular directory"
            ),
        }
        anyhow::ensure!(
            self.root.canonicalize()? == self.root,
            "VEC1: vector metadata root was redirected; data preserved"
        );
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| anyhow::anyhow!("VEC1: vector metadata path is outside its root"))?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("VEC1: unsupported vector metadata path"))?
            .replace('\\', "/");
        let checked = engram_core::safe_join(&self.root, &relative)?;
        match std::fs::symlink_metadata(&checked) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(metadata) => anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "VEC1: vector metadata must be a regular file"
            ),
        }
        let file = engram_core::safe_open_read(&self.root, &relative)?;
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "VEC1: vector metadata handle is not a regular file"
        );
        Ok(Some(file))
    }

    pub(super) fn read(&self, project: &str) -> anyhow::Result<Option<Record>> {
        let file = match self.open_metadata(&self.path(project)?)? {
            Some(file) => file,
            None => return Ok(None),
        };
        let mut bytes = Vec::new();
        file.take(65_537).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= 65_536,
            "Vector route exceeds its metadata budget"
        );
        let record: Record = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            record.version == 1 && record.project == project && record.dimension > 0,
            "Invalid vector route identity"
        );
        record.ownership().validate(project)?;
        for retired in &record.retired {
            retired.validate(project)?;
        }
        let mut identities = std::collections::HashSet::new();
        anyhow::ensure!(
            record.retired.len() <= 64
                && record
                    .retired
                    .iter()
                    .all(|r| r.table != record.table && identities.insert(r.lease_key())),
            "Invalid or excessive vector route metadata"
        );
        Ok(Some(record))
    }

    fn data_locked(
        &self,
        current: &mut HashMap<String, Arc<EpochData>>,
        project: &str,
        provider: &Arc<dyn engram_ml::Embedder>,
    ) -> anyhow::Result<Arc<EpochData>> {
        if let Some(epoch) = current.get(project) {
            anyhow::ensure!(
                epoch.record.is_some()
                    || !self.path(project)?.with_extension("pending.json").exists(),
                "VEC1: vector bootstrap is pending; explicit recovery is required"
            );
            // A matching engine can attach its provider after a mismatched
            // engine loaded the route, and can use rotated credentials.
            if epoch.record.as_ref().is_some_and(|r| {
                r.model_tag == provider.model_tag() && r.dimension == provider.dimension()
            }) {
                let replacement = Arc::new(EpochData {
                    record: epoch.record.clone(),
                    table: epoch.table.clone(),
                    query_embedder: Some(provider.clone()),
                });
                current.insert(project.to_owned(), replacement.clone());
                return Ok(replacement);
            }
            return Ok(epoch.clone());
        }
        let record = self.read(project)?;
        anyhow::ensure!(
            record.is_some() || !self.path(project)?.with_extension("pending.json").exists(),
            "VEC1: vector bootstrap is pending; explicit recovery is required"
        );
        let table = record
            .as_ref()
            .map(|r| r.table.clone())
            .unwrap_or_else(|| canonical_table(project));
        let query_embedder = match &record {
            Some(record)
                if record.model_tag == provider.model_tag()
                    && record.dimension == provider.dimension() =>
            {
                Some(provider.clone())
            }
            _ => None, // Unknown legacy space must not be guessed from dimension.
        };
        let epoch = Arc::new(EpochData {
            record,
            table,
            query_embedder,
        });
        current.insert(project.to_owned(), epoch.clone());
        Ok(epoch)
    }

    /// No await between durable publication and in-memory activation. The caller
    /// holds exclusive recovery admission; failure leaves it uncertain.
    pub fn publish(
        &self,
        record: Record,
        provider: Arc<dyn engram_ml::Embedder>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            record.model_tag == provider.model_tag() && record.dimension == provider.dimension(),
            "Cannot activate a vector route with another embedding space"
        );
        self.read_owned(&record.ownership())?;
        let destination = self.path(&record.project)?;
        let mut current = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector route lock poisoned"))?;
        std::fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec(&record)?;
        anyhow::ensure!(
            bytes.len() <= 65_536 && record.retired.len() <= 64,
            "Vector route metadata budget exhausted; collect retired epochs before recovery"
        );
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(destination).map_err(|e| e.error)?;
        #[cfg(test)]
        if self
            .fail_activation
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("Injected failure after durable vector publication");
        }
        let epoch = Arc::new(EpochData {
            table: record.table.clone(),
            record: Some(record.clone()),
            query_embedder: Some(provider),
        });
        current.insert(record.project, epoch.clone());
        Ok(())
    }

    /// An error from publication is ambiguous until both the durable route and
    /// its cached view exclude this attempt's target. Any inability to prove
    /// that exclusion preserves the table. The caller still holds exclusive
    /// recovery admission through the subsequent token-checked native cleanup.
    pub fn unpublished_after_failure(&self, owner: &OwnedTable) -> anyhow::Result<bool> {
        let current = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector route lock poisoned"))?;
        let references = |record: &Record| {
            record.table == owner.table
                || record
                    .retired
                    .iter()
                    .any(|retired| retired.table == owner.table)
        };
        let durable = self.read(&owner.project)?;
        Ok(!durable.as_ref().is_some_and(&references)
            && !current.get(&owner.project).is_some_and(|view| {
                view.table == owner.table || view.record.as_ref().is_some_and(references)
            }))
    }

    fn remove_retired(&self, project: &str, owner: &OwnedTable) -> anyhow::Result<()> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector route lock poisoned"))?;
        let mut record = self
            .read(project)?
            .ok_or_else(|| anyhow::anyhow!("No managed vector route for collection"))?;
        anyhow::ensure!(
            record.table != owner.table,
            "Cannot retire the active vector table"
        );
        if let Some(cached) = current.get(project).and_then(|data| data.record.as_ref()) {
            anyhow::ensure!(
                cached.ownership() == record.ownership(),
                "Active vector routing requires validation before collection"
            );
        }
        record.retired.retain(|retired| retired != owner);
        let bytes = serde_json::to_vec(&record)?;
        anyhow::ensure!(
            bytes.len() <= 65_536,
            "Vector route metadata budget exhausted"
        );
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.path(project)?).map_err(|e| e.error)?;
        // Both durable outcomes have the same active table/model. A failed
        // retirement-only update cannot reopen the old epoch for new readers.
        let provider = current
            .get(project)
            .and_then(|data| data.query_embedder.clone());
        current.insert(
            project.to_owned(),
            Arc::new(EpochData {
                table: record.table.clone(),
                record: Some(record),
                query_embedder: provider,
            }),
        );
        Ok(())
    }

    pub fn reserve_recovery(
        &self,
        coordinator: &Arc<mutation::Coordinator>,
        project: &str,
    ) -> anyhow::Result<mutation::RecoveryPermit> {
        match coordinator.try_recovery() {
            Ok(permit) => Ok(permit),
            Err(error) => {
                let pending = self
                    .publication
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Vector publication lock poisoned"))?;
                if pending.as_deref() == Some(project) {
                    Ok(coordinator.try_resume_uncertain()?)
                } else {
                    Err(error.into())
                }
            }
        }
    }

    pub fn mark_publication(
        &self,
        project: &str,
        permit: &mut mutation::RecoveryPermit,
    ) -> anyhow::Result<()> {
        let mut pending = self
            .publication
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector publication lock poisoned"))?;
        anyhow::ensure!(
            pending.as_deref().is_none_or(|p| p == project),
            "Another project's vector publication requires recovery"
        );
        permit.mark_publication_possible()?;
        *pending = Some(project.to_owned());
        Ok(())
    }

    pub fn finish_activation(&self, permit: mutation::RecoveryPermit) -> anyhow::Result<()> {
        let mut pending = self
            .publication
            .lock()
            .map_err(|_| anyhow::anyhow!("Vector publication lock poisoned"))?;
        permit.finish_activation()?;
        *pending = None;
        Ok(())
    }

    pub fn pin_recovery(
        &self,
        project: &str,
        provider: &Arc<dyn engram_ml::Embedder>,
        catalog: Arc<snapshots::Catalog>,
    ) -> anyhow::Result<Arc<Epoch>> {
        // Read authoritative durable routing after an uncertain activation,
        // rather than trusting a cached route from before publication.
        let record = self.read(project)?;
        let data = Arc::new(EpochData {
            table: record
                .as_ref()
                .map(|r| r.table.clone())
                .unwrap_or_else(|| canonical_table(project)),
            query_embedder: record
                .as_ref()
                .filter(|r| {
                    r.model_tag == provider.model_tag() && r.dimension == provider.dimension()
                })
                .map(|_| provider.clone()),
            record,
        });
        self.reader(data, catalog)
    }
}

impl HybridSearchEngine {
    /// Collect only recorded, unleased retired tables. The worker owns exclusion
    /// until native deletion settles even if its requesting future is dropped.
    pub async fn collect_retired_vectors(
        &self,
        project: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<usize> {
        let worker = self.clone();
        let project = project.to_owned();
        let cancel = cancel.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        tokio::spawn(async move {
            worker
                .collect_retired_vectors_inner(&project, &cancel)
                .await
        })
        .await
        .map_err(|e| anyhow::anyhow!("Vector collection worker did not complete: {e}"))?
    }

    pub(super) async fn collect_retired_vectors_inner(
        &self,
        project: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<usize> {
        let permit = self.snapshots.mutations.try_recovery()?;
        let result = async {
            let Some(record) = self.vector_routes.read(project)? else {
                return Ok(0);
            };
            let mut collected = 0;
            let mut deferred = 0;
            let mut first_error = None;
            for owner in &record.retired {
                anyhow::ensure!(!cancel.is_cancelled(), "Vector collection cancelled");
                if self.vector_routes.has_readers(owner)? {
                    continue;
                }
                if let Err(error) = self.drop_owned_vector_table(owner).await {
                    tracing::warn!(project_id = %project, table = %owner.table,
                        "Retired vector table preserved; cleanup deferred: {error:#}");
                    deferred += 1;
                    first_error.get_or_insert(error);
                    continue;
                }
                self.vector_routes.remove_retired(project, owner)?;
                collected += 1;
            }
            if let Some(first_error) = first_error {
                return Err(RetiredCleanupDeferred {
                    deferred,
                    collected,
                    first_error,
                }
                .into());
            }
            Ok(collected)
        }
        .await;
        // This operation changes retirement metadata only, never the active
        // route. Errors preserve receipts/retry state and do not disable writes.
        permit.finish_activation()?;
        result
    }

    pub(super) async fn open_vector_epoch_table(
        &self,
        epoch: &Epoch,
    ) -> anyhow::Result<lancedb::Table> {
        anyhow::ensure!(
            self.vector_routes.checked_table_location(&epoch.table)?,
            "VEC1: managed vector table is missing; complete recovery is required"
        );
        let table = self.native_store.capture()?.connection.open_table(&epoch.table).execute().await?;
        if let Some(record) = &epoch.record {
            let owner = record.ownership();
            self.vector_routes.read_owned(&owner)?;
            owner.validate_table(&table).await?;
        }
        Ok(table)
    }

    pub(super) async fn drop_owned_vector_table(&self, owner: &OwnedTable) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self
                .vector_routes
                .read(&owner.project)?
                .as_ref()
                .is_some_and(|r| r.table == owner.table),
            "Cannot collect an active vector table"
        );
        anyhow::ensure!(
            !self.vector_routes.has_readers(owner)?,
            "Cannot collect a vector table while readers hold it"
        );
        if !self.vector_routes.checked_table_location(&owner.table)? {
            self.vector_routes.forget_owned(owner)?;
            return Ok(());
        }
        self.vector_routes.read_owned(owner)?;
        let table = self.native_store.capture()?.connection.open_table(&owner.table).execute().await?;
        owner.validate_table(&table).await?;
        drop(table);
        self.native_store.capture()?.connection.drop_table(&owner.table, &[]).await?;
        anyhow::ensure!(
            !self.vector_routes.checked_table_location(&owner.table)?,
            "Owned vector deletion left residues; receipt retained"
        );
        self.vector_routes.forget_owned(owner)?;
        Ok(())
    }

    pub(super) fn ensure_managed_vector_table_present(&self, epoch: &Epoch) -> anyhow::Result<()> {
        anyhow::ensure!(
            epoch.record.is_none()
                || self
                    ._lance_dir
                    .join(format!("{}.lance", epoch.table))
                    .is_dir(),
            "VEC1: managed vector table is missing; complete recovery is required"
        );
        Ok(())
    }

    pub(super) fn pin_vector_epoch(&self, project: &str) -> anyhow::Result<Arc<Epoch>> {
        self.vector_routes
            .pin(project, &self.query_embedder, self.snapshots.clone())
    }

    /// Ordinary writers can create a brand-new store, but cannot silently
    /// recreate a lost managed table or adopt an unidentified legacy store.
    pub(super) async fn vector_write_table(&self, project: &str) -> anyhow::Result<lancedb::Table> {
        let epoch = self.pin_vector_epoch(project)?;
        if let Some(record) = &epoch.record {
            anyhow::ensure!(
                record.model_tag == self.embedder.model_tag()
                    && record.dimension == self.embedder.dimension(),
                "VEC1: embedding configuration changed; recover all retained vectors before writing"
            );
            anyhow::ensure!(
                self._lance_dir
                    .join(format!("{}.lance", epoch.table))
                    .exists(),
                "VEC1: managed vector table is missing; recover all retained vectors before writing"
            );
            return self.open_vector_epoch_table(&epoch).await;
        }
        anyhow::ensure!(
            !self
                ._lance_dir
                .join(format!("{}.lance", epoch.table))
                .exists(),
            "VEC1: existing vector table has no embedding identity; explicit recovery is required"
        );
        anyhow::ensure!(
            self.count_docs(project)? == 0,
            "VEC1: stored documents exist without an identified vector store; recover all namespaces before writing"
        );
        // Wait cancellably before starting native work. An older lexical-only
        // writer owns this mutex and must finish without a pending operation
        // blocking its own already-admitted ingestion steps.
        let bootstrap = self.vector_routes.bootstrap.clone().lock_owned().await;
        let worker = self.clone();
        let project = project.to_owned();
        self.run_native_vector_mutation(async move {
            worker.bootstrap_vector_table(&project, bootstrap).await
        })
        .await
    }

    async fn bootstrap_vector_table(
        &self,
        project: &str,
        _bootstrap: tokio::sync::OwnedMutexGuard<()>,
    ) -> anyhow::Result<lancedb::Table> {
        // The worker owns both admission and this mutex through native create,
        // validation and activation, including when its requester disappears.
        let epoch = self.pin_vector_epoch(project)?;
        if let Some(record) = &epoch.record {
            anyhow::ensure!(
                record.model_tag == self.embedder.model_tag(),
                "VEC1: embedding identity changed during bootstrap"
            );
            self.ensure_managed_vector_table_present(&epoch)?;
            return self.open_vector_epoch_table(&epoch).await;
        }
        anyhow::ensure!(
            self.count_docs(project)? == 0
                && !self
                    ._lance_dir
                    .join(format!("{}.lance", epoch.table))
                    .exists(),
            "VEC1: existing source or vectors require explicit recovery"
        );
        let owner = OwnedTable::new(project, &epoch.table, &self.embedder)?;
        let record = Record {
            version: 1,
            project: project.to_owned(),
            table: epoch.table.clone(),
            model_tag: self.embedder.model_tag(),
            dimension: self.embedder.dimension(),
            creation_token: owner.creation_token.clone(),
            baseline_rows: 0,
            baseline_digest: String::new(),
            retired: Vec::new(),
        };
        let pending = self
            .vector_routes
            .path(project)?
            .with_extension("pending.json");
        std::fs::create_dir_all(&self.vector_routes.root)?;
        self.vector_routes.write_owned(&owner)?;
        let mut receipt = tempfile::NamedTempFile::new_in(&self.vector_routes.root)?;
        receipt.write_all(&serde_json::to_vec(&record)?)?;
        receipt.as_file().sync_all()?;
        receipt.persist_noclobber(&pending).map_err(|e| e.error)?;
        // Pending ownership survives cancellation, but no ready route points to
        // an absent table. Creation is exclusive and never adopts legacy data.
        let created = self.create_bootstrap_table(&owner).await;
        let table = match created {
            Ok(table) => table,
            Err(error) => {
                if let Err(cleanup_error) = self
                    .vector_routes
                    .discard_absent_bootstrap(&owner, &record, &pending)
                {
                    tracing::warn!(project, error = %cleanup_error, "Vector bootstrap cleanup incomplete; retained evidence requires inspection");
                }
                return Err(error);
            }
        };
        self.vector_routes
            .publish(record, self.query_embedder.clone())?;
        std::fs::remove_file(pending)?;
        Ok(table)
    }

    async fn create_bootstrap_table(&self, owner: &OwnedTable) -> anyhow::Result<lancedb::Table> {
        #[cfg(test)]
        let failure = self
            .bootstrap_failure
            .as_ref()
            .map_or(0, |flag| flag.swap(0, std::sync::atomic::Ordering::SeqCst));
        #[cfg(test)]
        if failure == 1 {
            anyhow::bail!("controlled vector table creation I/O failure");
        }
        let table = crate::vector::create_owned_table(
            &self.native_store.capture()?.connection,
            &owner.table,
            self.embedder.dimension(),
            owner.metadata(),
        )
        .await?;
        #[cfg(test)]
        if failure == 2 {
            anyhow::bail!("controlled vector table validation I/O failure");
        }
        owner.validate_table(&table).await?;
        Ok(table)
    }
}

#[cfg(test)]
mod bootstrap_failure_tests {
    use super::*;

    async fn engine(root: &Path, failure: u8) -> HybridSearchEngine {
        let mut engine = HybridSearchEngine::new(
            root.join("tantivy"),
            root.join("lance"),
            &engram_core::Config {
                embedding_backend: "local".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        engine.bootstrap_failure = Some(Arc::new(std::sync::atomic::AtomicU8::new(failure)));
        engine
    }

    #[tokio::test]
    async fn transient_create_failure_before_any_table_allows_ordinary_write_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let index = engine(temporary.path(), 1).await;
        let first = index.vector_write_table("project").await;
        let first_failed = first.is_err();
        drop(first);
        let pending = index
            .vector_routes
            .path("project")
            .unwrap()
            .with_extension("pending.json");
        let intent_removed = !pending.exists();
        let receipts = index.vector_routes.root.join("owned");
        let no_receipts = std::fs::read_dir(receipts).unwrap().next().is_none();
        let doc = IndexDoc {
            generation: 1,
            chunk_id: 1,
            path: RelPath::new("source.rs"),
            language: "rust".into(),
            content: "bootstrap retry source".into(),
            namespace: "memory".into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 1,
            doc_id: "entry".into(),
            content_hash: ContentHash::compute(b"bootstrap retry source").0,
        };
        let retry = index
            .index_docs("project", &[doc], &CancellationToken::new())
            .await;
        let source_ready = index.count_docs("project").unwrap_or(0) == 1;
        let rows = match index.vector_write_table("project").await {
            Ok(table) => table.count_rows(None).await.unwrap_or(0),
            Err(_) => 0,
        };
        let retry_succeeded = retry.is_ok();
        drop(retry);
        drop(index);
        temporary.close().unwrap();
        assert!(
            first_failed && intent_removed && no_receipts,
            "Known-absent failed bootstrap must not strand a pending route or ownership receipt"
        );
        assert!(
            retry_succeeded && source_ready && rows == 1,
            "An ordinary retry must store source and its vector without operator recovery"
        );
    }

    #[tokio::test]
    async fn validation_failure_with_existing_table_preserves_owned_pending_evidence() {
        let temporary = tempfile::tempdir().unwrap();
        let index = engine(temporary.path(), 2).await;
        let first = index.vector_write_table("project").await;
        let failed = first.is_err();
        drop(first);
        let pending = index
            .vector_routes
            .path("project")
            .unwrap()
            .with_extension("pending.json");
        let bytes = std::fs::read(&pending).unwrap();
        let record: Record = serde_json::from_slice(&bytes).unwrap();
        let owner = OwnedTable {
            version: 1,
            project: record.project.clone(),
            table: record.table.clone(),
            creation_token: record.creation_token.clone(),
            model_tag: record.model_tag.clone(),
            dimension: record.dimension,
        };
        let preserved = index
            .vector_routes
            .checked_table_location(&owner.table)
            .unwrap()
            && index.vector_routes.read_owned(&owner).is_ok();
        let retry_refused = index.vector_write_table("project").await.is_err();
        let unchanged = std::fs::read(&pending).unwrap() == bytes;
        let no_source_written = index.count_docs("project").unwrap_or(usize::MAX) == 0;
        drop(index);
        temporary.close().unwrap();
        assert!(
            failed && preserved && unchanged,
            "A possible native table must retain its exact pending intent and ownership"
        );
        assert!(
            retry_refused && no_source_written,
            "Ambiguous bootstrap must not be silently adopted or reported ready"
        );
    }
}
