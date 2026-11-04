//! Logical source snapshots select immutable physical file revisions.
//!
//! A small delta is written per edit. Checkpoints bound ancestry depth. This
//! catalog does not publish the server's active generation; that remains the
//! final, serialized registry operation after both search stores are ready.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};

use engram_core::memory::{AllocationGuard, MemoryBudget, Subsystem};
use serde::{Deserialize, Serialize};
use tantivy::query::{BooleanQuery, ConstScoreQuery, EmptyQuery, EnableScoring, Occur, Query, TermQuery, TermSetQuery, Weight};
use tantivy::schema::{IndexRecordOption, Term};
use crate::tantivy_index::Fields;

const FORMAT: u32 = 1;
const MAX_DEPTH: usize = 32;
const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
const MAX_ANCESTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SELECTION_ENTRIES: usize = 250_000;
const CACHE_ENTRIES: usize = 4;
// Native Lance planning gets costly for fragmented revision disjunctions.
// Keep simple generation predicates for small selections; a measured crossover
// at 16 groups avoids the large OR tree without retaining another cached form.
#[cfg(feature = "vector")]
const COMPOSITE_SELECTOR_MIN_GROUPS: usize = 16;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    project: String,
    namespace: String,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Selection {
    /// A pre-manifest full physical generation, or a full reindex.
    Legacy(u64),
    Files(BTreeMap<String, u64>),
}

pub(super) struct Snapshot {
    pub generation: u64,
    pub selection: Selection,
    depth: usize,
    ancestry: Vec<u64>,
    query: OnceLock<Arc<dyn Query>>,
    #[cfg(feature = "vector")]
    sql: OnceLock<String>,
    _allocation: Option<AllocationGuard>,
}

impl Snapshot {
    pub fn contains(&self, path: &str, physical_generation: u64) -> bool {
        match &self.selection {
            Selection::Legacy(generation) => *generation == physical_generation,
            Selection::Files(files) => files.get(path) == Some(&physical_generation),
        }
    }

    pub fn query(&self, fields: &Fields) -> Box<dyn Query> {
        let query = self.query.get_or_init(|| {
            let generation = |value| Box::new(TermQuery::new(Term::from_field_u64(fields.generation, value), IndexRecordOption::Basic)) as Box<dyn Query>;
            let query: Box<dyn Query> = match &self.selection {
                Selection::Legacy(value) => generation(*value),
                Selection::Files(files) => {
                    let mut groups: BTreeMap<u64, Vec<Term>> = BTreeMap::new();
                    for (path, revision) in files { groups.entry(*revision).or_default().push(Term::from_field_text(fields.path, path)); }
                    let clauses = groups.into_iter().map(|(revision, paths)| (Occur::Should,
                        Box::new(BooleanQuery::new(vec![(Occur::Must, generation(revision)),
                            (Occur::Must, Box::new(TermSetQuery::new(paths)))])) as Box<dyn Query>)).collect::<Vec<_>>();
                    if clauses.is_empty() { Box::new(EmptyQuery) } else { Box::new(BooleanQuery::new(clauses)) }
                },
            };
            // Membership must not boost content from rarer physical revisions.
            // Otherwise an editor save changes relevance even for equal text.
            Arc::from(Box::new(ConstScoreQuery::new(query, 0.0)) as Box<dyn Query>)
        });
        Box::new(SharedQuery(query.clone()))
    }

    #[cfg(feature = "vector")]
    pub fn sql(&self) -> &str {
        self.sql.get_or_init(|| match &self.selection {
            Selection::Legacy(generation) => format!("generation = {}", crate::sql_literals::u64_literal(*generation)),
            Selection::Files(files) => {
                let mut revisions = BTreeSet::new();
                for revision in files.values() {
                    revisions.insert(*revision);
                    if revisions.len() >= COMPOSITE_SELECTOR_MIN_GROUPS { break; }
                }
                if revisions.len() >= COMPOSITE_SELECTOR_MIN_GROUPS {
                    use std::fmt::Write;
                    const PREFIX: &str = "path IS NOT NULL AND generation IS NOT NULL AND concat(CAST(generation AS STRING), ':', path) IN (";
                    // UInt64 decimal has at most 20 digits and no colon. The
                    // first colon therefore separates the exact generation
                    // from the entire path, including any colons in that path.
                    // Account for doubled SQL quotes before allocating. The
                    // snapshot's existing selection reservation covers this
                    // sole cached SQL form; do not construct both selectors.
                    let capacity = PREFIX.len() + 1 + files.keys().map(|path|
                        path.len() + path.bytes().filter(|b| *b == b'\'').count() + 24).sum::<usize>();
                    let mut sql = String::with_capacity(capacity);
                    sql.push_str(PREFIX);
                    for (i, (path, revision)) in files.iter().enumerate() {
                        if i > 0 { sql.push(','); }
                        let _ = write!(sql, "'{revision}:");
                        for ch in path.chars() {
                            sql.push(ch);
                            if ch == '\'' { sql.push('\''); }
                        }
                        sql.push('\'');
                    }
                    sql.push(')');
                    return sql;
                }
                let mut groups: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
                for (path, revision) in files { groups.entry(*revision).or_default().push(path); }
                let terms = groups.into_iter().map(|(revision, paths)| {
                    let values = paths.into_iter().map(|path| format!("'{}'", path.replace('\'', "''"))).collect::<Vec<_>>().join(",");
                    format!("(generation = {} AND path IN ({values}))", crate::sql_literals::u64_literal(revision))
                }).collect::<Vec<_>>();
                fn balanced(terms: &[String]) -> String {
                    match terms.len() {
                        0 => "false".into(),
                        1 => terms[0].clone(),
                        count => { let (left, right) = terms.split_at(count / 2); format!("({} OR {})", balanced(left), balanced(right)) },
                    }
                }
                balanced(&terms)
            },
        })
    }
}

#[derive(Clone, Debug)]
struct SharedQuery(Arc<dyn Query>);
impl Query for SharedQuery {
    fn weight(&self, scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> { self.0.weight(scoring) }
    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) { self.0.query_terms(visitor); }
}

/// Holding this Arc pins the selected physical revisions against collection.
pub(super) type Lease = Arc<Snapshot>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: u32,
    project: String,
    namespace: String,
    generation: u64,
    parent: Option<u64>,
    pending: bool,
    /// Present at a checkpoint, including the first legacy-to-manifest update.
    files: Option<BTreeMap<String, u64>>,
    /// A missing revision explicitly deletes a file from this snapshot.
    changes: BTreeMap<String, Option<u64>>,
    #[serde(skip)]
    _allocation: Option<AllocationGuard>,
    #[serde(skip)]
    serialized_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct InvalidRecord(&'static str);

#[derive(Default)]
struct State {
    snapshots: HashMap<Key, Lease>,
    floors: HashMap<(String, String), u64>,
}

pub(super) struct Catalog {
    root: PathBuf,
    budget: Option<Arc<MemoryBudget>>,
    state: Mutex<State>,
    // Weak ownership preserves the catalog-only lease across Registry teardown.
    source_registry: Mutex<Option<Weak<engram_core::Registry>>>,
    _process_lock: Option<std::fs::File>,
    pub mutations: Arc<super::mutation::Coordinator>,
    #[cfg(feature = "vector")]
    vector_routes: Mutex<Option<Arc<super::vector_epochs::Routes>>>,
}

static CATALOGS: LazyLock<Mutex<HashMap<PathBuf, Weak<Catalog>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

impl Catalog {
    /// Read in-memory ownership only. No disk reads, pinning, pruning or cache
    /// maintenance. Bounds keep a diagnostics request from scanning huge views.
    pub fn retention_observation(&self, project: &str) -> serde_json::Value {
        use serde_json::json;
        let Ok(state) = self.state.try_lock() else {
            return json!({"status":"busy_or_unavailable"});
        };
        let mut views = Vec::new();
        let mut inspected_keys = 0;
        let mut omitted_views = 0;
        let mut remaining_files = 4096usize;
        for (key, snapshot) in state.snapshots.iter().take(512) {
            inspected_keys += 1;
            if key.project != project { continue; }
            if views.len() >= 16 { omitted_views += 1; continue; }
            let (file_entries, groups, omitted_files) = match &snapshot.selection {
                Selection::Legacy(_) => (None, 1, 0),
                Selection::Files(files) => {
                    let inspected = files.len().min(remaining_files);
                    let groups: BTreeSet<_> = files.values().take(inspected).copied().collect();
                    remaining_files -= inspected;
                    (Some(files.len()), groups.len(), files.len() - inspected)
                },
            };
            views.push(json!({"namespace":key.namespace,"generation":snapshot.generation,
                "external_pin_references":Arc::strong_count(snapshot).saturating_sub(1),
                "selected_file_entries":file_entries,"observed_physical_revision_groups":groups,
                "group_count_is_lower_bound":omitted_files>0,"file_entries_not_inspected":omitted_files}));
        }
        json!({"status":"observed","catalog_cached_views":state.snapshots.len(),
            "project_views":views,"project_views_omitted":omitted_views,
            "catalog_keys_not_inspected":state.snapshots.len().saturating_sub(inspected_keys),
            "scope":"Cached views only, not every durable snapshot; one catalog-owned Arc per view is excluded from pin counts. Group counts are per cached view, not unique across views.",
            "limits":{"catalog_keys":512,"returned_views":16,"inspected_file_entries":4096}})
    }

    pub fn new(root: PathBuf, budget: Option<Arc<MemoryBudget>>) -> Self {
        Self { root, budget, state: Mutex::new(State::default()), source_registry: Mutex::new(None), _process_lock: None,
            mutations: Arc::new(super::mutation::Coordinator::default()),
            #[cfg(feature = "vector")]
            vector_routes: Mutex::new(None) }
    }

    #[cfg(feature = "vector")]
    pub fn bind_vectors(&self, root: &Path) -> anyhow::Result<Arc<super::vector_epochs::Routes>> {
        let root = root.canonicalize()?;
        let mut routes = self.vector_routes.lock().map_err(|_| anyhow::anyhow!("Vector root binding poisoned"))?;
        if let Some(routes) = routes.as_ref() {
            routes.check_root(&root)?;
            return Ok(routes.clone());
        }
        let bound = Arc::new(super::vector_epochs::Routes::new(self.root.join("vector-routes"), root)?);
        *routes = Some(bound.clone());
        Ok(bound)
    }

    /// Runtime reloads/retry engines for the same index share reader pins.
    pub fn shared(root: PathBuf, budget: Option<Arc<MemoryBudget>>) -> anyhow::Result<Arc<Self>> {
        let parent = root.parent().ok_or_else(|| anyhow::anyhow!("snapshot catalog has no parent"))?;
        let name = root.file_name().ok_or_else(|| anyhow::anyhow!("snapshot catalog has no name"))?;
        let canonical = std::fs::canonicalize(parent)?.join(name);
        let mut catalogs = CATALOGS.lock().map_err(|_| anyhow::anyhow!("snapshot catalog registry poisoned"))?;
        catalogs.retain(|_, catalog| catalog.strong_count() > 0);
        if let Some(catalog) = catalogs.get(&canonical).and_then(Weak::upgrade) { return Ok(catalog); }
        // Reader pins are process-local. Exclude another process for the entire
        // catalog lifetime so its collector cannot bypass those active pins.
        let lock_path = engram_core::safe_join(canonical.parent().expect("checked above"), ".source-snapshot-catalog.lock")?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0x00000001 | 0x00000002);
        }
        let lock = options.open(lock_path)?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|error| anyhow::anyhow!("source index is already open in another process; connect through the primary Engram server: {error}"))?;
        let mut owned = Self::new(canonical.clone(), budget);
        owned._process_lock = Some(lock);
        let catalog = Arc::new(owned);
        catalogs.insert(canonical, Arc::downgrade(&catalog));
        Ok(catalog)
    }

    /// Bind authority without opening/pinning source or admitting a mutation.
    /// Runtime construction must remain available for unrelated namespaces.
    pub fn bind_source_registry(&self, registry: &Arc<engram_core::Registry>) -> anyhow::Result<()> {
        *self.source_registry.lock().map_err(|_| anyhow::anyhow!("source authority binding poisoned"))? = Some(Arc::downgrade(registry));
        Ok(())
    }

    fn independently_managed(&self, project: &str) -> anyhow::Result<bool> {
        let binding = self.source_registry.lock().map_err(|_| anyhow::anyhow!("source authority binding poisoned"))?.clone();
        let Some(binding) = binding else { return Ok(false); };
        let registry = binding.upgrade().ok_or_else(|| anyhow::anyhow!("Source authority is unavailable; reopen the runtime"))?;
        let flag = registry.get_meta_bounded(project, engram_core::registry::SOURCE_SNAPSHOTS_REQUIRED_KEY, 1)?;
        anyhow::ensure!(flag.as_deref().is_none_or(|value| value == "1"), "Invalid source snapshot requirement metadata");
        Ok(flag.is_some())
    }

    fn legacy_permitted(&self, project: &str, generation: u64) -> anyhow::Result<bool> {
        let binding = self.source_registry.lock().map_err(|_| anyhow::anyhow!("source authority binding poisoned"))?.clone();
        let Some(binding) = binding else { return Ok(true); };
        let registry = binding.upgrade().ok_or_else(|| anyhow::anyhow!(
            "Source authority is unavailable; reopen the source runtime before starting a new read"))?;
        let (head, requirement) = registry.source_snapshot_requirement(project)?;
        use engram_core::registry::SourceSnapshotRequirement;
        Ok(match requirement {
            SourceSnapshotRequirement::Unpublished => generation == 0 && head == 0,
            SourceSnapshotRequirement::AttestedLegacy => generation == 1 && head == 1,
            SourceSnapshotRequirement::Managed | SourceSnapshotRequirement::Unproven => false,
        })
    }

    fn scope(&self, project: &str, namespace: &str) -> PathBuf {
        let identity = blake3::hash(format!("{project}\0{namespace}").as_bytes());
        self.root.join(identity.to_hex().as_str())
    }

    fn path(&self, key: &Key) -> PathBuf {
        self.scope(&key.project, &key.namespace).join(format!("{}.json", key.generation))
    }

    pub fn managed(&self, project: &str, namespace: &str) -> anyhow::Result<bool> {
        let path = self.checked_path(&self.scope(project, namespace))?;
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => { anyhow::ensure!(metadata.is_dir() && !metadata.file_type().is_symlink(), "unsupported source snapshot directory"); Ok(true) },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn checked_path(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let parent = self.root.parent().ok_or_else(|| anyhow::anyhow!("snapshot catalog has no parent"))?;
        let relative = path.strip_prefix(parent)?.to_str().ok_or_else(|| anyhow::anyhow!("invalid snapshot path"))?;
        Ok(engram_core::safe_join(parent, relative)?)
    }

    fn read_bytes(&self, path: &Path, limit: usize) -> anyhow::Result<Option<(Vec<u8>, Option<AllocationGuard>)>> {
        let path = self.checked_path(path)?;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink(), "unsupported snapshot metadata file");
        anyhow::ensure!(metadata.len() <= limit as u64, "source snapshot metadata exceeds its read limit");
        let parent = self.root.parent().ok_or_else(|| anyhow::anyhow!("snapshot catalog has no parent"))?;
        let relative = path.strip_prefix(parent)?.to_str().ok_or_else(|| anyhow::anyhow!("invalid snapshot path"))?;
        let file = engram_core::safe_open_read(parent, relative)?;
        let length = file.metadata()?.len();
        anyhow::ensure!(length <= limit as u64, "source snapshot metadata exceeds its read limit");
        // Reserve before allocating JSON bytes or deserializing map nodes. The
        // conservative expansion allowance remains held by the parsed record.
        let allocation = self.reserve(length.saturating_mul(24).saturating_add(1024), "source snapshot metadata read/parse")?;
        let mut bytes = Vec::new();
        file.take(length + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= length, "source snapshot metadata grew during read; retry");
        Ok(Some((bytes, allocation)))
    }

    fn reserve(&self, bytes: u64, operation: &str) -> anyhow::Result<Option<AllocationGuard>> {
        self.budget.as_ref().map(|budget|
            AllocationGuard::try_new(budget, bytes, Subsystem::Misc, operation)).transpose()
    }

    fn legacy_boundary(&self, project: &str, namespace: &str) -> anyhow::Result<Option<u64>> {
        self.read_bytes(&self.scope(project, namespace).join("legacy-through.json"), 64)?
            .map(|(bytes, _allocation)| serde_json::from_slice::<u64>(&bytes).map_err(Into::into)).transpose()
    }

    fn scope_has_metadata(&self, project: &str, namespace: &str) -> anyhow::Result<bool> {
        let path = self.checked_path(&self.scope(project, namespace))?;
        match std::fs::read_dir(path) {
            Ok(mut entries) => entries.next().transpose().map(|entry| entry.is_some()).map_err(Into::into),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Called under the catalog mutex. An empty directory can remain after an
    /// interrupted first transition. Existing metadata without its boundary is
    /// corruption; only an explicit full rebuild may recover it, conservatively
    /// allowing no implicit legacy generations above zero.
    fn ensure_legacy_boundary(&self, project: &str, namespace: &str, base: u64, full_rebuild: bool, empty_before_boundaries: bool) -> anyhow::Result<()> {
        if self.legacy_boundary(project, namespace)?.is_some() { return Ok(()); }
        let has_metadata = self.scope_has_metadata(project, namespace)?;
        anyhow::ensure!(!has_metadata || full_rebuild || empty_before_boundaries,
            "source snapshot legacy boundary is missing; full repair required");
        // The retirement initializer just created zero in the same locked
        // operation. Only its observed-empty result proves this is a first
        // transition; an older floor-only scope has no such authority.
        let boundary = if has_metadata && !empty_before_boundaries { 0 } else { base };
        self.write_json(&self.scope(project, namespace).join("legacy-through.json"), &serde_json::to_vec(&boundary)?)
    }

    fn allow_legacy(&self, key: &Key) -> anyhow::Result<()> {
        match self.legacy_boundary(&key.project, &key.namespace)? {
            Some(boundary) => anyhow::ensure!(key.generation <= boundary,
                "managed source snapshot metadata is missing; full repair required"),
            None => anyhow::ensure!(!self.scope_has_metadata(&key.project, &key.namespace)?,
                "source snapshot legacy boundary is missing; full repair required"),
        }
        Ok(())
    }

    fn read_record(&self, key: &Key) -> anyhow::Result<Option<Record>> {
        let Some((bytes, allocation)) = self.read_bytes(&self.path(key), MAX_RECORD_BYTES)? else { return Ok(None); };
        let mut record: Record = serde_json::from_slice(&bytes)?;
        record._allocation = allocation;
        record.serialized_bytes = bytes.len() as u64;
        anyhow::ensure!(record.format == FORMAT && record.project == key.project
            && record.namespace == key.namespace && record.generation == key.generation,
            InvalidRecord("source snapshot identity/format mismatch"));
        anyhow::ensure!(record.parent.is_none_or(|parent| parent < key.generation), InvalidRecord("cyclic source snapshot ancestry"));
        anyhow::ensure!(record.files.as_ref().is_none_or(|files| files.values().all(|revision| *revision <= key.generation)),
            InvalidRecord("source snapshot checkpoint references a future revision"));
        anyhow::ensure!(record.changes.values().flatten().all(|revision| *revision <= key.generation),
            InvalidRecord("source snapshot delta references a future revision"));
        Ok(Some(record))
    }

    fn write_json(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(bytes.len() <= MAX_RECORD_BYTES, "source snapshot metadata exceeds its 32 MiB limit");
        let checked = self.checked_path(path)?;
        let parent = checked.parent().ok_or_else(|| anyhow::anyhow!("snapshot path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let _ = self.checked_path(path)?;
        let root = std::fs::canonicalize(&self.root)?;
        let parent = std::fs::canonicalize(parent)?;
        anyhow::ensure!(parent.starts_with(root), "source snapshot path escapes catalog");
        let name = path.file_name().ok_or_else(|| anyhow::anyhow!("snapshot path has no filename"))?;
        let destination = parent.join(name);
        if let Ok(metadata) = std::fs::symlink_metadata(&destination) {
            anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink(), "unsupported source snapshot target");
        }
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        file.persist(destination).map_err(|error| error.error)?;
        Ok(())
    }

    fn load(&self, key: &Key, remaining: usize, state: &State, durable_seal: bool, permit_legacy: bool) -> anyhow::Result<Snapshot> {
        let mut cursor = key.clone();
        let mut records = Vec::new();
        let mut ancestry_bytes = 0u64;
        let mut checkpoint = None;
        let mut cached_base: Option<Lease> = None;
        loop {
            anyhow::ensure!(records.len() < remaining, "source snapshot ancestry exceeds its depth limit");
            let Some(mut record) = self.read_record(&cursor)? else {
                anyhow::ensure!(records.is_empty(), "source snapshot ancestor is missing");
                anyhow::ensure!(permit_legacy,
                    "Source snapshot layout is managed or unproven, but required metadata is missing; full repair required");
                self.allow_legacy(key)?;
                return Ok(Snapshot { generation: key.generation, selection: Selection::Legacy(key.generation),
                    depth: 0, ancestry: Vec::new(), query: OnceLock::new(), #[cfg(feature = "vector")] sql: OnceLock::new(), _allocation: None });
            };
            // Only the candidate being sealed may still be pending. Every
            // persisted parent must be complete even if a cached view exists.
            anyhow::ensure!(!record.pending || (durable_seal && cursor == *key),
                "source snapshot is unpublished/incomplete; retry after indexing finishes");
            ancestry_bytes = ancestry_bytes.saturating_add(record.serialized_bytes);
            anyhow::ensure!(ancestry_bytes <= MAX_ANCESTRY_BYTES, "source snapshot ancestry exceeds its aggregate byte limit");
            match (record.files.take(), record.parent) {
                (Some(files), None) => {
                    checkpoint = Some(files);
                    records.push(record);
                    break;
                },
                (None, Some(parent)) => {
                    records.push(record);
                    cursor.generation = parent;
                    if !durable_seal {
                        if let Some(snapshot) = state.snapshots.get(&cursor) {
                            anyhow::ensure!(matches!(snapshot.selection, Selection::Files(_)), "source snapshot ancestor is missing");
                            cached_base = Some(snapshot.clone());
                            break;
                        }
                    }
                },
                _ => anyhow::bail!("invalid source snapshot checkpoint/parent"),
            }
        }
        let base_files = checkpoint.as_ref().or_else(|| cached_base.as_ref().and_then(|base| {
            if let Selection::Files(files) = &base.selection { Some(files) } else { None }
        })).ok_or_else(|| anyhow::anyhow!("source snapshot checkpoint is missing"))?;
        anyhow::ensure!(base_files.len() <= MAX_SELECTION_ENTRIES, "source snapshot selection exceeds its entry limit");
        let estimate = |path: &String| (path.len() as u64).saturating_mul(4).saturating_add(256);
        let estimated = base_files.keys().map(estimate)
            .chain(records.iter().flat_map(|record| record.changes.keys()).map(estimate))
            .fold(0u64, u64::saturating_add);
        // One map is cloned at most once, after reserving for that clone and
        // its lexical/SQL selectors. Record guards cover the delta buffers.
        let depth = cached_base.as_ref().map_or(records.len() - 1, |base| base.depth + records.len());
        let ancestry_length = cached_base.as_ref().map_or(0, |base| base.ancestry.len()) + records.len();
        anyhow::ensure!(depth < MAX_DEPTH && ancestry_length <= MAX_DEPTH,
            "source snapshot resolved ancestry exceeds its depth limit");
        let allocation = self.reserve(estimated, "resolved source snapshot selection")?;
        let mut ancestry = cached_base.as_ref().map_or_else(Vec::new, |base| base.ancestry.clone());
        let mut files = match checkpoint {
            Some(files) => files,
            None => match &cached_base.as_ref().expect("base checked above").selection {
                Selection::Files(files) => files.clone(),
                Selection::Legacy(_) => unreachable!("base checked above"),
            },
        };
        for record in records.into_iter().rev() {
            for (path, revision) in record.changes {
                if let Some(revision) = revision {
                    anyhow::ensure!(revision <= record.generation, "source snapshot references a future revision");
                    anyhow::ensure!(files.contains_key(&path) || files.len() < MAX_SELECTION_ENTRIES,
                        "source snapshot selection exceeds its entry limit");
                    files.insert(path, revision);
                } else { files.remove(&path); }
            }
            if durable_seal {
                let mut observed = key.clone();
                observed.generation = record.generation;
                if let Some(known) = state.snapshots.get(&observed) {
                    anyhow::ensure!(matches!(&known.selection, Selection::Files(selected) if selected == &files),
                        "Persisted source selection differs from a known immutable view; full repair required");
                }
            }
            ancestry.push(record.generation);
        }
        Ok(Snapshot { generation: key.generation, selection: Selection::Files(files), depth,
            ancestry, query: OnceLock::new(), #[cfg(feature = "vector")] sql: OnceLock::new(), _allocation: allocation })
    }

    fn retirement_boundary(&self, project: &str, namespace: &str) -> anyhow::Result<Option<u64>> {
        self.read_bytes(&self.scope(project, namespace).join("retired-before.json"), 64)?
            .map(|(bytes, _allocation)| serde_json::from_slice::<u64>(&bytes).map_err(Into::into)).transpose()
    }

    /// No FORMAT=1 record distinguishes never-collected metadata from a lost
    /// retirement boundary. A nonempty scope therefore cannot adopt zero.
    /// Only an explicit parentless rebuild with a published logical base may
    /// restore missing authority, without retiring that still-published head.
    /// A recovery physical high-water is not such a base.
    fn ensure_retirement_boundary(&self, state: &mut State, project: &str, namespace: &str,
        published_base: u64, full_rebuild: bool) -> anyhow::Result<bool> {
        let scope = (project.to_owned(), namespace.to_owned());
        let known = state.floors.get(&scope).copied().unwrap_or(0);
        if let Some(floor) = self.retirement_boundary(project, namespace)? {
            anyhow::ensure!(floor >= known, "Source retirement boundary regressed; explicit metadata recovery required");
            state.floors.insert(scope, floor);
            return Ok(false);
        }
        let known_managed = state.snapshots.iter().any(|(key, snapshot)| key.project == project
            && key.namespace == namespace && matches!(snapshot.selection, Selection::Files(_)));
        let has_authority = self.scope_has_metadata(project, namespace)? || known != 0 || known_managed
            || self.independently_managed(project)?;
        anyhow::ensure!(!has_authority || full_rebuild,
            "Managed source retirement boundary is missing; full repair from a valid published head is required");
        anyhow::ensure!(published_base >= known, "Published source base is older than known retirement authority");
        let floor = if has_authority { published_base } else { 0 };
        self.write_json(&self.scope(project, namespace).join("retired-before.json"), &serde_json::to_vec(&floor)?)?;
        state.floors.insert(scope, floor);
        Ok(!has_authority)
    }

    fn floor(&self, state: &mut State, project: &str, namespace: &str) -> anyhow::Result<u64> {
        let key = (project.to_owned(), namespace.to_owned());
        if let Some(floor) = state.floors.get(&key) { return Ok(*floor); }
        let floor = match self.retirement_boundary(project, namespace)? {
            Some(floor) => floor,
            None => {
                anyhow::ensure!(!self.scope_has_metadata(project, namespace)?,
                    "Managed source retirement boundary is missing; full repair from a valid published head is required");
                0
            }
        };
        state.floors.insert(key, floor);
        Ok(floor)
    }

    fn make_cache_room(state: &mut State) {
        // The same bounded policy applies to read loads and newly sealed
        // candidates. Pinned views keep their actual allocation leases.
        while state.snapshots.len() >= CACHE_ENTRIES {
            let victim = state.snapshots.iter().filter(|(_, view)| Arc::strong_count(view) == 1)
                .min_by_key(|(key, _)| key.generation).map(|(key, _)| key.clone());
            let Some(victim) = victim else { break; };
            state.snapshots.remove(&victim);
        }
    }

    pub fn pin(&self, project: &str, namespace: &str, generation: u64) -> anyhow::Result<Lease> {
        self.pin_inner(project, namespace, generation, true)
    }

    /// Begin a request rather than joining a view its existing lease protects.
    pub fn pin_fresh(&self, project: &str, namespace: &str, generation: u64) -> anyhow::Result<Lease> {
        self.pin_inner(project, namespace, generation, false)
    }

    fn pin_inner(&self, project: &str, namespace: &str, generation: u64, join_retired: bool) -> anyhow::Result<Lease> {
        engram_core::security::validate_key_component("project", project).map_err(anyhow::Error::msg)?;
        engram_core::security::validate_key_component("namespace", namespace).map_err(anyhow::Error::msg)?;
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("snapshot catalog lock poisoned"))?;
        let key = Key { project: project.into(), namespace: namespace.into(), generation };
        let floor = self.floor(&mut state, project, namespace)?;
        if generation < floor {
            // A higher-level read can pin once and make several nested engine
            // calls. Those calls may join the still-protected immutable view.
            // Once the last reader leaves, a stale generation cannot reopen it.
            if join_retired {
                if let Some(view) = state.snapshots.get(&key).filter(|view| Arc::strong_count(view) > 1) { return Ok(view.clone()); }
            }
            anyhow::bail!("source snapshot retired; refresh the request generation");
        }
        // Nested reads may finish a real existing lease after its authority
        // owner closes. Presence of a cached Legacy value alone grants nothing
        // to a fresh request, including after a managed publication.
        if join_retired && let Some(snapshot) = state.snapshots.get(&key).filter(|view| Arc::strong_count(view) > 1) {
            return Ok(snapshot.clone());
        }
        let permit_legacy = self.legacy_permitted(project, generation)?;
        if let Some(snapshot) = state.snapshots.get(&key) {
            anyhow::ensure!(permit_legacy || matches!(snapshot.selection, Selection::Files(_)),
                "Source snapshot layout is managed or unproven, but required metadata is missing; full repair required");
            if matches!(snapshot.selection, Selection::Legacy(_)) { self.allow_legacy(&key)?; }
            return Ok(snapshot.clone());
        }
        Self::make_cache_room(&mut state);
        let view = Arc::new(self.load(&key, MAX_DEPTH + 1, &state, false, permit_legacy)?);
        state.snapshots.insert(key, view.clone());
        Ok(view)
    }

    /// Caller owns the project's serialized update lock and clears abandoned
    /// physical rows at new_generation before writing this attempt's chunks.
    pub fn prepare(&self, project: &str, namespace: &str, base: u64, generation: u64,
        legacy_paths: &[String], changed: &[String], deleted: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(generation > base, "source snapshot must advance its base");
        let parent = self.pin(project, namespace, base)?;
        let key = Key { project: project.into(), namespace: namespace.into(), generation };
        {
            let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("snapshot catalog lock poisoned"))?;
            // Establish zero before the first managed metadata. Never use a
            // cached zero to recreate a missing boundary in a nonempty scope.
            let empty_before_boundaries = self.ensure_retirement_boundary(&mut state, project, namespace, base, false)?;
            self.ensure_legacy_boundary(project, namespace, base, false, empty_before_boundaries)?;
            anyhow::ensure!(state.snapshots.get(&key).is_none_or(|view| Arc::strong_count(view) == 1), "cannot replace a pinned source snapshot");
            state.snapshots.remove(&key);
        }
        // Validate any preexisting record before replacing an abandoned attempt.
        let _ = self.read_record(&key)?;
        let checkpoint = matches!(parent.selection, Selection::Legacy(_)) || parent.depth + 1 >= MAX_DEPTH;
        let estimate = |path: &String| (path.len() as u64).saturating_mul(4).saturating_add(256);
        let base_bytes = if checkpoint {
            match &parent.selection {
                Selection::Files(files) => files.keys().map(estimate).fold(0u64, u64::saturating_add),
                Selection::Legacy(_) => legacy_paths.iter().map(estimate).fold(0u64, u64::saturating_add),
            }
        } else { 0 };
        let changed_bytes = changed.iter().chain(deleted).map(estimate).fold(0u64, u64::saturating_add);
        let allocation = self.reserve(base_bytes.saturating_add(changed_bytes), "source snapshot staging/serialization")?;
        let files = if checkpoint {
            Some(match &parent.selection {
                Selection::Files(files) => files.clone(),
                Selection::Legacy(revision) => legacy_paths.iter().map(|path| (path.clone(), *revision)).collect(),
            })
        } else { None };
        let mut changes: BTreeMap<_, _> = deleted.iter().map(|path| (path.clone(), None)).collect();
        changes.extend(changed.iter().map(|path| (path.clone(), Some(generation))));
        let record = Record { format: FORMAT, project: project.into(), namespace: namespace.into(), generation,
            parent: (!checkpoint).then_some(base), pending: true, files, changes, _allocation: allocation, serialized_bytes: 0 };
        self.write_json(&self.path(&key), &serde_json::to_vec(&record)?)
    }

    /// Full repair must not depend on loading the damaged old selection.
    pub fn prepare_full(&self, project: &str, namespace: &str, base: u64, generation: u64, paths: &[String]) -> anyhow::Result<()> {
        self.prepare_full_inner(project, namespace, base, generation, paths, None)
    }

    /// Recovery must never overwrite a manifest or a view held by an old reader.
    pub fn prepare_fresh_full(&self, project: &str, namespace: &str, base: u64, generation: u64,
        paths: &[String], budget: &super::source_highwater::WorkBudget<'_>) -> anyhow::Result<()> {
        self.prepare_full_inner(project, namespace, base, generation, paths, Some(budget))
    }

    fn prepare_full_inner(&self, project: &str, namespace: &str, base: u64, generation: u64,
        paths: &[String], fresh: Option<&super::source_highwater::WorkBudget<'_>>) -> anyhow::Result<()> {
        engram_core::security::validate_key_component("project", project).map_err(anyhow::Error::msg)?;
        engram_core::security::validate_key_component("namespace", namespace).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(generation > base, "source snapshot must advance its base");
        anyhow::ensure!(paths.len() <= MAX_SELECTION_ENTRIES, "source snapshot selection exceeds its entry limit");
        let key = Key { project: project.into(), namespace: namespace.into(), generation };
        let mut state = if fresh.is_some() {
            self.state.try_lock().map_err(|_| anyhow::anyhow!("Source recovery catalog is busy or unavailable; retry"))?
        } else {
            self.state.lock().map_err(|_| anyhow::anyhow!("snapshot catalog lock poisoned"))?
        };
        if let Some(budget) = fresh {
            budget.check()?;
            anyhow::ensure!(!state.snapshots.contains_key(&key), "Recovered source snapshot target is already cached; refresh its generation floor");
            match std::fs::symlink_metadata(self.checked_path(&self.path(&key))?) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => anyhow::bail!("Recovered source snapshot target already exists; refresh its generation floor"),
            }
        }
        anyhow::ensure!(state.snapshots.get(&key).is_none_or(|view| Arc::strong_count(view) == 1), "cannot replace a pinned source snapshot");
        // Ordinary full repair receives the validated published generation.
        // Fresh damaged-HEAD recovery instead receives a physical high-water;
        // it may not use that number to retire a concurrent published source.
        let empty_before_boundaries = self.ensure_retirement_boundary(&mut state, project, namespace, base, fresh.is_none())?;
        self.ensure_legacy_boundary(project, namespace, base, true, empty_before_boundaries)?;
        // A malformed abandoned attempt is deliberately replaced by full repair.
        state.snapshots.remove(&key);
        let estimate = paths.iter().map(|path| (path.len() as u64).saturating_mul(4).saturating_add(256)).fold(0u64, u64::saturating_add);
        let allocation = self.reserve(estimate, "full source snapshot staging")?;
        let record = Record { format: FORMAT, project: project.into(), namespace: namespace.into(), generation,
            parent: None, pending: true, files: Some(paths.iter().map(|path| (path.clone(), generation)).collect()),
            changes: BTreeMap::new(), _allocation: allocation, serialized_bytes: 0 };
        self.write_json(&self.path(&key), &serde_json::to_vec(&record)?)
    }

    /// Filenames reserve generations even for empty or malformed abandoned
    /// manifests. Recovery deliberately does not deserialize their contents.
    pub fn recovery_high_water(&self, project: &str, namespace: &str,
        budget: &mut super::source_highwater::WorkBudget<'_>) -> anyhow::Result<u64> {
        use super::source_highwater::MAX_METADATA_ENTRIES;
        engram_core::security::validate_key_component("project", project).map_err(anyhow::Error::msg)?;
        engram_core::security::validate_key_component("namespace", namespace).map_err(anyhow::Error::msg)?;
        budget.check()?;
        let state = self.state.try_lock().map_err(|_| anyhow::anyhow!("Source recovery catalog is busy or unavailable; retry"))?;
        anyhow::ensure!(state.snapshots.len() <= MAX_METADATA_ENTRIES && state.floors.len() <= MAX_METADATA_ENTRIES,
            "Source recovery inspection budget exceeded (4096 cached catalog entries); source unchanged");
        let mut maximum = state.floors.get(&(project.into(), namespace.into())).copied().unwrap_or(0);
        for (key, snapshot) in &state.snapshots {
            budget.charge(1)?;
            if key.project != project || key.namespace != namespace { continue; }
            maximum = maximum.max(key.generation).max(snapshot.generation);
            match &snapshot.selection {
                Selection::Legacy(generation) => maximum = maximum.max(*generation),
                Selection::Files(files) => for generation in files.values() {
                    budget.charge(1)?;
                    maximum = maximum.max(*generation);
                },
            }
        }
        if !self.managed(project, namespace)? { return Ok(maximum); }
        let directory = self.checked_path(&self.scope(project, namespace))?;
        let mut entries = 0usize;
        for entry in std::fs::read_dir(&directory)? {
            budget.charge(1)?;
            entries += 1;
            anyhow::ensure!(entries <= MAX_METADATA_ENTRIES,
                "Source recovery inspection budget exceeded (4096 catalog directory entries); source unchanged");
            let entry = entry?;
            let kind = entry.file_type()?;
            anyhow::ensure!(kind.is_file() && !kind.is_symlink(), "Source recovery found unsupported catalog metadata");
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| anyhow::anyhow!("Source recovery found an unsupported catalog filename"))?;
            match name {
                "legacy-through.json" | "retired-before.json" => {
                    let (bytes, _allocation) = self.read_bytes(&directory.join(name), 64)?
                        .ok_or_else(|| anyhow::anyhow!("Source recovery catalog boundary disappeared; retry"))?;
                    maximum = maximum.max(serde_json::from_slice::<u64>(&bytes)?);
                }
                _ => {
                    let stem = name.strip_suffix(".json").ok_or_else(|| anyhow::anyhow!(
                        "Source recovery found an unknown catalog filename; inspect the preserved metadata before retrying"))?;
                    let generation = stem.parse::<u64>().map_err(|_| anyhow::anyhow!(
                        "Source recovery found an unsupported catalog generation filename"))?;
                    anyhow::ensure!(generation.to_string() == stem, "Source recovery found a noncanonical catalog generation filename");
                    maximum = maximum.max(generation);
                }
            }
        }
        budget.check()?;
        Ok(maximum)
    }

    pub fn seal(&self, project: &str, namespace: &str, generation: u64) -> anyhow::Result<()> {
        engram_core::security::validate_key_component("project", project).map_err(anyhow::Error::msg)?;
        engram_core::security::validate_key_component("namespace", namespace).map_err(anyhow::Error::msg)?;
        let key = Key { project: project.into(), namespace: namespace.into(), generation };
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("snapshot catalog lock poisoned"))?;
        // Cached complete views remain usable after metadata damage. They are
        // not proof that a new head can be reconstructed after restart. Read
        // both durable boundaries without consulting the in-memory floor.
        let legacy = self.legacy_boundary(project, namespace)?
            .ok_or_else(|| anyhow::anyhow!("source snapshot legacy boundary is missing; full repair required"))?;
        anyhow::ensure!(legacy < generation, "Source candidate does not advance its durable legacy boundary");
        let durable_floor = self.retirement_boundary(project, namespace)?
            .ok_or_else(|| anyhow::anyhow!("Managed source retirement boundary is missing; full repair from a valid published head is required"))?;
        let scope = (project.to_owned(), namespace.to_owned());
        let known_floor = state.floors.get(&scope).copied().unwrap_or(0);
        anyhow::ensure!(durable_floor >= known_floor && generation >= durable_floor,
            "Source retirement boundary is missing, regressed or newer than the candidate; explicit metadata recovery required");
        // Release unpinned cached maps before reserving/parsing this new
        // candidate's durable chain. Existing candidate Arcs stay intact.
        if !state.snapshots.contains_key(&key) { Self::make_cache_room(&mut state); }
        let mut record = self.read_record(&key)?.ok_or_else(|| anyhow::anyhow!("source snapshot was not prepared"))?;
        // Reuse the same depth, per-record/aggregate bytes, selection limits and
        // pre-allocation guards as normal resolution, but never shortcut a
        // persisted parent through a cached selection during publication.
        let verified = self.load(&key, MAX_DEPTH + 1, &state, true, false)?;
        if let Some(known) = state.snapshots.get(&key) {
            anyhow::ensure!(known.selection == verified.selection && known.ancestry == verified.ancestry,
                "Persisted source ancestry differs from a known immutable view; full repair required");
        }
        record.pending = false;
        self.write_json(&self.path(&key), &serde_json::to_vec(&record)?)?;
        // Keep an existing Arc: replacing it would hide its external readers
        // from GC's reference-count ownership check. A new candidate caches
        // precisely the selection validated from its complete durable chain.
        state.snapshots.entry(key).or_insert_with(|| Arc::new(verified));
        state.floors.insert(scope, durable_floor);
        Ok(())
    }

    /// Advance logical retention and return revisions GC must protect. A caller
    /// already pinned before retirement remains valid; new stale pins fail.
    pub fn protect_for_gc(&self, project: &str, namespace: &str, active: u64) -> anyhow::Result<Vec<Lease>> {
        let active_view = self.pin(project, namespace, active)?;
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("snapshot catalog lock poisoned"))?;
        let previous_floor = self.floor(&mut state, project, namespace)?;
        // A reader may join an already pinned retired view, but that does not
        // authorize a delayed sweep to collect revisions used by a newer head.
        anyhow::ensure!(active >= previous_floor, "stale source snapshot GC head; refresh the published generation");
        let floor = active;
        self.write_json(&self.scope(project, namespace).join("retired-before.json"), &serde_json::to_vec(&floor)?)?;
        state.floors.insert((project.into(), namespace.into()), floor);
        let mut protected = vec![active_view];
        for (key, snapshot) in &state.snapshots {
            if key.project == project && key.namespace == namespace && key.generation != active
                && Arc::strong_count(snapshot) > 1 { protected.push(snapshot.clone()); }
        }
        state.snapshots.retain(|key, snapshot| key.project != project || key.namespace != namespace
            || key.generation >= floor || Arc::strong_count(snapshot) > 1);
        // Keep ancestry metadata separately from physical revisions. Ancestors
        // needed to resolve the active delta are not all live content snapshots.
        // After a checkpoint, old chains can be removed without keeping a file
        // for every editor save forever. New/incomplete future attempts stay.
        self.prune_metadata(project, namespace, active, &protected[0].ancestry)?;
        Ok(protected)
    }

    fn prune_metadata(&self, project: &str, namespace: &str, active: u64, ancestry: &[u64]) -> anyhow::Result<()> {
        let keep: std::collections::HashSet<_> = ancestry.iter().copied().collect();
        let directory = self.checked_path(&self.scope(project, namespace))?;
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut invalid_records = 0usize;
        let mut first_invalid = None;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(number) = name.to_str().and_then(|name| name.strip_suffix(".json")) else { continue; };
            let Ok(generation) = number.parse::<u64>() else { continue; };
            // read_record validates the canonical path, not an arbitrary alias
            // such as 09.json. Never delete a different, unvalidated entry.
            if generation.to_string() != number { continue; }
            if generation >= active || keep.contains(&generation) { continue; }
            let key = Key { project: project.into(), namespace: namespace.into(), generation };
            // Only delete validated catalog records, never an arbitrary file
            // that happens to share a temporary-looking name.
            match self.read_record(&key) {
                Ok(Some(_)) => std::fs::remove_file(self.checked_path(&entry.path())?)?,
                Ok(None) => {},
                // This record is older than active and absent from its verified
                // ancestry. A parentless repair may have superseded corruption;
                // retain that evidence without preventing later collection.
                Err(error) if error.downcast_ref::<serde_json::Error>().is_some()
                    || error.downcast_ref::<InvalidRecord>().is_some() => {
                    invalid_records = invalid_records.saturating_add(1);
                    first_invalid.get_or_insert((generation, error.downcast_ref::<InvalidRecord>()
                        .map(|error| error.0).unwrap_or("JSON parse/type failure")));
                }
                Err(error) => return Err(error),
            }
        }
        if let Some((first_generation, first_problem)) = first_invalid {
            tracing::warn!(project, namespace, invalid_records, first_generation, first_problem,
                "Preserving invalid unreachable source snapshot metadata during collection");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_and_deleted_files_select_new_revisions_without_revising_their_sibling() {
        let temporary = tempfile::tempdir().unwrap();
        let catalog = Catalog::new(temporary.path().join("snapshots"), None);
        let old = catalog.pin("project", "memory", 1).unwrap();
        catalog.prepare("project", "memory", 1, 2, &["a.rs".into(), "b.rs".into(), "deleted.rs".into()],
            &["a.rs".into()], &["deleted.rs".into()]).unwrap();
        assert!(catalog.pin("project", "memory", 2).is_err(), "partial snapshots must not become readable");
        catalog.seal("project", "memory", 2).unwrap();
        let new = catalog.pin("project", "memory", 2).unwrap();
        assert!(new.contains("a.rs", 2));
        assert!(!new.contains("a.rs", 1));
        assert!(new.contains("b.rs", 1));
        assert!(!new.contains("deleted.rs", 1));
        assert!(old.contains("a.rs", 1), "an existing reader must keep its old revision");
        let protected = catalog.protect_for_gc("project", "memory", 2).unwrap();
        assert!(protected.iter().any(|view| view.generation == 1));
        assert!(catalog.pin("project", "memory", 1).is_ok(), "nested reads can finish an already pinned snapshot");
        drop(protected); drop(old);
        let protected = catalog.protect_for_gc("project", "memory", 2).unwrap();
        assert!(!protected.iter().any(|view| view.generation == 1));
        assert!(catalog.pin("project", "memory", 1).is_err(), "an unpinned retired snapshot cannot reopen");
    }

    #[test]
    fn interrupted_attempt_can_be_replaced_and_restart_keeps_complete_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("snapshots");
        {
            let catalog = Catalog::new(path.clone(), None);
            catalog.prepare("project", "memory", 1, 2, &["a.rs".into(), "b.rs".into()], &["a.rs".into()], &[]).unwrap();
            catalog.prepare("project", "memory", 1, 2, &["a.rs".into(), "b.rs".into()], &["b.rs".into()], &[]).unwrap();
            catalog.seal("project", "memory", 2).unwrap();
            catalog.prepare("project", "memory", 2, 3, &[], &["a.rs".into()], &[]).unwrap();
            catalog.seal("project", "memory", 3).unwrap();
            catalog.protect_for_gc("project", "memory", 3).unwrap();
        }
        let reopened = Catalog::new(path, None);
        let current = reopened.pin("project", "memory", 3).unwrap();
        assert!(current.contains("a.rs", 3) && current.contains("b.rs", 2));
        assert!(reopened.pin("project", "memory", 2).is_err());
    }

    #[test]
    fn delayed_collection_cannot_use_an_old_reader_to_sweep_a_newer_head() {
        let temporary = tempfile::tempdir().unwrap();
        let catalog = Catalog::new(temporary.path().join("snapshots"), None);
        catalog.prepare("project", "memory", 1, 2, &["stable.rs".into(), "hot.rs".into()],
            &["hot.rs".into()], &[]).unwrap();
        catalog.seal("project", "memory", 2).unwrap();
        let old_reader = catalog.pin("project", "memory", 2).unwrap();
        catalog.prepare("project", "memory", 2, 3, &[], &["hot.rs".into()], &[]).unwrap();
        catalog.seal("project", "memory", 3).unwrap();
        catalog.protect_for_gc("project", "memory", 3).unwrap();
        assert!(catalog.protect_for_gc("project", "memory", 2).is_err(),
            "holding an old read must not authorize collection with a stale head");
        let current = catalog.pin("project", "memory", 3).unwrap();
        assert!(current.contains("stable.rs", 1) && current.contains("hot.rs", 3));
        assert!(old_reader.contains("hot.rs", 2));
    }

    #[test]
    fn cached_ancestors_cannot_hide_an_excessive_persisted_chain() {
        let temporary = tempfile::tempdir().unwrap();
        let catalog = Catalog::new(temporary.path().join("snapshots"), None);
        for generation in 2..=33 {
            catalog.prepare("project", "memory", generation - 1, generation,
                &["hot.rs".into(), "stable.rs".into()], &["hot.rs".into()], &[]).unwrap();
            catalog.seal("project", "memory", generation).unwrap();
        }
        let previous = catalog.pin("project", "memory", 33).unwrap();
        // Simulate an invalid persisted delta where a checkpoint was required.
        let malformed = serde_json::json!({
            "format": 1, "project": "project", "namespace": "memory",
            "generation": 34, "parent": 33, "pending": false,
            "files": null, "changes": {"hot.rs": 34}
        });
        std::fs::write(catalog.scope("project", "memory").join("34.json"),
            serde_json::to_vec(&malformed).unwrap()).unwrap();
        assert!(catalog.pin("project", "memory", 34).is_err(),
            "a cached parent must not let corrupt ancestry bypass the depth bound");
        assert!(previous.contains("stable.rs", 1) && previous.contains("hot.rs", 33),
            "rejecting corrupt metadata must preserve the complete earlier view");
    }

    #[test]
    fn many_edits_checkpoint_metadata_and_retain_untouched_file_revisions() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("snapshots");
        {
            let catalog = Catalog::new(path.clone(), None);
            for generation in 2..100 {
                catalog.prepare("project", "memory", generation - 1, generation,
                    &["hot.rs".into(), "stable.rs".into()], &["hot.rs".into()], &[]).unwrap();
                catalog.seal("project", "memory", generation).unwrap();
                catalog.protect_for_gc("project", "memory", generation).unwrap();
            }
        }
        let reopened = Catalog::new(path, None);
        let snapshot = reopened.pin("project", "memory", 99).unwrap();
        assert!(snapshot.contains("hot.rs", 99) && snapshot.contains("stable.rs", 1));
        assert!(std::fs::read_dir(reopened.scope("project", "memory")).unwrap().count() <= MAX_DEPTH + 2,
            "editor saves must not accumulate unbounded snapshot metadata files");
    }

    #[test]
    fn repeated_seals_without_gc_keep_bounded_retained_memory_and_real_held_selections() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let budget = Arc::new(MemoryBudget::new(1024 * 1024));
        let result = (|| -> anyhow::Result<()> {
            let catalog = Catalog::new(temporary.path().join("snapshots"), Some(budget.clone()));
            let paths = (0..128).map(|index| format!("src/module_{index:03}/source.rs")).collect::<Vec<_>>();
            let changed = vec![paths[0].clone()];
            catalog.prepare("project", "memory", 1, 2, &paths, &changed, &[])?;
            catalog.seal("project", "memory", 2)?;
            let held = catalog.pin_fresh("project", "memory", 2)?;
            let mut steady_charge = None;
            // No GC runs: failed publication attempts and delayed maintenance
            // must not exhaust this small real admission budget as saves grow.
            for generation in 3..=130 {
                catalog.prepare("project", "memory", generation - 1, generation, &[], &changed, &[])?;
                catalog.seal("project", "memory", generation)?;
                let current = catalog.pin_fresh("project", "memory", generation)?;
                anyhow::ensure!(current.contains(&paths[0], generation)
                    && paths[1..].iter().all(|path| current.contains(path, 1)),
                    "A complete current selection was lost while bounded admission continued");
                anyhow::ensure!(held.contains(&paths[0], 2)
                    && paths[1..].iter().all(|path| held.contains(path, 1)),
                    "A genuine held old selection was evicted or rewritten");
                drop(current);
                if generation == 48 { steady_charge = Some(budget.used()); }
                if let Some(steady) = steady_charge {
                    // Allow checkpoint/depth and decimal-length fluctuations;
                    // retained charges must plateau, not grow with every seal.
                    anyhow::ensure!(budget.used() <= steady + 64 * 1024,
                        "Retained source-view reservations keep growing without GC");
                }
            }
            drop(catalog);
            anyhow::ensure!(budget.used() > 0 && held.contains(&paths[0], 2),
                "The actual held view must own its reservation after the catalog closes");
            drop(held);
            anyhow::ensure!(budget.used() == 0, "Closed source views retained admission reservations");
            Ok(())
        })();
        let cleanup = temporary.close();
        result?;
        cleanup?;
        Ok(())
    }

    #[test]
    fn cached_managed_zero_floor_cannot_recreate_legacy_authority_after_scope_metadata_loss() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let result = (|| -> anyhow::Result<()> {
            let root = temporary.path().join("snapshots");
            let catalog = Catalog::new(root.clone(), None);
            let paths = vec!["hot.rs".to_owned(), "stable.rs".to_owned()];
            let changed = vec![paths[0].clone()];
            catalog.prepare("project", "memory", 1, 2, &paths, &changed, &[])?;
            catalog.seal("project", "memory", 2)?;
            let held = catalog.pin_fresh("project", "memory", 2)?;
            catalog.prepare("project", "memory", 2, 3, &[], &changed, &[])?;
            catalog.seal("project", "memory", 3)?;
            let current = catalog.pin_fresh("project", "memory", 3)?;
            anyhow::ensure!(catalog.retirement_boundary("project", "memory")? == Some(0));
            let scope = catalog.scope("project", "memory");
            // Only this fixture's exact owned metadata is removed. No GC or
            // physical payload mutation is involved in this authority case.
            for entry in std::fs::read_dir(&scope)? { std::fs::remove_file(entry?.path())?; }
            anyhow::ensure!(catalog.prepare("project", "memory", 3, 4, &[], &changed, &[]).is_err(),
                "Cached managed views at floor zero were mistaken for a new legacy scope");
            anyhow::ensure!(std::fs::read_dir(&scope)?.next().is_none(),
                "Refusal wrote false legacy authority after managed metadata disappeared");
            anyhow::ensure!(held.contains("hot.rs", 2) && held.contains("stable.rs", 1)
                && current.contains("hot.rs", 3) && current.contains("stable.rs", 1));
            // A parentless rebuild may recover from its known published base,
            // while that still-current complete cached selection stays usable.
            catalog.prepare_full("project", "memory", 3, 4, &paths)?;
            let before_publication = catalog.pin_fresh("project", "memory", 3)?;
            anyhow::ensure!(before_publication.contains("hot.rs", 3) && before_publication.contains("stable.rs", 1));
            drop(before_publication);
            catalog.seal("project", "memory", 4)?;
            anyhow::ensure!(catalog.pin("project", "memory", 2)?.contains("stable.rs", 1),
                "Reconstruction lost the real held old source group");
            drop(current); drop(held); drop(catalog);
            let cold = Catalog::new(root, None);
            let repaired = cold.pin_fresh("project", "memory", 4)?;
            anyhow::ensure!(paths.iter().all(|path| repaired.contains(path, 4)),
                "Cold reconstruction lost the complete parentless source repair");
            anyhow::ensure!(cold.pin_fresh("project", "memory", 2).is_err(),
                "Recovery resurrected an unknown old logical source root");
            drop(repaired); drop(cold);
            Ok(())
        })();
        let cleanup = temporary.close();
        result?;
        cleanup?;
        Ok(())
    }

}

#[cfg(test)]
mod bound_layout_tests {
    use super::*;
    use engram_core::{ProjectRecord, Registry};

    #[test]
    fn bound_source_fresh_pins_follow_publication_and_registry_lifetime_while_real_held_views_finish() -> anyhow::Result<()> {
        let owned = tempfile::tempdir()?;
        let result = (|| -> anyhow::Result<()> {
            let path = owned.path().join("registry.redb");
            let registry = Arc::new(Registry::open(&path)?);
            registry.put_project(&ProjectRecord { project_id: "project".into(), project_name: "Owned source authority".into(),
                project_type: "general".into(), directory: owned.path().join("source").to_string_lossy().into_owned(),
                created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None })?;
            // This fixture is the producer of an explicitly complete legacy
            // selection. The attestation is not inferred from metadata loss.
            let base = registry.capture_source_head("project")?;
            anyhow::ensure!(registry.attest_legacy_source_head(&base)?);
            let catalog = Catalog::new(owned.path().join("snapshots"), None);
            catalog.bind_source_registry(&registry)?;
            let legacy = catalog.pin_fresh("project", "memory", 1)?;
            catalog.prepare_full("project", "memory", 1, 2, &["source.rs".into()])?;
            catalog.seal("project", "memory", 2)?;
            anyhow::ensure!(catalog.pin_fresh("project", "memory", 1)?.contains("source.rs", 1),
                "Preparing the first managed view retired the still-published genuine legacy view");
            anyhow::ensure!(registry.publish_source_head(&base, 2)?);
            anyhow::ensure!(catalog.pin_fresh("project", "memory", 1).is_err(),
                "A cached Legacy view granted a new source read after managed publication");
            let current = catalog.pin_fresh("project", "memory", 2)?;
            drop(registry);
            anyhow::ensure!(catalog.pin_fresh("project", "memory", 2).is_err(),
                "Expired bound Registry authority silently turned into an unbound source pin");
            anyhow::ensure!(catalog.pin("project", "memory", 1)?.contains("source.rs", 1)
                && catalog.pin("project", "memory", 2)?.contains("source.rs", 2),
                "Real held source readers could not finish after Registry teardown");
            // A catalog-only lease must not retain the Registry database lock.
            let reopened = Arc::new(Registry::open(&path)?);
            catalog.bind_source_registry(&reopened)?;
            anyhow::ensure!(catalog.pin_fresh("project", "memory", 2)?.contains("source.rs", 2));
            reopened.delete_project("project")?;
            anyhow::ensure!(catalog.pin_fresh("project", "memory", 2).is_err(),
                "A new source reader survived deletion of its registered project");
            anyhow::ensure!(catalog.pin("project", "memory", 2)?.contains("source.rs", 2));
            drop(legacy); drop(current); drop(catalog); drop(reopened);
            Ok(())
        })();
        let cleanup = owned.close(); result?; cleanup?; Ok(())
    }
}
