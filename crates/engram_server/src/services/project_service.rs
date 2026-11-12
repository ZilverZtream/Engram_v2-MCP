use crate::error::EngramError;
use crate::models::ProjectType;
use crate::state::{AppState, ProjectInfo, ProjectState};
use engram_core::ProjectRecord;
use std::path::{Path, PathBuf};

/// Detection uses the same eligible-file walk as ingestion, without invoking
/// language sidecars before choosing a preset. Explicit profiles bypass it.
pub async fn detect_indexing_preset(state: &AppState, root: &Path) -> anyhow::Result<ProjectType> {
    let exts = crate::utils::files::exts_for_project_type_enum(ProjectType::Auto);
    let files = scan_source_files_with_parser_preflight(state, root, &exts, false).await?;
    let paths = files.iter().filter_map(|path| path.strip_prefix(root).ok())
        .map(|path| path.to_string_lossy().replace('\\', "/")).collect::<Vec<_>>();
    let candidate = crate::capabilities::project::ProjectCapabilities::from_paths(paths.iter().map(String::as_str)).indexing_preset();
    let selected = crate::utils::files::exts_for_project_type_enum(candidate);
    // An otherwise single-ecosystem repository may also contain SQL, templates,
    // native headers or other admitted assets. Do not silently drop them when
    // narrowing the automatic preset. Explicit caller profiles still narrow.
    let complete = files.iter().all(|path| path.extension().and_then(|ext| ext.to_str())
        .is_some_and(|ext| selected.iter().any(|allowed| ext.eq_ignore_ascii_case(allowed))));
    Ok(if complete { candidate } else { ProjectType::General })
}

/// Capabilities are derived from the held, persisted source manifest, including
/// zero-chunk files. Unpublished graph/disk changes cannot alter this report.
pub fn selected_project_capabilities(
    profile: &str,
    generation: u64,
    view: &engram_index::SourceSnapshotGuard,
) -> (crate::capabilities::project::ProjectCapabilities, serde_json::Value) {
    use crate::capabilities::project::ProjectCapabilities;
    let files = view.selected_file_revisions();
    let detected = files.map(|files| ProjectCapabilities::from_paths(files.keys().map(String::as_str)));
    let mut effective = detected.clone().unwrap_or_default();
    effective.include_explicit_profile(profile);
    let metadata = serde_json::json!({
        "format": "engram-project-capabilities-v1",
        "source_generation": generation,
        "indexing_preset": profile,
        "preset_selection": "Fixed at registration; auto selects from eligible manifests and source paths. Explicit profiles are honored.",
        "coverage": if files.is_some() { "selected_source_paths" } else { "unknown_legacy_source_manifest" },
        "scope": "Configured indexing preset and ignore rules; file-name evidence, not build evaluation or working-tree freshness.",
        "detected": detected,
        "available": { "webforms":effective.webforms,"classic_asp":effective.classic_asp,
            "dotnet":effective.dotnet,"visual_basic":effective.visual_basic,"javascript":effective.javascript,
            "modern_state":effective.modern_state,"react_redux_state":effective.react_redux_state },
        "visibility": "Union across registered source projects, intersected with the configured tool profile; explicit WebForms profiles retain access. Unknown source coverage preserves specialized discovery."
    });
    (effective, metadata)
}

/// Ensure a project record exists in the registry. Returns `EngramError::ProjectNotFound` if missing.
pub async fn ensure_project_record(
    state: &AppState,
    project_id: &str,
) -> Result<ProjectRecord, EngramError> {
    validate_project_id(project_id)?;
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid)).await??;
    if let Some(rec) = rec {
        return Ok(rec);
    }
    // The reserved user-memory project is created on first use: it has no
    // source directory to index, just a place to hold cross-project notes.
    if project_id == engram_core::namespaces::USER_PROJECT_ID {
        let now = crate::utils::now_ms();
        let record = ProjectRecord {
            project_id: project_id.to_string(),
            project_name: "User Memory".to_string(),
            project_type: "general".to_string(),
            directory: String::new(),
            created_at_ms: now,
            updated_at_ms: now,
            reindex_required_since_ms: None,
        };
        let reg = state.registry.clone();
        let to_write = record.clone();
        tokio::task::spawn_blocking(move || reg.put_project(&to_write)).await??;
        return Ok(record);
    }
    Err(EngramError::ProjectNotFound(project_id.to_string()))
}

/// Validate that `project_id` contains only safe characters to prevent directory traversal.
/// Allowed: ASCII alphanumerics, hyphens, underscores.
pub fn validate_project_id(project_id: &str) -> Result<(), EngramError> {
    // Hard upper bound prevents path/accounting amplification attacks and accidental
    // oversized IDs that can exceed filesystem or DB index limits.
    if project_id.len() > 128 {
        return Err(EngramError::InvalidParams(
            "project_id must be at most 128 characters".into(),
        ));
    }

    if project_id.is_empty()
        || !project_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(EngramError::InvalidParams(
            "project_id must contain only alphanumeric characters, hyphens, or underscores".into(),
        ));
    }
    Ok(())
}

/// Open (or cache-hit) a project's search engine runtime.
pub async fn ensure_project_runtime(
    state: &AppState,
    project_id: &str,
) -> Result<ProjectState, EngramError> {
    validate_project_id(project_id)?;

    // Deletion owns this same lane through cache eviction and physical cleanup.
    // Cache hits must not hand out a fresh owner after deletion admission.
    let _initializing = state.acquire_project_runtime_lock(project_id).await;
    if let Some(p) = state.get_project_cached(project_id) {
        p.search.bind_source_registry(&state.registry)?;
        return Ok(p);
    }

    let mut rec = ensure_project_record(state, project_id).await?;
    if let Some(project_type) = ProjectType::from_registry_str(&rec.project_type) {
        let canonical = project_type.as_str();
        if rec.project_type != canonical {
            rec.project_type = canonical.to_string();
            let reg = state.registry.clone();
            let rec_for_write = rec.clone();
            tokio::task::spawn_blocking(move || reg.put_project(&rec_for_write)).await??;
        }
    }
    let project_root = state.cfg.data_dir.join("projects").join(project_id);
    let tantivy_dir = project_root.join("tantivy");
    let lancedb_dir = project_root.join("lancedb");
    tokio::fs::create_dir_all(&tantivy_dir)
        .await
        .map_err(|e| EngramError::Internal(format!("failed to create tantivy dir: {e}")))?;
    tokio::fs::create_dir_all(&lancedb_dir)
        .await
        .map_err(|e| EngramError::Internal(format!("failed to create lancedb dir: {e}")))?;

    let search = engram_index::HybridSearchEngine::new_with_budget(
        tantivy_dir.clone(),
        lancedb_dir.clone(),
        &state.cfg,
        Some(state.memory_budget.clone()),
    )
    .await?;

    search.bind_source_registry(&state.registry)?;
    let ps = ProjectState {
            pagerank_cache: Default::default(),
        info: ProjectInfo {
            project_id: project_id.to_string(),
            project_name: rec.project_name,
            project_type: rec.project_type,
            directory: rec.directory,
            tantivy_dir,
            lancedb_dir,
        },
        search: std::sync::Arc::new(search),
    };
    state.put_project_cached(ps.clone()).await;
    Ok(ps)
}

/// Start a fresh source read, including direct service callers with a cached
/// injected engine. Binding itself never pins source or retains the Registry.
pub(crate) fn pin_source_generation(
    state: &AppState, search: &engram_index::HybridSearchEngine, project_id: &str, generation: u64,
) -> anyhow::Result<engram_index::SourceSnapshotGuard> {
    search.bind_source_registry(&state.registry)?;
    search.pin_source_generation(project_id, generation)
}

/// Get the current active generation for a project.
pub async fn get_active_generation(state: &AppState, project_id: &str) -> Result<u64, EngramError> {
    validate_project_id(project_id)?;
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let (generation, legacy_attested) = tokio::task::spawn_blocking(move || {
        let generation = reg.source_generation_metadata(&pid)?;
        let legacy_attested = generation.is_none() && reg.legacy_source_head_attested(&pid)?;
        Ok::<_, anyhow::Error>((generation, legacy_attested))
    })
    .await??;
    if let Some(generation) = generation {
        return Ok(generation);
    }
    let exists = if let Some(project) = state.get_project_cached(project_id) {
        project.search.has_managed_source_snapshots(project_id)?
    } else {
        let tantivy = state
            .cfg
            .data_dir
            .join("projects")
            .join(project_id)
            .join("tantivy");
        engram_index::HybridSearchEngine::source_snapshot_directory_exists(&tantivy, project_id)?
    };
    if exists && !legacy_attested {
        return Err(EngramError::Internal("Source generation metadata is absent for an existing snapshot store without a legacy absence proof; explicit source recovery is required".into()));
    }
    Ok(1)
}

/// Generate a human-readable indexing report from ingest stats.
pub fn generate_indexing_report(stats: &engram_index::IngestStats) -> String {
    let mut report = String::new();
    report.push_str("# Indexing Report\n\n");

    report.push_str("## Summary\n");
    report.push_str(&format!("- Total files found: {}\n", stats.files));
    // `all_files` includes undecodable files on purpose (they carry
    // fingerprint nodes so the incremental scan stops re-reading them
    // forever); "indexed" for the reader means content actually ingested.
    report.push_str(&format!(
        "- Files indexed: {}\n",
        stats
            .all_files
            .len()
            .saturating_sub(stats.skipped_files.len())
    ));
    report.push_str(&format!("- Files skipped: {}\n", stats.skipped_files.len()));
    report.push_str(&format!("- Total chunks created: {}\n", stats.chunks));
    if let Some(warning) = stats.embedding_warning() {
        report.push_str(&format!("- status: degraded\n- {warning}\n"));
    }
    report.push_str(&format!(
        "- Total bytes processed: {} ({:.2} MB)\n",
        stats.bytes,
        stats.bytes as f64 / 1024.0 / 1024.0
    ));

    if !stats.languages.is_empty() {
        report.push_str("\n## Languages Detected\n");
        let mut langs: Vec<_> = stats.languages.iter().collect();
        langs.sort_by(|a, b| b.1.cmp(a.1));
        for (lang, count) in langs {
            report.push_str(&format!("- {}: {}\n", lang, count));
        }
    }

    report.push_str("\n## Graph Stats\n");
    let mut node_kinds = std::collections::HashMap::new();
    for (_, sym) in &stats.symbols {
        *node_kinds.entry(sym.kind.clone()).or_insert(0) += 1;
    }
    report.push_str("- Nodes by kind:\n");
    let mut kinds: Vec<_> = node_kinds.iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(a.1));
    for (kind, count) in kinds {
        report.push_str(&format!("  - {}: {}\n", kind, count));
    }

    let mut edge_kinds = std::collections::HashMap::new();
    for (_, edge) in &stats.edges {
        *edge_kinds.entry(edge.kind.clone()).or_insert(0) += 1;
    }
    report.push_str("- Edges by kind:\n");
    let mut ekinds: Vec<_> = edge_kinds.iter().collect();
    ekinds.sort_by(|a, b| b.1.cmp(a.1));
    for (kind, count) in ekinds {
        report.push_str(&format!("  - {}: {}\n", kind, count));
    }

    let observed_runtime_total: usize = edge_kinds
        .iter()
        .filter(|(kind, _)| {
            let k = kind.to_string();
            k.contains("observed_runtime")
        })
        .map(|(_, count)| *count)
        .sum();
    if observed_runtime_total > 0 {
        report.push_str(&format!(
            "- Runtime-observed edges: {} 🟢 observed at runtime\n",
            observed_runtime_total
        ));
    }

    if !stats.skipped_files.is_empty() {
        report.push_str("\n## Skipped Files\n");
        for (path, reason) in stats.skipped_files.iter().take(50) {
            report.push_str(&format!("- {}: {}\n", path, reason));
        }
        if stats.skipped_files.len() > 50 {
            report.push_str(&format!(
                "... and {} more\n",
                stats.skipped_files.len() - 50
            ));
        }
    }

    if !stats.warnings.is_empty() {
        report.push_str("\n## Warnings\n");
        for warn in &stats.warnings {
            report.push_str(&format!("- {}\n", warn));
        }
    }

    report
}

/// P0-6: whether a stored (mtime, size) pair is a trustworthy "unchanged"
/// signal on its own. Zero values mean the metadata was never recorded (or
/// the platform couldn't produce it), so a stat match proves nothing and the
/// caller must fall back to content hashing.
pub(crate) fn stat_match_is_trustworthy(stored_mtime: u64, stored_size: u64) -> bool {
    stored_mtime != 0 && stored_size != 0
}

/// Whether a file node's metadata carries the indexer's fingerprint
/// (`file_hash` written alongside `mtime`/`size` by `process_ingest_stats`).
pub(crate) fn meta_has_fingerprint(meta: &Option<serde_json::Value>) -> bool {
    meta.as_ref()
        .map(|v| v.get("file_hash").is_some())
        .unwrap_or(false)
}

/// Basic configured chunk contract only. Full source compatibility additionally
/// needs SourceExtractionExpectation::metadata_is_compatible with the file path
/// and actual current VB parser authority, captured once off the async executor.
pub fn source_metadata_is_compatible(
    metadata: &serde_json::Value,
    target_chars_per_chunk: usize,
) -> bool {
    let Some(identity) = metadata
        .get("source_extraction_identity")
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    metadata.get("source_index_version").and_then(|value| value.as_u64())
        == Some(engram_index::SOURCE_INDEX_VERSION)
        && identity.len() == 2
        && identity.get("source_index_version").and_then(|value| value.as_u64())
            == Some(engram_index::SOURCE_INDEX_VERSION)
        && identity.get("target_chars_per_chunk").and_then(|value| value.as_u64())
            .and_then(|value| usize::try_from(value).ok()) == Some(target_chars_per_chunk)
}

/// Compute incremental changes between on-disk files and graph-stored file metadata.
pub async fn outdated_source_index_paths(
    state: &AppState,
    project_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(source_index_format_coverage(state, project_id).await?.1)
}

pub async fn source_index_format_coverage(
    state: &AppState,
    project_id: &str,
) -> anyhow::Result<(usize, Vec<String>)> {
    let active_generation = get_active_generation(state, project_id).await?;
    source_index_format_coverage_for_generation(state, project_id, active_generation).await
}

pub async fn source_index_format_coverage_for_generation(
    state: &AppState,
    project_id: &str,
    active_generation: u64,
) -> anyhow::Result<(usize, Vec<String>)> {
    let observation = observe_source_file_metadata(state, project_id).await;
    let rec = ensure_project_record(state, project_id).await?;
    source_index_format_coverage_from_observation(state, Path::new(&rec.directory), active_generation, &observation).await
}

/// Request-local live graph observation. Share the raw, validated rows even if
/// parser provenance checking fails; this is not a cached completeness verdict
/// or a historical graph snapshot for the selected source generation.
pub(crate) type SourceFileMetadataObservation = Result<
    std::sync::Arc<Vec<(engram_core::RelPath, Option<serde_json::Value>, u64)>>,
    String,
>;

pub(crate) async fn observe_source_file_metadata(
    state: &AppState,
    project_id: &str,
) -> SourceFileMetadataObservation {
    let graph = state.graph.clone();
    let pid = project_id.to_string();
    match tokio::task::spawn_blocking(move || graph.list_file_node_metadata_with_generation(&pid)).await {
        Ok(Ok(rows)) => Ok(std::sync::Arc::new(rows)),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) async fn source_index_format_coverage_from_observation(
    state: &AppState,
    root: &Path,
    active_generation: u64,
    observation: &SourceFileMetadataObservation,
) -> anyhow::Result<(usize, Vec<String>)> {
    let rows = observation.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?.clone();
    let target_chars_per_chunk = state.cfg.max_chunks_per_file;
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut paths = std::collections::BTreeMap::<String, (bool, bool)>::new();
        let expected = engram_index::SourceExtractionExpectation::for_project(
            &root, target_chars_per_chunk, rows.iter().map(|(path, _, _)| path.as_str()),
        )?;
        for (path, metadata, generation) in rows.iter() {
            let current = metadata.as_ref().is_some_and(|m| {
                expected.metadata_is_compatible(m, path.as_str())
            });
            // Legacy shadow file nodes must not hide the canonical file's marker.
            let entry = paths.entry(path.as_str().to_owned()).or_default();
            entry.0 |= current;
            entry.1 |= *generation > active_generation;
        }
        Ok((
            paths.len(),
            paths
                .into_iter()
                .filter_map(|(p, (current, pending))| (!current || pending).then_some(p))
                .collect(),
        ))
    })
    .await?
}

/// A source writer needs a complete walk, including a valid directory root.
/// Keep filesystem traversal off the async executor and enforce configured
/// collection limits before allocating the complete project's path list.
pub async fn scan_source_files(
    state: &AppState,
    root: &Path,
    exts: &[&str],
) -> anyhow::Result<Vec<PathBuf>> {
    scan_source_files_with_parser_preflight(state, root, exts, true).await
}

async fn scan_source_files_with_parser_preflight(
    state: &AppState,
    root: &Path,
    exts: &[&str],
    preflight: bool,
) -> anyhow::Result<Vec<PathBuf>> {
    let root = root.to_path_buf();
    let exts = exts.iter().map(|ext| ext.to_string()).collect::<Vec<_>>();
    let max_files = state.cfg.max_project_files;
    let max_bytes = state.cfg.max_project_bytes;
    let source_chunk_target = state.cfg.max_chunks_per_file;
    let data_dir = state.cfg.data_dir.clone();
    tokio::task::spawn_blocking(move || {
        let canonical_root = root.canonicalize()?;
        let canonical_storage = data_dir.canonicalize()?;
        anyhow::ensure!(!canonical_root.starts_with(&canonical_storage),
            "Source root cannot be Engram's runtime storage or a directory inside it");
        let excluded = canonical_storage.strip_prefix(&canonical_root).ok()
            .map(|relative| vec![relative.to_path_buf()]).unwrap_or_default();
        let refs = exts.iter().map(String::as_str).collect::<Vec<_>>();
        let files = engram_index::ingest::iter_files_checked_excluding(&root, &refs, max_files, max_bytes, &excluded)?;
        // Preflight unknown old-protocol parsers before source staging. No
        // registry flag/revision is written by read-only walks or health calls.
        if preflight {
            let relative = files.iter().filter_map(|path| engram_core::RelPath::from_relative(&root, path))
                .map(|path| path.as_str().to_owned()).collect::<Vec<_>>();
            engram_index::SourceExtractionExpectation::for_project(&root, source_chunk_target, relative.iter().map(String::as_str))?;
        }
        Ok(files)
    })
    .await?
}

/// Read-only change detection. Health requests never persist stat observations.
pub async fn get_incremental_changes(
    state: &AppState,
    project_id: &str,
    root: &Path,
    exts: &[&str],
) -> anyhow::Result<(Vec<PathBuf>, Vec<engram_core::RelPath>)> {
    let scan = scan_incremental_changes(state, project_id, root, exts, None).await?;
    Ok((scan.changed, scan.deleted))
}

pub(crate) struct SourceChangeScan {
    pub changed: Vec<PathBuf>,
    pub deleted: Vec<engram_core::RelPath>,
    pub extraction: engram_index::SourceExtractionExpectation,
    pub vb_source_files: std::sync::Arc<Vec<PathBuf>>,
}

/// Preserve the source walk's admitted VB inventory without another walk or
/// deriving it from chunk rows (which omit empty/zero-symbol sources).
pub(crate) fn admitted_vb_source_files(root: &Path, files: &[PathBuf]) -> std::sync::Arc<Vec<PathBuf>> {
    std::sync::Arc::new(files.iter().filter(|path| engram_core::RelPath::from_relative(root, path)
        .is_some_and(|rel| engram_index::vb_extractor::requires_parser_provenance(rel.as_str())))
        .cloned().collect())
}

/// Full indexing/repair captures one expectation for the complete admitted
/// file set before staging and carries it to produced-output verification/CAS.
pub(crate) async fn capture_source_extraction_for_files(
    state: &AppState, root: &Path, files: &[PathBuf],
) -> anyhow::Result<engram_index::SourceExtractionExpectation> {
    let root = root.to_owned();
    let files = files.to_vec();
    let target = state.cfg.max_chunks_per_file;
    tokio::task::spawn_blocking(move || {
        let paths = files.iter().map(|path| engram_core::RelPath::from_relative(&root, path)
            .ok_or_else(|| anyhow::anyhow!("Source file outside admitted root: {}", path.display())))
            .collect::<anyhow::Result<Vec<_>>>()?;
        engram_index::SourceExtractionExpectation::for_project(&root, target, paths.iter().map(|path| path.as_str()))
    }).await?
}

/// An admitted source update may warm only observations of already-published
/// file revisions. A failed later update does not certify new source via them.
pub(crate) async fn get_incremental_changes_for_update(
    state: &AppState,
    search: &engram_index::HybridSearchEngine,
    base: &engram_core::registry::SourceHeadToken,
    root: &Path,
    exts: &[&str],
) -> anyhow::Result<SourceChangeScan> {
    let admission = search.begin_project_mutation(base.project_id())?;
    let scoped = search.with_mutation(&admission)?;
    // Refuse unavailable/unproven source BEFORE no-op/stat-cache observations
    // and before any legacy physical-path enumeration or staging.
    anyhow::ensure!(state.registry.source_head_is_current(base)?, "Source head changed before source admission");
    let source_snapshot = pin_source_generation(state, &scoped, base.project_id(), base.generation())?;
    let writer = StatRefreshWriter {
        _source_snapshot: source_snapshot,
        base: base.clone(), registry: state.registry.clone(), graph: state.graph.clone(),
        _operation: scoped.begin_operation()?,
    };
    scan_incremental_changes(state, base.project_id(), root, exts, Some(writer)).await
}

struct StatRefreshWriter {
    _source_snapshot: engram_index::SourceSnapshotGuard,
    base: engram_core::registry::SourceHeadToken,
    registry: std::sync::Arc<engram_core::Registry>,
    graph: std::sync::Arc<engram_graph::GraphStore>,
    // Last: the blocking worker owns admission through actual cache commits,
    // including when its requesting future is cancelled.
    _operation: engram_index::hybrid::IndexOperationGuard,
}

impl StatRefreshWriter {
    fn flush(&self, patches: &mut Vec<engram_graph::FileStatPatch>) -> anyhow::Result<()> {
        anyhow::ensure!(self.registry.source_head_is_current(&self.base)?,
            "Source head or identity changed during file verification; retry");
        if !patches.is_empty() {
            let report = self.graph.patch_verified_file_stats(self.base.project_id(), patches)?;
            if report.changed + report.missing + report.deferred > 0 {
                tracing::warn!(project_id = %self.base.project_id(), ?report,
                    "Some verified file-stat hints were deferred; later updates will reverify them");
            }
            patches.clear();
        }
        Ok(())
    }
}

fn modified_ns(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
}

struct VerifiedSourceBytes {
    hash: String,
    mtime_ns: Option<u64>,
    size: u64,
}

/// Hash a bounded stream from one handle and retain only the stamp actually
/// observed around those bytes. Unknown/changed observations never warm hints.
fn verify_source_bytes(path: &Path, initial: &std::fs::Metadata) -> anyhow::Result<Option<VerifiedSourceBytes>> {
    use std::io::Read;
    const MAX_VERIFY_BYTES: u64 = 100_000_000;
    if initial.len() > MAX_VERIFY_BYTES { return Ok(None); }
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("Cannot verify source content {}: {e}", path.display()))?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() != initial.len() || modified_ns(&opened) != modified_ns(initial) {
        return Ok(None);
    }
    let mut hasher = blake3::Hasher::new();
    let mut limited = (&file).take(MAX_VERIFY_BYTES + 1);
    let read = std::io::copy(&mut limited, &mut hasher)
        .map_err(|e| anyhow::anyhow!("Cannot verify source content {}: {e}", path.display()))?;
    let after = file.metadata()?;
    let path_after = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let stamp = modified_ns(&opened);
    if read != opened.len() || read > MAX_VERIFY_BYTES || after.len() != read || path_after.len() != read
        || modified_ns(&after) != stamp || modified_ns(&path_after) != stamp {
        return Ok(None);
    }
    Ok(Some(VerifiedSourceBytes { hash: hasher.finalize().to_hex().to_string(), mtime_ns: stamp, size: read }))
}

/// Rebuild-only publication must still describe exactly the indexed bytes.
/// Reuse the bounded content verifier; no stat cache or source writer is opened.
pub(crate) fn verify_graph_rebuild_bytes(
    root: &Path,
    files: &[engram_graph::Node],
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    for file in files {
        anyhow::ensure!(!cancel.is_cancelled(), "Graph rebuild cancelled before publication");
        let path = engram_core::safe_join(root, file.file_path.as_str())?;
        let initial = std::fs::metadata(&path)?;
        let observed = verify_source_bytes(&path, &initial)?
            .ok_or_else(|| anyhow::anyhow!("Source changed while verifying {}; run update_project", file.file_path))?;
        let expected = file.metadata.as_ref().and_then(|m| m.get("file_hash")).and_then(|h| h.as_str());
        anyhow::ensure!(expected == Some(observed.hash.as_str()),
            "Source content changed for {}; run update_project before graph_only repair", file.file_path);
    }
    Ok(())
}

async fn scan_incremental_changes(
    state: &AppState,
    project_id: &str,
    root: &Path,
    exts: &[&str],
    writer: Option<StatRefreshWriter>,
) -> anyhow::Result<SourceChangeScan> {
    let active_generation = get_active_generation(state, project_id).await?;
    let opt_in_hash_verification = state.cfg.verify_unchanged_hashes;
    let target_chars_per_chunk = state.cfg.max_chunks_per_file;
    let disk_files = scan_source_files(state, root, exts).await?;
    let vb_source_files = admitted_vb_source_files(root, &disk_files);
    let graph = state.graph.clone();
    let registry = state.registry.clone();
    let pid = project_id.to_string();
    let (db_file_meta, (verify_source_identity, identity_epoch)) = tokio::task::spawn_blocking(move || {
        Ok::<_, anyhow::Error>((graph.list_file_node_metadata_with_generation(&pid)?,
            registry.source_stat_cache_state(&pid)?))
    }).await??;
    let verify_hashes = opt_in_hash_verification || verify_source_identity;
    let root_owned = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        // Move the owned operation into this worker explicitly; readonly calls
        // have no writer and perform no persistent mutation.
        let writer = writer;
        let expected = engram_index::SourceExtractionExpectation::for_project(&root_owned, target_chars_per_chunk,
            disk_files.iter().filter_map(|path| path.strip_prefix(&root_owned).ok()).filter_map(|path| path.to_str()))?;
        let mut patches = Vec::with_capacity(256);
        let mut patch_bytes = 0;
        if let Some(writer) = &writer { writer.flush(&mut patches)?; }
        let mut changed = Vec::new();
        let mut deleted = Vec::new();
        let mut db_map = std::collections::HashMap::<
            engram_core::RelPath, (Option<serde_json::Value>, u64)>::new();
        let mut unpublished_paths = std::collections::HashSet::new();
        for (path, metadata, generation) in db_file_meta {
            if generation > active_generation { unpublished_paths.insert(path.clone()); }
            match db_map.entry(path) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if !meta_has_fingerprint(&entry.get().0) && meta_has_fingerprint(&metadata) {
                        entry.insert((metadata, generation));
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => { entry.insert((metadata, generation)); }
            }
        }
        for path in disk_files {
            let relative = engram_core::RelPath::from_relative(&root_owned, &path)
                .ok_or_else(|| anyhow::anyhow!("Source file is outside its registered root: {}", path.display()))?;
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    db_map.remove(&relative); deleted.push(relative); continue;
                }
                Err(e) => anyhow::bail!("Cannot inspect source file {}: {e}", path.display()),
            };
            let size = metadata.len();
            let stamp = modified_ns(&metadata);
            let mut is_changed = true;
            if let Some((Some(meta), generation)) = db_map.remove(&relative) {
                if generation == 0 || unpublished_paths.contains(&relative)
                    || !expected.metadata_is_compatible(&meta, relative.as_str()) {
                    changed.push(path); continue;
                }
                let hash = meta.get("file_hash").and_then(|v| v.as_str())
                    .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
                let stored_size = meta.get("size").and_then(|v| v.as_u64());
                let hint = meta.get("verified_stat_v1")
                    .and_then(|v| serde_json::from_value::<engram_graph::VerifiedFileStat>(v.clone()).ok())
                    .filter(|hint| hint.version == 1 && Some(hint.identity_epoch) == identity_epoch
                        && hint.identity_epoch != 0 && hint.generation == generation
                        && Some(hint.file_hash.as_str()) == hash && Some(hint.size) == stored_size
                        && hint.mtime_ns != 0);
                let raw_stamp = meta.get("mtime_ns").and_then(|v| v.as_u64());
                let cached_stamp = hint.as_ref().map(|hint| hint.mtime_ns).or(raw_stamp);
                let stat_matches = stored_size == Some(size) && stamp.is_some() && cached_stamp == stamp;
                if hash.is_some() && stat_matches && !verify_hashes
                    && (hint.is_some() || stat_match_is_trustworthy(cached_stamp.unwrap_or(0), size)) {
                    is_changed = false;
                } else if stored_size == Some(size) && let Some(hash) = hash
                    && let Some(verified) = verify_source_bytes(&path, &metadata)?
                    && verified.hash == hash {
                    is_changed = false;
                    if let (Some(writer), Some(mtime_ns)) = (&writer, verified.mtime_ns) {
                        let observation = engram_graph::VerifiedFileStat {
                            version: 1, identity_epoch: writer.base.identity_epoch(), generation,
                            file_hash: verified.hash, mtime_ns, size: verified.size,
                        };
                        // Do not rewrite an already-current hint during opt-in
                        // hash verification or source identity settlement.
                        if hint.as_ref() != Some(&observation) {
                            let patch = engram_graph::FileStatPatch { path: relative.clone(), generation,
                                metadata_digest: engram_graph::file_metadata_digest(&meta)?, stat: observation };
                            let bytes = serde_json::to_vec(&patch)?.len();
                            if patches.len() == 256 || patch_bytes + bytes > 512 * 1024 {
                                writer.flush(&mut patches)?; patch_bytes = 0;
                            }
                            if bytes <= 512 * 1024 { patches.push(patch); patch_bytes += bytes; }
                            else { tracing::warn!(path = %relative.as_str(), "File-stat hint exceeds cache budget; left untrusted"); }
                        }
                    }
                }
            }
            if is_changed { changed.push(path); }
        }
        engram_index::ingest::validate_source_root(&root_owned)?;
        for (relative, _) in db_map { deleted.push(relative); }
        if let Some(writer) = &writer { writer.flush(&mut patches)?; }
        if !changed.is_empty() || !deleted.is_empty() {
            tracing::info!(changed = changed.len(), deleted = deleted.len(), "incremental change scan");
        }
        Ok::<_, anyhow::Error>(SourceChangeScan { changed, deleted, extraction: expected, vb_source_files })
    }).await?
}

/// Scoped repair. The worker owns the project lock and GC registration until
/// native work AND durable requirement settlement have finished, even if the
/// requesting tool disconnects.
pub async fn repair_project_scoped(
    state: &AppState,
    project_id: &str,
    scope: &str,
) -> anyhow::Result<String> {
    Ok(run_scoped_repair(state, project_id, scope, false)
        .await?
        .message)
}

/// Revalidate an integrity observation under the project update lock. A stale
/// hourly observation must not start another all-document embedding rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorRepairOutcome {
    Recovered {
        vectors: usize,
        warning_settled: bool,
    },
    NoCurrentCountMismatch,
    Disabled,
}

struct ScopedRepairReport {
    message: String,
    vector_outcome: Option<VectorRepairOutcome>,
}

pub async fn repair_vectors_if_mismatch(
    state: &AppState,
    project_id: &str,
) -> anyhow::Result<VectorRepairOutcome> {
    run_scoped_repair(state, project_id, "vector_only", true)
        .await?
        .vector_outcome
        .ok_or_else(|| anyhow::anyhow!("Vector repair returned no disposition"))
}

async fn run_scoped_repair(
    state: &AppState,
    project_id: &str,
    scope: &str,
    recheck_vectors: bool,
) -> anyhow::Result<ScopedRepairReport> {
    validate_project_id(project_id)?;
    anyhow::ensure!(
        matches!(
            scope,
            "vector_only" | "initialize_vectors" | "tantivy_only" | "graph_only"
        ),
        "Unknown repair scope: {scope}. Valid: tantivy_only, vector_only, graph_only, initialize_vectors"
    );
    // Cancellation while waiting starts no worker and holds no GC slot.
    let lock = state.acquire_project_update_lock(project_id).await;
    let active = crate::state::ActiveIndexingSlot::acquire(state);
    let state = state.clone();
    let project_id = project_id.to_owned();
    let scope = scope.to_owned();
    tokio::spawn(async move {
        let _lock = lock;
        let _active = active;
        let ps = ensure_project_runtime(&state, &project_id).await?;
        let generation = get_active_generation(&state, &project_id).await?;
        match scope.as_str() {
            "initialize_vectors" | "vector_only" => {
                if recheck_vectors {
                    if matches!(state.cfg.embedding_backend.as_str(), "fts_only" | "") {
                        return Ok(ScopedRepairReport { message: "Automatic vector recovery skipped: embeddings are disabled.".into(), vector_outcome: Some(VectorRepairOutcome::Disabled) });
                    }
                    let search = ps.search.clone();
                    let project = project_id.clone();
                    let lexical_count = tokio::task::spawn_blocking(move || search.count_docs(&project)).await?? as u64;
                    let vector_count = ps.search.count_vectors(&project_id).await? as u64;
                    if super::integrity_service::build_integrity_mismatches(lexical_count, vector_count, true).is_empty() {
                        return Ok(ScopedRepairReport { message: "No current vector count mismatch; no documents were re-embedded.".into(), vector_outcome: Some(VectorRepairOutcome::NoCurrentCountMismatch) });
                    }
                }
                let registry = state.registry.clone();
                let project = project_id.clone();
                let initialize = scope == "initialize_vectors";
                let requirement = tokio::task::spawn_blocking(move || {
                    // Persist before admitted work so provider failure or panic
                    // cannot leave a missing/incomplete table reported healthy.
                    registry.begin_reindex_repair(&project, engram_core::registry::ReindexReason::Vector, crate::utils::now_ms())
                }).await??;
                anyhow::ensure!(requirement.is_some(), "Project disappeared before vector repair admission; no recovery started");
                let cancel = tokio_util::sync::CancellationToken::new();
                // The service worker is owned. Do not drop this future on a
                // requester cancellation: the engine also owns native tasks.
                let result = if initialize {
                    ps.search.initialize_vectors(&project_id, &cancel).await
                } else {
                    ps.search.recover_vectors(&project_id, &cancel).await
                };
                let count = match result {
                    Ok(count) => count,
                    Err(error) => {
                        let refused = error.downcast_ref::<engram_index::hybrid::VectorInitializationRejection>().is_some();
                        if refused {
                            // Busy means no recovery was admitted. In particular,
                            // do not turn a normal history ingestion's transient
                            // lexical/vector lag into a permanent warning.
                            if let Some((token, true)) = requirement {
                                let registry = state.registry.clone();
                                tokio::task::spawn_blocking(move || registry.settle_reindex_requirement(&token)).await??;
                            }
                        }
                        return Err(error);
                    }
                };
                let settled = if let Some((token, _)) = requirement {
                    let registry = state.registry.clone();
                    tokio::task::spawn_blocking(move || registry.settle_reindex_requirement(&token)).await??
                } else { true };
                let verb = if initialize { "Initialized" } else { "Recovered" };
                let pending = if settled { "" } else { " A newer vector recovery requirement remains pending." };
                Ok(ScopedRepairReport { message: format!("{verb} {count} vectors from stored documents in every retained namespace. Text, history, knowledge and active generation {generation} were preserved.{pending}"),
                    vector_outcome: Some(VectorRepairOutcome::Recovered { vectors: count, warning_settled: settled }) })
            }
            "tantivy_only" => {
                ps.search.purge_old_generations(&project_id, generation).await?;
                Ok(ScopedRepairReport { message: format!("Purged eligible superseded search generations at source generation {generation}."), vector_outcome: None })
            }
            "graph_only" => {
                let graph = state.graph.clone();
                tokio::task::spawn_blocking(move || graph.delete_project_data(&project_id)).await??;
                Ok(ScopedRepairReport { message: format!("Graph purged for re-indexing at generation {generation}"), vector_outcome: None })
            }
            _ => unreachable!("scope validated before acquiring the project lock"),
        }
    }).await.map_err(|error| anyhow::anyhow!("Scoped repair worker did not complete: {error}"))?
}

/// Inject repo rules for a file path into the content header.
pub async fn inject_repo_rules(
    state: &AppState,
    project_id: &str,
    file_path: &engram_core::RelPath,
    content: &str,
) -> String {
    let rule_state = state.clone();
    let pid = project_id.to_string();
    let fp = file_path.as_str();
    let rule_path = fp.to_owned();
    let effective = tokio::task::spawn_blocking(move || {
        crate::services::quality_source::effective_repo_rules(
            &rule_state,
            &pid,
            Some(&[rule_path.as_str()]),
        )
    })
    .await;
    let (rules, notes) = match effective {
        Ok(Ok(effective)) => (effective.rules, effective.notes),
        _ => (
            Vec::new(),
            vec!["INCOMPLETE policy coverage: repository-rule lookup unavailable".into()],
        ),
    };

    // effective_repo_rules already applies the shared scope contract. A second,
    // narrower glob filter used to drop directory/case matches accepted by
    // planning and review, especially for new files.
    let applicable = rules;
    if applicable.is_empty() && notes.is_empty() {
        return content.to_string();
    }

    let mut header = String::new();
    for note in notes {
        header.push_str(&format!("[Policy coverage]: {note}\n"));
    }
    for r in applicable {
        header.push_str(&format!("[Repo Constraint]: {}\n", r.rule_text));
    }
    header.push('\n');
    header + content
}

#[cfg(test)]
mod p0_stat_trust_tests {
    use super::stat_match_is_trustworthy;

    /// P0-6: a recorded (mtime, size) pair is trustworthy on its own.
    #[test]
    fn nonzero_stat_pair_is_trustworthy() {
        assert!(stat_match_is_trustworthy(1_700_000_000, 4096));
    }

    /// P0-6: zero mtime means "never recorded" — must force a hash check,
    /// otherwise files indexed before stat tracking would never re-index.
    #[test]
    fn zero_mtime_is_not_trustworthy() {
        assert!(!stat_match_is_trustworthy(0, 4096));
    }

    /// P0-6: zero size is ambiguous (empty file vs missing metadata); fall
    /// back to hashing — hashing an empty file costs nothing anyway.
    #[test]
    fn zero_size_is_not_trustworthy() {
        assert!(!stat_match_is_trustworthy(1_700_000_000, 0));
    }

    /// A parse-status metadata blob (sidecar `sym:file:` shadow) must not
    /// count as a fingerprint; the real indexer blob must.
    #[test]
    fn meta_has_fingerprint_distinguishes_shadow_from_real() {
        use super::meta_has_fingerprint;
        let shadow = Some(serde_json::json!({
            "fqn": "file", "is_designer": "false",
            "parse_error_count": "0", "parse_success": "true"
        }));
        let real = Some(serde_json::json!({
            "mtime": 1_700_000_000u64, "size": 4096u64, "file_hash": "abc"
        }));
        assert!(!meta_has_fingerprint(&shadow));
        assert!(!meta_has_fingerprint(&None));
        assert!(meta_has_fingerprint(&real));
    }
}

/// Select explicit re-extraction only from the same ignore/extension-filtered
/// source set as normal indexing. Validate the whole request before mutation.
pub async fn include_reindex_paths(
    state: &AppState,
    root: &Path,
    exts: &[&str],
    requested: &[String],
    changed: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    if requested.is_empty() {
        return Ok(());
    }
    anyhow::ensure!(
        requested.len() <= 100,
        "reindex_paths accepts at most 100 files"
    );
    let allowed = scan_source_files(state, root, exts).await?;
    select_reindex_paths(root, &allowed, requested, changed)
}

fn select_reindex_paths(
    root: &Path,
    allowed: &[PathBuf],
    requested: &[String],
    changed: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let mut selected = Vec::new();
    for raw in requested {
        let normalized = raw.replace('\\', "/");
        anyhow::ensure!(
            !normalized.is_empty()
                && !normalized.starts_with('/')
                && !normalized.contains(':')
                && normalized
                    .split('/')
                    .all(|p| !p.is_empty() && p != "." && p != ".."),
            "reindex_paths requires exact project-relative paths: {raw}"
        );
        let path = allowed
            .iter()
            .find(|p| {
                engram_core::RelPath::from_relative(root, p)
                    .is_some_and(|r| r.as_str() == normalized)
            })
            .ok_or_else(|| {
                anyhow::anyhow!("reindex path is missing, ignored, or unsupported: {raw}")
            })?;
        anyhow::ensure!(
            path.canonicalize()?.starts_with(root.canonicalize()?),
            "reindex path resolves outside project: {raw}"
        );
        selected.push(path.clone());
    }
    for path in selected {
        if !changed.contains(&path) {
            changed.push(path);
        }
    }
    Ok(())
}

/// Admit an exclusive source refresh using metadata only outside the selected
/// files. Do not certify unverified inheritance or adopt a foreign failed attempt.
pub(crate) async fn exclusive_source_changes(
    state: &AppState,
    search: &engram_index::HybridSearchEngine,
    base: &engram_core::registry::SourceHeadToken,
    root: &Path,
    exts: &[&str],
    requested: &[String],
) -> anyhow::Result<SourceChangeScan> {
    anyhow::ensure!(!requested.is_empty() && requested.len() <= 4096,
        "only_paths requires 1..4096 exact eligible paths");
    anyhow::ensure!(requested.iter().all(|p| p.len() <= 2048 && !p.chars().any(char::is_control)),
        "only_paths contains an oversized or control-character path");
    let source = pin_source_generation(state, search, base.project_id(), base.generation())?;
    let allowed = scan_source_files_with_parser_preflight(state, root, exts, false).await?;
    let mut changed = Vec::new();
    select_reindex_paths(root, &allowed, requested, &mut changed)?;
    let selected = changed.iter().filter_map(|p| engram_core::RelPath::from_relative(root, p))
        .collect::<std::collections::HashSet<_>>();
    // An explicit source subset still inherits the compiler context of every
    // admitted VB peer. Never mix new bindings with incompatible inherited ones.
    let extraction = capture_source_extraction_for_files(state, root, &allowed).await?;
    let inherited_extraction = extraction.clone();
    let graph = state.graph.clone();
    let registry = state.registry.clone();
    let token = base.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        anyhow::ensure!(registry.source_head_is_current(&token)?, "Source head changed before exclusive admission");
        anyhow::ensure!(!registry.source_identity_needs_verification(token.project_id())?,
            "only_paths requires an already verified source identity; run a normal verified update first");
        for (path, metadata, generation) in graph.list_file_node_metadata_with_generation(token.project_id())? {
            if selected.contains(&path) {
                anyhow::ensure!(generation <= token.generation().saturating_add(1),
                    "Selected graph revision is beyond the next source generation; recover before only_paths: {}", path.as_str());
            } else {
                anyhow::ensure!(generation <= token.generation()
                    && (generation != 0 || !meta_has_fingerprint(&metadata)),
                    "Unpublished or unproven outside source revision requires recovery before only_paths: {}", path.as_str());
                anyhow::ensure!(!engram_index::vb_extractor::requires_parser_provenance(path.as_str())
                    || metadata.as_ref().is_some_and(|meta|
                        inherited_extraction.metadata_is_compatible(meta, path.as_str())),
                    "Inherited VB source requires compiler-context migration before only_paths: {}; run ordinary update_project without only_paths first", path.as_str());
            }
        }
        Ok(())
    }).await??;
    let vb_source_files = admitted_vb_source_files(root, &allowed);
    drop(source);
    Ok(SourceChangeScan { changed, deleted:Vec::new(), extraction, vb_source_files })
}

#[cfg(test)]
mod reindex_tests {
    use super::*;
    #[test]
    fn explicit_reindex_deduplicates_and_rejects_invalid_requests_atomically() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("helper.vb");
        std::fs::write(&file, "Class C\nEnd Class").unwrap();
        let mut changed = Vec::new();
        select_reindex_paths(
            root.path(),
            &[file.clone()],
            &["helper.vb".into(), "helper.vb".into()],
            &mut changed,
        )
        .unwrap();
        assert_eq!(changed, vec![file.clone()]);
        for bad in [
            "../helper.vb",
            "C:/helper.vb",
            "/helper.vb",
            "missing.vb",
            "./helper.vb",
        ] {
            let mut unchanged = Vec::new();
            assert!(
                select_reindex_paths(
                    root.path(),
                    &[file.clone()],
                    &["helper.vb".into(), bad.into()],
                    &mut unchanged
                )
                .is_err()
            );
            assert!(unchanged.is_empty());
        }
    }
}
