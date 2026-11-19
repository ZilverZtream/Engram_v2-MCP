/// Comparison of one managed memory-source selection in the two stores.
#[derive(Debug, Clone, Copy)]
pub struct SourceGenerationObservation {
    pub lexical_count: usize,
    pub vector_count: usize,
    /// Distinct selected lexical primary keys present in the vector store.
    /// None means counts differed or the identity comparison was not performed.
    pub matching_vector_ids: Option<usize>,
}

#[cfg(feature = "vector")]
use std::collections::HashSet;

#[cfg(feature = "vector")]
const MAX_IDENTITY_ROWS: usize = 4096;
#[cfg(feature = "vector")]
const MAX_IDENTITY_BYTES: usize = 4 * 1024 * 1024;

#[cfg(feature = "vector")]
#[derive(Clone)]
pub(super) struct FileScope {
    pub path: String,
    pub revision: u64,
}

#[cfg(feature = "vector")]
impl FileScope {
    pub(super) fn lexical_query(
        &self,
        path: tantivy::schema::Field,
        generation: tantivy::schema::Field,
    ) -> Box<dyn tantivy::query::Query> {
        use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
        use tantivy::schema::{IndexRecordOption, Term};
        let term =
            |value| Box::new(TermQuery::new(value, IndexRecordOption::Basic)) as Box<dyn Query>;
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, term(Term::from_field_text(path, &self.path))),
            (
                Occur::Must,
                term(Term::from_field_u64(generation, self.revision)),
            ),
        ]))
    }

    pub(super) fn vector_filter(&self) -> String {
        format!(
            "namespace = 'memory' AND path = '{}' AND generation = {}",
            self.path.replace('\'', "''"),
            crate::sql_literals::u64_literal(self.revision),
        )
    }
}

#[cfg(feature = "vector")]
pub(super) fn lexical_rows(
    searcher: &tantivy::Searcher,
    query: &dyn tantivy::query::Query,
    pk: tantivy::schema::Field,
    compare_keys: bool,
) -> anyhow::Result<(usize, Option<HashSet<String>>)> {
    use tantivy::collector::{Count, DocSetCollector};
    use tantivy::schema::Value;

    let count = searcher.search(query, &Count)?;
    if !compare_keys || count > MAX_IDENTITY_ROWS {
        return Ok((count, None));
    }
    let mut keys = HashSet::with_capacity(count);
    let mut bytes = 0;
    for address in searcher.search(query, &DocSetCollector)? {
        let document: tantivy::TantivyDocument = searcher.doc(address)?;
        let value = document
            .get_first(pk)
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Selected Tantivy row has no primary key"))?;
        bytes += value.len();
        if bytes > MAX_IDENTITY_BYTES {
            return Ok((count, None));
        }
        anyhow::ensure!(
            keys.insert(value.to_owned()),
            "Duplicate selected Tantivy primary key"
        );
    }
    anyhow::ensure!(keys.len() == count, "Selected Tantivy row count changed");
    Ok((count, Some(keys)))
}

#[cfg(feature = "vector")]
pub(super) async fn match_vector_keys(
    table: &lancedb::Table,
    selection: String,
    mut expected: HashSet<String>,
) -> anyhow::Result<usize> {
    use arrow_array::{Array, StringArray};
    use futures::TryStreamExt;
    use lancedb::query::{ExecutableQuery, QueryBase, Select};

    let count = expected.len();
    let mut rows = table
        .query()
        .only_if(selection)
        .select(Select::Columns(vec!["pk".into()]))
        .limit(count.saturating_add(1))
        .execute()
        .await?;
    let mut observed = 0;
    while let Some(batch) = rows.try_next().await? {
        let keys = batch
            .column_by_name("pk")
            .and_then(|column| column.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| anyhow::anyhow!("Selected vector table has no primary key column"))?;
        for index in 0..keys.len() {
            if !keys.is_null(index) {
                expected.remove(keys.value(index));
            }
        }
        observed += keys.len();
    }
    anyhow::ensure!(
        observed == count,
        "Selected vector row count changed during the identity check"
    );
    Ok(count - expected.len())
}
