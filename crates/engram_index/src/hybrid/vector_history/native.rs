use super::*;
use lance_table::io::commit::{ManifestLocation, ManifestNamingScheme};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

mod structural;
#[cfg(test)]
mod tests;

#[cfg(test)]
std::thread_local! {
    static NATIVE_OPEN_PROBE: std::cell::RefCell<Option<Arc<std::sync::atomic::AtomicUsize>>> = const { std::cell::RefCell::new(None) };
    static PATH_BYTE_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_RETAINED_PATH_BYTES) };
}

const MAX_ENTRIES: usize = 16_384;
const MAX_MANIFESTS: usize = 4096;
const MAX_MANIFEST_BYTES: u64 = 512 * 1024;
const MAX_METADATA_BYTES: u64 = 32 * 1024 * 1024;
const MAX_REFERENCE_WORK: usize = 1_000_000;
const ALLOCATION_ALLOWANCE: u64 = 64 * 1024 * 1024;
const MAX_RETAINED_PATH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct PathBytes(usize);

impl PathBytes {
    fn charge(&mut self, bytes: usize) -> anyhow::Result<()> {
        let total = self
            .0
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("Native history path accounting overflow"))?;
        #[cfg(test)]
        let limit = PATH_BYTE_LIMIT.with(|limit| limit.get());
        #[cfg(not(test))]
        let limit = MAX_RETAINED_PATH_BYTES;
        anyhow::ensure!(
            total <= limit,
            "Native history retained path byte budget exceeded; history preserved"
        );
        self.0 = total;
        Ok(())
    }
}

fn reference<'a>(
    files: &'a BTreeMap<String, u64>,
    expected: &mut BTreeSet<&'a str>,
    retained: &mut PathBytes,
    path: &str,
) -> anyhow::Result<()> {
    let (canonical, _) = files.get_key_value(path).ok_or_else(|| {
        anyhow::anyhow!("Referenced native file is missing; whole history preserved")
    })?;
    if !expected.contains(canonical.as_str()) {
        anyhow::ensure!(
            expected.len() < MAX_ENTRIES,
            "Native reference set exceeds its inspection budget"
        );
        retained.charge(canonical.len())?;
        expected.insert(canonical.as_str());
    }
    Ok(())
}

struct Budget<'a> {
    cancel: &'a CancellationToken,
    started: Instant,
    remaining: usize,
}

struct PanicFence(Arc<mutation::Coordinator>);

impl Drop for PanicFence {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.mark_faulted();
        }
    }
}

impl<'a> Budget<'a> {
    fn new(cancel: &'a CancellationToken) -> Self {
        Self {
            cancel,
            started: Instant::now(),
            remaining: MAX_REFERENCE_WORK,
        }
    }

    fn charge(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.cancel.is_cancelled(),
            "History preflight cancelled; files preserved"
        );
        anyhow::ensure!(
            self.started.elapsed() < Duration::from_secs(30),
            "History preflight deadline exceeded; files preserved"
        );
        self.remaining = self.remaining.checked_sub(1).ok_or_else(|| {
            anyhow::anyhow!("History preflight work budget exceeded; files preserved")
        })?;
        Ok(())
    }
}

fn ordinary(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    !metadata.file_type().is_symlink()
}

fn leaf(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        && value != "."
        && value != ".."
}

fn inventory(root: &Path, budget: &mut Budget<'_>) -> anyhow::Result<BTreeMap<String, u64>> {
    let mut files = BTreeMap::new();
    let mut retained = PathBytes::default();
    retained.charge(root.as_os_str().as_encoded_bytes().len())?;
    let mut pending = vec![root.to_path_buf()];
    let mut entries = 0;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            budget.charge()?;
            entries += 1;
            anyhow::ensure!(
                entries <= MAX_ENTRIES,
                "History preflight exceeds 16384 entries; files preserved"
            );
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(
                ordinary(&metadata),
                "History contains a redirected path; files preserved"
            );
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Unsupported native history filename"))?;
            anyhow::ensure!(
                relative.len() <= 4096,
                "Native history path exceeds its inspection bound"
            );
            let first = relative.split(['/', '\\']).next().unwrap_or("");
            anyhow::ensure!(
                matches!(
                    first,
                    "_versions" | "_transactions" | "data" | "_deletions" | "_indices" | "_refs"
                ),
                "Unknown native table entry; history preserved: {relative}"
            );
            if metadata.is_dir() {
                retained.charge(path.as_os_str().as_encoded_bytes().len())?;
                pending.push(path);
            } else {
                anyhow::ensure!(
                    metadata.is_file(),
                    "Unsupported native history entry; files preserved"
                );
                anyhow::ensure!(
                    first != "_refs",
                    "Native tags or branches exist; history preserved"
                );
                anyhow::ensure!(
                    first != "_indices",
                    "Native index payloads require separate validated maintenance; files preserved"
                );
                retained.charge(relative.len())?;
                files.insert(relative.replace('\\', "/"), metadata.len());
            }
        }
    }
    Ok(files)
}

impl HybridSearchEngine {
    pub(in super::super) async fn prune_vector_history_inner(
        &self,
        project: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<NativeHistoryPruneOutcome> {
        let deferred = |error: anyhow::Error| NativeHistoryPruneOutcome::Deferred {
            reason: format!("{error:#}"),
        };
        let _exclusive = match self.snapshots.mutations.try_recovery() {
            Ok(permit) => permit,
            Err(error) => return Ok(deferred(error.into())),
        };
        let _panic_fence = PanicFence(self.snapshots.mutations.clone());
        let record = match self.vector_routes.read(project) {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(NativeHistoryPruneOutcome::Unavailable),
            Err(error) => return Ok(deferred(error)),
        };
        let owner = record.ownership();
        let _prune = match self.vector_routes.try_prune(&owner) {
            Ok(lease) => lease,
            Err(error) => return Ok(deferred(error)),
        };
        let mut budget = Budget::new(cancel);
        #[cfg(test)]
        if let Some(limit) = self.history_preflight_work_limit {
            budget.remaining = limit;
        }
        // Reserve before allocating inventories or dependency-decoded metadata.
        // Inventory/queued paths retain <=8MiB total, reference keys are borrowed
        // (also byte-capped), and at most one <=512KiB raw manifest is read at a
        // time. The structural guard bounds codec Vec/map/scalar expansion before
        // decode (including implicit latest decode on table-open). This logical
        // allowance includes collection/codec headroom; it is not measured RSS.
        let _allocation = match self
            .memory_budget
            .as_ref()
            .map(|memory| {
                AllocationGuard::try_new(
                    memory,
                    ALLOCATION_ALLOWANCE,
                    Subsystem::LanceDb,
                    "native history metadata preflight",
                )
            })
            .transpose()
        {
            Ok(guard) => guard,
            Err(error) => return Ok(deferred(error.into())),
        };
        let preflight = async {
            budget.charge()?;
            self.native_store.ensure_no_failed_candidate()?;
            self.vector_routes.read_owned(&owner)?;
            anyhow::ensure!(self.vector_routes.checked_table_location(&owner.table)?, "Owned native table is missing; history preserved");
            let root = self._lance_dir.join(format!("{}.lance", owner.table));
            let files = inventory(&root, &mut budget)?;
            let manifests: Vec<_> = files.iter().filter(|(name, _)| name.starts_with("_versions/")).collect();
            anyhow::ensure!(!manifests.is_empty() && manifests.len() <= MAX_MANIFESTS,
                "Native history requires 1..4096 validated manifests; files preserved");
            let metadata_bytes = manifests.iter().try_fold(0u64, |total, (_, bytes)| {
                anyhow::ensure!(**bytes <= MAX_MANIFEST_BYTES, "Native manifest exceeds 512KiB; history preserved");
                total.checked_add(**bytes).ok_or_else(|| anyhow::anyhow!("Native metadata size overflow"))
            })?;
            anyhow::ensure!(metadata_bytes <= MAX_METADATA_BYTES, "Native history metadata exceeds 32MiB; files preserved");
            // Do not open even the latest native Dataset until every physical
            // manifest has passed allocation-free structural admission. Neither
            // the Session cache nor the semantic decoder is a corruption gate.
            for (relative, bytes) in &manifests {
                budget.charge()?;
                let filename = relative.strip_prefix("_versions/").filter(|name| leaf(name))
                    .ok_or_else(|| anyhow::anyhow!("Unknown manifest layout; history preserved"))?;
                let scheme = ManifestNamingScheme::detect_scheme(filename)
                    .ok_or_else(|| anyhow::anyhow!("Unverified or temporary manifest; history preserved"))?;
                let version = scheme.parse_version(filename).filter(|version| *version > 0 && *version < (1u64 << 63))
                    .ok_or_else(|| anyhow::anyhow!("Detached or unsupported native version; history preserved"))?;
                // Compare the leaf without constructing/opening a Dataset.
                let canonical = match scheme {
                    ManifestNamingScheme::V1 => format!("{version}.manifest"),
                    ManifestNamingScheme::V2 => format!("{:020}.manifest", u64::MAX - version),
                };
                anyhow::ensure!(filename == canonical, "Noncanonical manifest filename; history preserved");
                structural::check_file(&root.join(relative), **bytes, &mut budget)?;
            }
            #[cfg(test)]
            if let Some(probe) = NATIVE_OPEN_PROBE.with(|probe| probe.borrow().clone()) {
                probe.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let table = self.native_store.capture()?.connection.open_table(&owner.table).execute().await?;
            owner.validate_table(&table).await?;
            let expected_schema = table.schema().await?;
            let wrapper = table.dataset().ok_or_else(|| anyhow::anyhow!("Native dataset inspection is unavailable"))?;
            let dataset = wrapper.get().await?;
            let latest = dataset.manifest().version;
            // Ordinary writes/explicit compaction migrate the known legacy
            // policy under their native commit authority. A read-only prune
            // preflight must never introduce a metadata write before the full
            // inventory is known safe, so enabled latest policy defers here.
            crate::vector::validate_history_config(&dataset.manifest().config, false)?;
            let mut expected = BTreeSet::new();
            let mut expected_bytes = PathBytes::default();
            let mut naming_scheme = None;
            let mut highest = 0;
            for (relative, bytes) in manifests {
                budget.charge()?;
                let filename = relative.strip_prefix("_versions/").filter(|name| leaf(name))
                    .ok_or_else(|| anyhow::anyhow!("Unknown manifest layout; history preserved"))?;
                let scheme = ManifestNamingScheme::detect_scheme(filename)
                    .ok_or_else(|| anyhow::anyhow!("Unverified or temporary manifest; history preserved"))?;
                anyhow::ensure!(naming_scheme.is_none_or(|old| old == scheme), "Mixed manifest naming schemes; history preserved");
                naming_scheme = Some(scheme);
                let version = scheme.parse_version(filename).filter(|version| *version > 0 && *version < (1u64 << 63))
                    .ok_or_else(|| anyhow::anyhow!("Detached or unsupported native version; history preserved"))?;
                let path = scheme.manifest_path(dataset.refs.base(), version);
                anyhow::ensure!(path.filename() == Some(filename), "Noncanonical manifest filename; history preserved");
                highest = highest.max(version);
                // Read physical metadata through the locked codec, bypassing
                // Session's cached manifests so malformed evidence is visible.
                let manifest = lance_table::io::manifest::read_manifest(&dataset.object_store, &path, Some(*bytes)).await?;
                let schema: arrow_schema::Schema = (&manifest.schema).into();
                anyhow::ensure!(manifest.version == version,
                    "Native manifest version differs from its filename; history preserved");
                anyhow::ensure!(manifest.branch.is_none() && manifest.tag.is_none(),
                    "Native manifest records a branch or tag; history preserved");
                anyhow::ensure!(manifest.base_paths.is_empty(),
                    "Native manifest references external base paths; history preserved");
                anyhow::ensure!(manifest.table_metadata.is_empty(),
                    "Native manifest has unsupported table metadata; history preserved");
                crate::vector::validate_history_config(&manifest.config, true)?;
                anyhow::ensure!(schema.fields() == expected_schema.fields()
                    && owner.metadata().iter().all(|(key, value)| schema.metadata().get(key) == Some(value)),
                    "Native manifest schema or ownership differs; history preserved");
                let location = ManifestLocation { version, path, size: Some(*bytes), naming_scheme: scheme, e_tag: None };
                let indices = lance_table::io::manifest::read_manifest_indexes(&dataset.object_store, &location, &manifest).await?;
                anyhow::ensure!(indices.is_empty(), "Native index metadata requires separate maintenance; history preserved");
                reference(&files, &mut expected, &mut expected_bytes, relative)?;
                if let Some(transaction) = &manifest.transaction_file {
                    anyhow::ensure!(leaf(transaction) && transaction.ends_with(".txn"), "Unsupported transaction path; history preserved");
                    reference(&files, &mut expected, &mut expected_bytes, &format!("_transactions/{transaction}"))?;
                }
                for fragment in manifest.fragments.iter() {
                    budget.charge()?;
                    anyhow::ensure!(!matches!(&fragment.row_id_meta, Some(lance_table::format::RowIdMeta::External(_))),
                        "External native row metadata requires separate maintenance; history preserved");
                    for file in &fragment.files {
                        budget.charge()?;
                        anyhow::ensure!(file.base_id.is_none() && leaf(&file.path) && file.path.ends_with(".lance"),
                            "Unsupported native data reference; history preserved");
                        reference(&files, &mut expected, &mut expected_bytes, &format!("data/{}", file.path))?;
                    }
                    if let Some(deletion) = &fragment.deletion_file {
                        reference(&files, &mut expected, &mut expected_bytes,
                            &lance_table::io::deletion::relative_deletion_file_path(fragment.id, deletion))?;
                    }
                    anyhow::ensure!(expected.len() <= MAX_ENTRIES, "Native reference set exceeds its inspection budget");
                }
            }
            anyhow::ensure!(highest == latest, "Native latest version differs from the complete inventory; history preserved");
            anyhow::ensure!(files.keys().all(|path| expected.contains(path.as_str())), "Unverified native file exists; whole history preserved");
            anyhow::ensure!(expected.iter().all(|path| files.contains_key(*path)), "Referenced native file is missing; whole history preserved");
            budget.charge()?;
            drop(dataset);
            // End the dataset read guard before the native pruning write lock.
            Ok::<_, anyhow::Error>((table, latest))
        }.await;
        let (table, latest) = match preflight {
            Ok(value) => value,
            Err(error) => return Ok(deferred(error)),
        };
        if let Err(error) = budget.charge() {
            return Ok(deferred(error));
        }
        table.checkout_latest().await?;
        self.vector_routes.read_owned(&owner)?;
        owner.validate_table(&table).await?;
        anyhow::ensure!(
            table.version().await? == latest,
            "Native version changed during exclusive preflight; pruning refused"
        );
        if let Err(error) = budget.charge() {
            return Ok(deferred(error));
        }
        #[cfg(test)]
        if let Some(pause) = &self.history_prune_pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
        // A started backend deletion is never raced against cancellation. The
        // worker keeps both barriers until completion, including ordinary errors.
        let stats = table.optimize(lancedb::table::OptimizeAction::Prune {
            older_than: Some(Default::default()),
            delete_unverified: Some(false),
            error_if_tagged_old_versions: Some(true),
        }).await.map_err(|error| anyhow::anyhow!("Native history pruning failed; old history may be partly removed, active source authority unchanged: {error:#}"))?
            .prune.ok_or_else(|| anyhow::anyhow!("Native pruning returned no outcome"))?;
        // The SDK retains version-keyed metadata after physical pruning.
        // Release every old Dataset/Table before considering a new cache owner.
        drop(table);
        let cache_rotation = if stats.old_versions > 0 {
            self.rotate_native_session_after_prune(&owner, latest).await
        } else {
            NativeSessionRotationOutcome::Skipped {
                reason: "No old native versions were retired; current caches retained".into(),
            }
        };
        let outcome = NativeHistoryPruneOutcome::Pruned {
            old_versions: stats.old_versions,
            bytes_removed: stats.bytes_removed,
            cache_rotation,
        };
        drop(_prune);
        drop(_exclusive);
        Ok(outcome)
    }
}
