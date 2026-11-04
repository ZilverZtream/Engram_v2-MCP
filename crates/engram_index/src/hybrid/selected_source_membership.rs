//! File-revision membership under an existing request capability. No fresh pins,
//! stored-body reads, or reconstruction of historical graph declarations.
use super::*;
use std::collections::HashMap;
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

impl HybridSearchEngine {
    pub fn selected_source_revision_lookup(
        &self,
        project_id: &str,
        generation: u64,
        view: Arc<SourceSnapshotGuard>,
    ) -> anyhow::Result<SelectedSourceRevisionLookup> {
        self.validate_source_snapshot_guard(project_id, generation, &view)?;
        let legacy = if matches!(view._snapshot.selection, snapshots::Selection::Legacy(_)) {
            Some(LegacyLookup { searcher: self.tantivy_index.reader()?.searcher(),
                fields: self.fields.clone(), cache: Mutex::new(LegacyCache::default()) })
        } else { None };
        Ok(SelectedSourceRevisionLookup { view, legacy })
    }
}

impl SelectedSourceRevisionLookup {
    /// Check the exact physical file revision, including inherited older files.
    /// Unknown observations must not be interpreted as proven absence/presence.
    pub fn matches(&self, path: &str, physical_generation: u64) -> anyhow::Result<Option<bool>> {
        match &self.view._snapshot.selection {
            snapshots::Selection::Files(files) => return Ok(Some(files.get(path) == Some(&physical_generation))),
            snapshots::Selection::Legacy(generation) if *generation != physical_generation => return Ok(Some(false)),
            snapshots::Selection::Legacy(_) => {}
        }
        let legacy = self.legacy.as_ref().ok_or_else(|| anyhow::anyhow!("Legacy source membership reader unavailable"))?;
        let mut cache = legacy.cache.lock().map_err(|_| anyhow::anyhow!("Legacy source membership cache unavailable"))?;
        // Borrow the path for a cache hit instead of allocating another key.
        // The bounded legacy case deliberately avoids retaining source bodies.
        if let Some(result) = cache.entries.get(path) {
            return Ok(*result);
        }
        let Some(path_bytes) = cache.path_bytes.checked_add(path.len()).filter(|bytes| *bytes <= MAX_LEGACY_PATH_BYTES) else {
            return Ok(None);
        };
        if cache.entries.len() >= MAX_LEGACY_CHECKS { return Ok(None); }
        let term = |field, text: &str| -> Box<dyn tantivy::query::Query> {
            Box::new(TermQuery::new(Term::from_field_text(field, text), IndexRecordOption::Basic))
        };
        let query = BooleanQuery::new(vec![
            (Occur::Must, term(legacy.fields.project_id, &self.view._project)),
            (Occur::Must, term(legacy.fields.namespace, engram_core::namespaces::NAMESPACE_MEMORY)),
            (Occur::Must, term(legacy.fields.path, path)),
            (Occur::Must, Box::new(TermQuery::new(Term::from_field_u64(legacy.fields.generation,
                physical_generation), IndexRecordOption::Basic))),
        ]);
        let observed = legacy.searcher.search(&query, &Count).map(|count| count > 0);
        cache.path_bytes = path_bytes;
        cache.entries.insert(path.to_owned(), observed.as_ref().ok().copied());
        observed.map(Some).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[test]
    fn legacy_membership_requires_real_selected_source_and_reports_unknown_after_its_read_budget() -> Result<()> {
        let owned = tempfile::tempdir()?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
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
            (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup also failed: {cleanup}"))),
            (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup.map_err(Into::into),
        }
    }
}
