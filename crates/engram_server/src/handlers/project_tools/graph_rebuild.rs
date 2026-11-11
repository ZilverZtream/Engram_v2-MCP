use super::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tokio_util::sync::CancellationToken;

impl Engram {
    pub(super) async fn rebuild_project_graph(
        &self,
        project_id: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let update_guard = self.state.acquire_project_update_lock(project_id).await;
        let indexing_slot = crate::state::ActiveIndexingSlot::acquire(&self.state);
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Graph rebuild cancelled before extraction"
        );
        let runtime = self
            .ensure_project_runtime(project_id)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let mutation = runtime.search.begin_project_mutation(project_id)?;
        let search = runtime.search.with_mutation(&mutation)?;
        let base = self
            .capture_source_head(
                project_id,
                &runtime.info.directory,
                &runtime.info.project_type,
            )
            .await?;
        let generation = base.generation();
        let view =
            project_service::pin_source_generation(&self.state, &search, project_id, generation)?;
        let root = PathBuf::from(&runtime.info.directory);
        let exts = ProjectType::from_registry_str(&runtime.info.project_type)
            .map(exts_for_project_type_enum)
            .unwrap_or_else(|| exts_for_project_type(&runtime.info.project_type));
        let files = project_service::scan_source_files(&self.state, &root, &exts).await?;
        let scanned: BTreeSet<String> = files
            .iter()
            .map(|path| {
                engram_core::RelPath::from_relative(&root, path)
                    .map(|p| p.as_str().to_owned())
                    .ok_or_else(|| anyhow::anyhow!("Source outside registered root"))
            })
            .collect::<anyhow::Result<_>>()?;
        if let Some(selected) = view.selected_file_revisions() {
            anyhow::ensure!(
                selected.keys().cloned().collect::<BTreeSet<_>>() == scanned,
                "Source file set changed; run update_project before graph_only repair"
            );
        }
        let graph = self.state.graph.clone();
        let pid = project_id.to_owned();
        let expected_files = tokio::task::spawn_blocking(move || {
            Ok::<_, anyhow::Error>(
                graph
                    .query_nodes(&pid, Some("file"), None, None, usize::MAX)?
                    .into_iter()
                    .filter(|node| node.namespace == "memory")
                    .collect::<Vec<_>>(),
            )
        })
        .await??;
        let mut revisions = BTreeMap::new();
        for file in &expected_files {
            let path = file.file_path.as_str();
            anyhow::ensure!(
                scanned.contains(path),
                "Indexed file {path} is missing; run update_project first"
            );
            let selected = view
                .selected_file_revisions()
                .map_or(Some(generation), |paths| paths.get(path).copied());
            anyhow::ensure!(
                selected == Some(file.generation),
                "Graph file {path} does not match its selected source revision"
            );
            let target = file
                .metadata
                .as_ref()
                .and_then(|m| m.get("source_extraction_identity"))
                .and_then(|m| m.get("target_chars_per_chunk"))
                .and_then(|v| v.as_u64());
            anyhow::ensure!(
                target == Some(self.state.cfg.max_chunks_per_file as u64),
                "Chunk configuration changed or is unproven for {path}; run update_project first"
            );
            anyhow::ensure!(
                revisions.insert(path.to_owned(), file.generation).is_none(),
                "Duplicate graph file identity for {path}; full repair required"
            );
        }
        let extraction =
            project_service::capture_source_extraction_for_files(&self.state, &root, &files)
                .await?;
        let vb_files = project_service::admitted_vb_source_files(&root, &files);
        let stats = search
            .with_vb_source_inventory(vb_files)
            .extract_graph_files(
                project_id,
                &root,
                files,
                self.state.cfg.max_chunks_per_file,
                cancel,
                |_, _| {},
            )
            .await?;
        extraction.verify_produced(&stats)?;
        let produced = stats
            .all_files
            .iter()
            .map(|path| path.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        anyhow::ensure!(
            produced == revisions.keys().cloned().collect(),
            "Extraction changed the indexed file set; run update_project before graph_only repair"
        );
        let hashes: HashMap<_, _> = expected_files
            .iter()
            .map(|file| {
                (
                    file.file_path.as_str(),
                    file.metadata
                        .as_ref()
                        .and_then(|m| m.get("file_hash"))
                        .and_then(|v| v.as_str()),
                )
            })
            .collect();
        anyhow::ensure!(
            stats.fingerprints.len() == expected_files.len(),
            "Incomplete extraction fingerprints"
        );
        for fingerprint in &stats.fingerprints {
            anyhow::ensure!(
                hashes.get(fingerprint.rel_path.as_str())
                    == Some(&Some(fingerprint.file_hash.as_str())),
                "Source content changed for {}; run update_project before graph_only repair",
                fingerprint.rel_path
            );
        }
        let prepared = ingest_service::prepare_ingest_graph(
            &self.state,
            project_id,
            generation,
            &stats,
            Some(&revisions),
        )
        .await?;
        let files_count = stats.all_files.len();
        let skipped = stats.skipped_files;
        let warnings = stats.warnings;
        // Staging reuses the existing resolvers, but never exposes an incomplete
        // graph to readers. Its DB is closed before owned directory teardown.
        let data_dir = self.state.cfg.data_dir.clone();
        let pid = project_id.to_owned();
        let operation = search.begin_operation()?;
        let (nodes, edges, prepared) = tokio::task::spawn_blocking(move || {
            let _operation = operation;
            let temporary = tempfile::Builder::new()
                .prefix("graph-rebuild-")
                .tempdir_in(data_dir)?;
            let staged = (|| -> anyhow::Result<_> {
                let staging = engram_graph::GraphStore::open(&temporary.path().join("graph.redb"))?;
                staging.upsert_nodes_and_edges(&pid, &prepared.nodes, &prepared.edges)?;
                graph_service::resolve_app_code_globals(&staging, &pid, generation)?;
                staging.resolve_symbol_edges(&pid)?;
                let nodes = staging.query_nodes(&pid, None, None, None, usize::MAX)?;
                let mut edges = staging.list_edges(&pid, None)?;
                for edge in &mut edges {
                    if edge
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("resolved_from"))
                        .and_then(|v| v.as_str())
                        == Some("app_code")
                    {
                        let original = edge
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("original_target"))
                            .and_then(|v| v.as_str());
                        let producers: BTreeSet<_> = prepared
                            .edges
                            .iter()
                            .filter(|old| {
                                old.source_id == edge.source_id
                                    && Some(old.target_id.as_str()) == original
                            })
                            .map(|old| old.generation)
                            .collect();
                        anyhow::ensure!(
                            producers.len() == 1,
                            "Cannot recover App_Code producer revision"
                        );
                        edge.generation = *producers.first().unwrap();
                    }
                }
                Ok((nodes, edges))
            })();
            temporary.close()?;
            let (nodes, edges) = staged?;
            Ok::<_, anyhow::Error>((nodes, edges, prepared))
        })
        .await??;
        // Drain optional ranking workers so an old computation cannot republish
        // a cache under the unchanged source generation after replacement.
        let ranking = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Graph rebuild cancelled before publication"),
            permit = self.state.pagerank_workers.clone().acquire_many_owned(crate::state::PAGERANK_WORKER_LIMIT) => permit?,
        };
        #[cfg(test)]
        tests::before_publication(&self.state, cancel)?;
        // Check after any wait for ranking workers, immediately before the
        // content verification and atomic replacement worker.
        let rescanned = project_service::scan_source_files(&self.state, &root, &exts).await?;
        let final_paths: BTreeSet<_> = rescanned
            .iter()
            .filter_map(|path| engram_core::RelPath::from_relative(&root, path))
            .map(|p| p.as_str().to_owned())
            .collect();
        anyhow::ensure!(
            scanned == final_paths,
            "Source file set changed during graph rebuild; retry after update_project"
        );
        let state = self.state.clone();
        let pid = project_id.to_owned();
        let cancel = cancel.clone();
        let operation = search.begin_operation()?;
        let nodes_count = nodes.len();
        let edges_count = edges.len();
        let report = tokio::task::spawn_blocking(move || {
            // Actual publication owns every admission through completion, even
            // if the requesting future is dropped during this worker.
            let _owners = (
                update_guard,
                indexing_slot,
                mutation,
                operation,
                view,
                prepared,
                ranking,
            );
            project_service::verify_graph_rebuild_bytes(&root, &expected_files, &cancel)?;
            extraction.with_publication_fence(|| {
                anyhow::ensure!(
                    !cancel.is_cancelled(),
                    "Graph rebuild cancelled before publication"
                );
                anyhow::ensure!(
                    state.registry.source_head_is_current(&base)?,
                    "Source head changed during graph rebuild; retry"
                );
                let report =
                    state
                        .graph
                        .replace_source_graph(&pid, &expected_files, &nodes, &edges)?;
                runtime.pagerank_cache.invalidate_after_graph_rebuild();
                Ok(report)
            })
        })
        .await??;
        Ok(format!(
            "Graph rebuilt for project_id: {project_id}\nactive_generation: {generation} (unchanged)\nfiles={files_count} nodes={nodes_count} edges={edges_count}\nsearch/vector stores preserved; embeddings=0; Git history follow-up skipped (graph_only)\nreplacement: {}\nextraction_skips: {}\nextraction_warnings: {}",
            serde_json::to_string(&report)?,
            serde_json::to_string(
                &serde_json::json!({"total":skipped.len(),"sample":skipped.iter().take(8).collect::<Vec<_>>()})
            )?,
            serde_json::to_string(
                &serde_json::json!({"total":warnings.len(),"sample":warnings.iter().take(8).collect::<Vec<_>>()})
            )?
        ))
    }
}

#[cfg(test)]
mod tests;
