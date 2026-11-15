use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// Physical retained rows, deliberately independent of source publication.
#[derive(Debug, Clone)]
pub struct NamespaceVectorCounts {
    pub total: usize,
    pub by_namespace: BTreeMap<String, usize>,
}

impl HybridSearchEngine {
    /// Inventory every lexical namespace for this project without loading stored
    /// documents. Bounds apply to metadata discovery, not corpus size.
    pub fn count_all_namespace_docs(
        &self,
        project_id: &str,
    ) -> anyhow::Result<BTreeMap<String, usize>> {
        let searcher = self.tantivy_index.reader()?.searcher();
        let mut namespaces = BTreeSet::new();
        for segment in searcher.segment_readers() {
            let index = segment.inverted_index(self.fields.namespace)?;
            let mut stream = index.terms().stream()?;
            while let Some((bytes, _)) = stream.next() {
                anyhow::ensure!(
                    bytes.len() <= 256,
                    "Namespace metadata exceeds count byte budget"
                );
                let namespace = std::str::from_utf8(bytes)?;
                anyhow::ensure!(
                    namespaces.contains(namespace) || namespaces.len() < 256,
                    "Namespace inventory exceeds count budget; counts unavailable"
                );
                namespaces.insert(namespace.to_owned());
            }
        }
        let mut counts = BTreeMap::new();
        for namespace in namespaces {
            let query = BooleanQuery::new(vec![
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.project_id, project_id),
                        IndexRecordOption::Basic,
                    )) as Box<dyn tantivy::query::Query>,
                ),
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.namespace, &namespace),
                        IndexRecordOption::Basic,
                    )),
                ),
            ]);
            let count = searcher.search(&query, &tantivy::collector::Count)?;
            if count > 0 {
                counts.insert(namespace, count);
            }
        }
        Ok(counts)
    }

    /// Count requested namespace filters in one admitted native epoch. None
    /// means vector support is unavailable, rather than a verified empty table.
    pub async fn count_namespace_vectors(
        &self,
        project_id: &str,
        namespaces: Vec<String>,
    ) -> anyhow::Result<Option<NamespaceVectorCounts>> {
        anyhow::ensure!(
            namespaces.len() <= 512 && namespaces.iter().all(|name| name.len() <= 256),
            "Namespace vector count request exceeds metadata budget"
        );
        #[cfg(feature = "vector")]
        {
            let epoch = self.pin_vector_epoch(project_id)?;
            let worker = self.clone();
            self.run_native_vector_query(epoch.clone(), &CancellationToken::new(), async move {
                worker.ensure_managed_vector_table_present(&epoch)?;
                let mut counts = NamespaceVectorCounts {
                    total: 0,
                    by_namespace: BTreeMap::new(),
                };
                if !worker
                    .native_store
                    .capture()?
                    .connection
                    .table_names()
                    .execute()
                    .await?
                    .contains(&epoch.table)
                {
                    return Ok(Some(counts));
                }
                let table = worker.open_vector_epoch_table(&epoch).await?;
                counts.total = table.count_rows(None).await?;
                for namespace in namespaces.into_iter().collect::<BTreeSet<_>>() {
                    let filter = format!("namespace = '{}'", namespace.replace('\'', "''"));
                    counts
                        .by_namespace
                        .insert(namespace, table.count_rows(Some(filter)).await?);
                }
                Ok(Some(counts))
            })
            .await
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = (project_id, namespaces);
            Ok(None)
        }
    }
}
