//! File-revision membership under an existing request capability. No fresh pins,
//! stored-body reads, or reconstruction of historical graph declarations.
use super::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use tantivy::collector::Count;

const MAX_LEGACY_CHECKS: usize = 4096;
const MAX_LEGACY_PATH_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct LegacyCache {
    // None remembers an unavailable observation, preventing repeated failing IO.
    // The physical generation is fixed by the held legacy view before lookup.
    entries: HashMap<String, Option<bool>>,
    path_bytes: usize,
}

struct LegacyLookup {
    searcher: tantivy::Searcher,
    fields: Fields,
    cache: Mutex<LegacyCache>,
}

/// A validated project/source view used to check graph file revisions.
/// Managed views use their existing immutable map. Legacy views retain one
/// lexical reader and bound unique metadata-only checks; None means unknown.
pub struct SelectedSourceRevisionLookup {
    view: Arc<SourceSnapshotGuard>,
    legacy: Option<LegacyLookup>,
}

/// Indexed code belonging to one held published source selection. Retained
/// history, knowledge and abandoned physical revisions are not source code.
#[derive(Debug, Clone)]
pub struct SelectedSourceSummary {
    pub source_files: Option<usize>,
    pub source_chunks: usize,
    pub chunks_by_language: std::collections::BTreeMap<String, usize>,
    /// One observation per managed file identity, independent of chunk counts.
    /// Extension labels are ASCII-case-normalized and bounded to 64 bytes;
    /// excess distinct labels/long extensions share the `(other)` bucket.
    pub files_by_extension: std::collections::BTreeMap<String, usize>,
    /// Non-overlapping parent-directory groups, at most three components deep.
    /// At most 4096 groups/256 KiB of group names are observed; 32 are shown.
    /// Truncation means this is a sample, never a claim of global top groups.
    pub files_by_directory: std::collections::BTreeMap<String, usize>,
    pub directories_truncated: bool,
}

/// Bounded navigation over the existing immutable managed map. File identities
/// are not declarations, disk freshness or a runtime architecture inference.
pub struct SelectedSourceNavigation {
    pub matching_files: Option<usize>,
    pub inspected_files: usize,
    pub scan_truncated: bool,
    pub areas_truncated: bool,
    pub representative_sample: bool,
    pub files: Vec<(String, u64, String)>,
    pub areas: Vec<(String, usize)>,
    pub next_offset: Option<usize>,
}

impl HybridSearchEngine {
    /// Count lexical metadata under an existing capability; never materialize
    /// stored bodies, open native vectors or rebuild the source selection.
    pub fn selected_source_summary(
        &self,
        project_id: &str,
        generation: u64,
        view: Arc<SourceSnapshotGuard>,
    ) -> anyhow::Result<SelectedSourceSummary> {
        self.validate_source_snapshot_guard(project_id, generation, &view)?;
        let searcher = self.tantivy_index.reader()?.searcher();
        let term = |field, value: &str| -> Box<dyn tantivy::query::Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, value),
                IndexRecordOption::Basic,
            ))
        };
        let count = |language: Option<&str>| -> anyhow::Result<usize> {
            let mut clauses = vec![
                (Occur::Must, term(self.fields.project_id, project_id)),
                (
                    Occur::Must,
                    term(
                        self.fields.namespace,
                        engram_core::namespaces::NAMESPACE_MEMORY,
                    ),
                ),
                (Occur::Must, view._snapshot.query(&self.fields)),
            ];
            if let Some(language) = language {
                clauses.push((Occur::Must, term(self.fields.language, language)));
            }
            Ok(searcher.search(&BooleanQuery::new(clauses), &Count)?)
        };
        let source_chunks = count(None)?;
        let mut languages = std::collections::BTreeSet::new();
        for segment in searcher.segment_readers() {
            let index = segment.inverted_index(self.fields.language)?;
            let mut stream = index.terms().stream()?;
            while let Some((bytes, _)) = stream.next() {
                anyhow::ensure!(
                    bytes.len() <= 128,
                    "Language metadata term exceeds overview byte budget"
                );
                let language = std::str::from_utf8(bytes)?;
                anyhow::ensure!(
                    languages.contains(language) || languages.len() < 256,
                    "Language metadata exceeds overview budget; source summary unavailable"
                );
                if !language.is_empty() {
                    languages.insert(language.to_owned());
                }
                anyhow::ensure!(
                    languages.len() <= 256,
                    "Language metadata exceeds overview budget; source summary unavailable"
                );
            }
        }
        let mut chunks_by_language = std::collections::BTreeMap::new();
        for language in languages {
            let chunks = count(Some(&language))?;
            if chunks > 0 {
                chunks_by_language.insert(language, chunks);
            }
        }
        let classified: usize = chunks_by_language.values().sum();
        anyhow::ensure!(
            classified <= source_chunks,
            "Source language counts overlap; overview unavailable"
        );
        if classified < source_chunks {
            *chunks_by_language.entry("unknown".into()).or_insert(0) += source_chunks - classified;
        }
        let mut directories = std::collections::BTreeMap::new();
        let mut files_by_extension = std::collections::BTreeMap::new();
        let mut directory_bytes = 0usize;
        let mut directory_discovery_capped = false;
        let source_files = match &view._snapshot.selection {
            snapshots::Selection::Files(files) => {
                for path in files.keys() {
                    let mut extension = match std::path::Path::new(path).extension().and_then(|value| value.to_str()) {
                        Some(value) if value.len() <= 64 => format!(".{}", value.to_ascii_lowercase()),
                        Some(_) => "(other)".into(),
                        None => "(no extension)".into(),
                    };
                    // Reserve one label for all omitted types, preserving the
                    // exact file denominator even when display detail is capped.
                    if !files_by_extension.contains_key(&extension) && files_by_extension.len() >= 255 {
                        extension = "(other)".into();
                    }
                    *files_by_extension.entry(extension).or_insert(0usize) += 1;
                    let parent = path.rsplit_once('/').map_or("(root)", |(parent, _)| parent);
                    let end = parent
                        .match_indices('/')
                        .nth(2)
                        .map_or(parent.len(), |(offset, _)| offset);
                    let directory = &parent[..end];
                    if let Some(count) = directories.get_mut(directory) {
                        *count += 1;
                        continue;
                    }
                    if directories.len() >= 4096
                        || directory.len() > 2048
                        || directory_bytes.saturating_add(directory.len()) > 256 * 1024
                    {
                        directory_discovery_capped = true;
                        continue;
                    }
                    directory_bytes += directory.len();
                    let directory = directory.to_owned();
                    *directories.entry(directory).or_insert(0usize) += 1;
                }
                Some(files.len())
            }
            snapshots::Selection::Legacy(_) => None,
        };
        let directories_truncated = directory_discovery_capped || directories.len() > 32;
        let mut ranked: Vec<_> = directories.into_iter().collect();
        ranked.sort_by(|(left_path, left_count), (right_path, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_path.cmp(right_path))
        });
        ranked.truncate(32);
        Ok(SelectedSourceSummary {
            source_files,
            source_chunks,
            chunks_by_language,
            files_by_extension,
            files_by_directory: ranked.into_iter().collect(),
            directories_truncated,
        })
    }

    pub fn selected_source_revision_lookup(
        &self,
        project_id: &str,
        generation: u64,
        view: Arc<SourceSnapshotGuard>,
    ) -> anyhow::Result<SelectedSourceRevisionLookup> {
        self.validate_source_snapshot_guard(project_id, generation, &view)?;
        let legacy = if matches!(view._snapshot.selection, snapshots::Selection::Legacy(_)) {
            Some(LegacyLookup {
                searcher: self.tantivy_index.reader()?.searcher(),
                fields: self.fields.clone(),
                cache: Mutex::new(LegacyCache::default()),
            })
        } else {
            None
        };
        Ok(SelectedSourceRevisionLookup { view, legacy })
    }
}

impl SelectedSourceRevisionLookup {
    pub fn navigation_paths(
        &self,
        scope: Option<&str>,
        excluded: &[String],
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<SelectedSourceNavigation> {
        let valid = |path: &str| !path.is_empty() && path.len() <= 2048
            && !path.contains(['\\', ':', '\0', '\n', '\r'])
            && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..");
        anyhow::ensure!(scope.is_none_or(valid) && excluded.len() <= 16
            && excluded.iter().all(|path| valid(path)) && (1..=64).contains(&limit)
            && offset <= 250_000, "Invalid CodeMap literal scope, exclusions or page bounds");
        let snapshots::Selection::Files(files) = &self.view._snapshot.selection else {
            return Ok(SelectedSourceNavigation { matching_files: None, inspected_files: 0,
                scan_truncated: false, areas_truncated: false, representative_sample: scope.is_none(),
                files: Vec::new(), areas: Vec::new(), next_offset: None });
        };
        fn within(path: &str, boundary: &str) -> bool {
            #[cfg(windows)]
            let (path, boundary) = (path.to_lowercase(), boundary.to_lowercase());
            #[cfg(windows)]
            let (path, boundary) = (path.as_str(), boundary.as_str());
            path == boundary || path.strip_prefix(boundary).is_some_and(|tail| tail.starts_with('/'))
        }
        fn area_of(path: &str) -> &str {
            // Root files use their actual file identity as an exploration
            // boundary, never a synthetic `(root)` path that cannot resolve.
            let Some((parent,_)) = path.rsplit_once('/') else { return path; };
            let end = parent.match_indices('/').nth(2).map_or(parent.len(), |(position,_)|position);
            &parent[..end]
        }
        let priority = |path: &str| {
            let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
            if name.contains("policy") { 4u8 }
            else if name.contains("service") || name.contains("controller") || name.contains("handler") { 3 }
            else if name.contains("application") || name.contains("bootstrap") || name.starts_with("main.") { 2 }
            else { 1 }
        };
        // Borrow map strings during selection. Only bounded result paths and
        // area labels are copied; never clone the complete source map.
        let mut groups: BTreeMap<&str, (usize, Vec<(&str,u64)>)> = BTreeMap::new();
        let mut group_bytes = 0usize;
        let mut page = Vec::new();
        let mut matching = 0usize;
        let mut inspected = 0usize;
        let mut scan_truncated = false;
        let mut areas_truncated = false;
        for (path, generation) in files {
            if inspected == 50_000 { scan_truncated = true; break; }
            inspected += 1;
            if !valid(path) { scan_truncated = true; continue; }
            if scope.is_some_and(|scope| !within(path, scope))
                || excluded.iter().any(|excluded| within(path, excluded)) { continue; }
            let position = matching;
            matching += 1;
            let area = area_of(path);
            if scope.is_some() && position >= offset && page.len() < limit {
                page.push((path.as_str(), *generation, area));
            }
            if !groups.contains_key(area) {
                if groups.len() == 128 || group_bytes.saturating_add(area.len()) > 128 * 1024 {
                    areas_truncated = true; continue;
                }
                group_bytes += area.len();
                groups.insert(area, (0, Vec::new()));
            }
            let bucket = groups.get_mut(area).expect("bounded area");
            bucket.0 += 1;
            if scope.is_none() {
                bucket.1.push((path.as_str(), *generation));
                bucket.1.sort_by(|a,b| priority(b.0).cmp(&priority(a.0)).then_with(|| a.0.cmp(b.0)));
                bucket.1.truncate(2);
            }
        }
        let mut ranked: Vec<_> = groups.into_iter().collect();
        ranked.sort_by(|a,b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
        let available = if scope.is_none() {
            // File representatives use the strongest basename lead in each
            // retained group before area size. The independently count-ranked
            // area display must not hide a small role-bearing source area.
            let mut representatives: Vec<_> = ranked.iter().collect();
            representatives.sort_by(|a,b| {
                let role = |group: &(&str, (usize, Vec<(&str, u64)>))|
                    group.1.1.first().map_or(0, |(path,_)| priority(path));
                role(b).cmp(&role(a))
                    .then_with(|| b.1.0.cmp(&a.1.0))
                    .then_with(|| a.0.cmp(b.0))
            });
            let mut diverse = Vec::new();
            for round in 0..2 {
                for (area, (_, paths)) in &representatives {
                    if let Some((path,generation)) = paths.get(round) { diverse.push((*path,*generation,*area)); }
                }
            }
            let available = diverse.len();
            page.extend(diverse.into_iter().skip(offset).take(limit));
            available
        } else { matching };
        if ranked.len() > 32 { areas_truncated = true; ranked.truncate(32); }
        let next_offset = (offset + page.len() < available && !page.is_empty()).then_some(offset + page.len());
        Ok(SelectedSourceNavigation {
            matching_files: (!scan_truncated).then_some(matching), inspected_files: inspected,
            scan_truncated, areas_truncated, representative_sample: scope.is_none(),
            files: page.into_iter().map(|(path,generation,area)| (path.to_owned(),generation,area.to_owned())).collect(),
            areas: ranked.into_iter().map(|(area,(count,_))| (area.to_owned(),count)).collect(), next_offset,
        })
    }

    /// Discover file identities from the held immutable map, without loading
    /// every stored chunk body. Scope is an exact file or directory boundary.
    pub fn paths_in_scope(
        &self,
        scope: &str,
        limit: usize,
    ) -> anyhow::Result<(Vec<(String, u64)>, bool)> {
        anyhow::ensure!(
            limit > 0 && limit <= 128,
            "Source path discovery limit must be 1..128"
        );
        anyhow::ensure!(
            !scope.is_empty()
                && !scope.contains('\\')
                && scope
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "Use a normalized project-relative area scope"
        );
        let snapshots::Selection::Files(files) = &self.view._snapshot.selection else {
            anyhow::bail!(
                "Automatic area evidence requires a managed source snapshot; complete an explicit non-wiping full repair, or supply evidence paths without area_context"
            );
        };
        #[cfg(windows)]
        let folded_scope = scope.to_lowercase();
        #[cfg(windows)]
        let scope = folded_scope.as_str();
        let mut selected = Vec::new();
        for (path, generation) in files.iter() {
            #[cfg(windows)]
            let folded_path = path.to_lowercase();
            #[cfg(windows)]
            let normalized = folded_path.as_str();
            #[cfg(not(windows))]
            let normalized = path.as_str();
            if normalized == scope
                || normalized
                    .strip_prefix(scope)
                    .is_some_and(|tail| tail.starts_with('/'))
            {
                if selected.len() == limit {
                    return Ok((selected, true));
                }
                selected.push((path.clone(), *generation));
            }
        }
        Ok((selected, false))
    }

    /// Check the exact physical file revision, including inherited older files.
    /// Unknown observations must not be interpreted as proven absence/presence.
    pub fn matches(&self, path: &str, physical_generation: u64) -> anyhow::Result<Option<bool>> {
        match &self.view._snapshot.selection {
            snapshots::Selection::Files(files) => {
                return Ok(Some(files.get(path) == Some(&physical_generation)));
            }
            snapshots::Selection::Legacy(generation) if *generation != physical_generation => {
                return Ok(Some(false));
            }
            snapshots::Selection::Legacy(_) => {}
        }
        let legacy = self
            .legacy
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Legacy source membership reader unavailable"))?;
        let mut cache = legacy
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Legacy source membership cache unavailable"))?;
        // Borrow the path for a cache hit instead of allocating another key.
        // The bounded legacy case deliberately avoids retaining source bodies.
        if let Some(result) = cache.entries.get(path) {
            return Ok(*result);
        }
        let Some(path_bytes) = cache
            .path_bytes
            .checked_add(path.len())
            .filter(|bytes| *bytes <= MAX_LEGACY_PATH_BYTES)
        else {
            return Ok(None);
        };
        if cache.entries.len() >= MAX_LEGACY_CHECKS {
            return Ok(None);
        }
        let term = |field, text: &str| -> Box<dyn tantivy::query::Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, text),
                IndexRecordOption::Basic,
            ))
        };
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                term(legacy.fields.project_id, &self.view._project),
            ),
            (
                Occur::Must,
                term(
                    legacy.fields.namespace,
                    engram_core::namespaces::NAMESPACE_MEMORY,
                ),
            ),
            (Occur::Must, term(legacy.fields.path, path)),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(legacy.fields.generation, physical_generation),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let observed = legacy
            .searcher
            .search(&query, &Count)
            .map(|count| count > 0);
        cache.path_bytes = path_bytes;
        cache
            .entries
            .insert(path.to_owned(), observed.as_ref().ok().copied());
        observed.map(Some).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[test]
    fn legacy_membership_requires_real_selected_source_and_reports_unknown_after_its_read_budget()
    -> Result<()> {
        let owned = tempfile::tempdir()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            let config = engram_core::Config { data_dir: owned.path().join("data"), embedding_backend: "fts_only".into(), ..Default::default() };
            let search = HybridSearchEngine::new(owned.path().join("lexical"), owned.path().join("vectors"), &config).await?;
            for (project, namespace, generation, path) in [
                ("legacy", "memory", 1, "present.rs"),
                ("legacy", "memory", 2, "only-newer.rs"),
                ("legacy", "memory_bank", 0, "knowledge-only.rs"),
                ("another-project", "memory", 1, "another-project.rs"),
            ] {
                let content = "pub fn actual_source() {}\n";
                search.index_docs(project, &[IndexDoc {
                    generation, chunk_id: generation, path: RelPath::new(path), language: "rust".into(), content: content.into(),
                    content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(), doc_id: path.into(), namespace: namespace.into(),
                    author: None, timestamp: None, start_line: 1, end_line: 1,
                }], &CancellationToken::new()).await?;
            }
            let view = Arc::new(search.pin_source_generation("legacy", 1)?);
            let revisions = search.selected_source_revision_lookup("legacy", 1, view)?;
            ensure!(revisions.matches("present.rs", 1)? == Some(true));
            ensure!(revisions.matches("missing.rs", 1)? == Some(false));
            ensure!(revisions.matches("only-newer.rs", 1)? == Some(false), "An existing path at another physical generation is not selected");
            ensure!(revisions.matches("present.rs", 2)? == Some(false));
            ensure!(revisions.matches("knowledge-only.rs", 1)? == Some(false));
            ensure!(revisions.matches("another-project.rs", 1)? == Some(false));
            let mut saw_unknown = false;
            for index in 0..4096 {
                if revisions.matches(&format!("bounded-missing-{index}.rs"), 1)?.is_none() { saw_unknown = true; break; }
            }
            ensure!(saw_unknown && revisions.matches("beyond-observation-budget.rs", 1)?.is_none(),
                "Uninspected legacy paths must be unknown when the finite metadata-read budget is exhausted");
            ensure!(revisions.matches("present.rs", 1)? == Some(true), "Budget exhaustion must preserve already-observed source evidence");
            Ok::<_, anyhow::Error>(())
        });
        drop(runtime);
        match (result, owned.close()) {
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("Fixture cleanup also failed: {cleanup}")))
            }
            (Err(error), _) => Err(error),
            (Ok(()), cleanup) => cleanup.map_err(Into::into),
        }
    }
}
