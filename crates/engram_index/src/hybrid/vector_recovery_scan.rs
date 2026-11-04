//! Byte-bounded aggregation of retained physical documents during recovery.
use super::*;
use std::collections::BTreeMap;
use tantivy::query::Query;

// A normal source file is at most 10 MiB. Leave room for its stored metadata
// and other namespace records; never silently omit an oversized retained row.
pub(super) const MAX_RECOVERY_RECORD_BYTES: usize = 32 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = MAX_RECOVERY_RECORD_BYTES;
const MAX_PAGE_RECORDS: usize = 128;

impl HybridSearchEngine {
    pub(super) async fn scan_retained_vectors(
        &self,
        project: &str,
        stage: &str,
        searcher: &tantivy::Searcher,
        query: &TermQuery,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(usize, String)> {
        let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(
            searcher,
        ))?;
        let mut groups = BTreeMap::<String, Vec<IndexDoc>>::new();
        let mut page_guards = Vec::<AllocationGuard>::new();
        let mut page_bytes = 0usize;
        let mut page_records = 0usize;
        let mut processed = 0usize;
        let mut coverage = blake3::Hasher::new();
        for (segment_ord, segment) in searcher.segment_readers().iter().enumerate() {
            let mut scorer = weight.scorer(segment, 1.0)?;
            while scorer.doc() != tantivy::TERMINATED && segment.is_deleted(scorer.doc()) {
                scorer.advance();
            }
            if scorer.doc() == tantivy::TERMINATED {
                continue;
            }
            let _read_allowance = self
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        MAX_RECOVERY_RECORD_BYTES as u64,
                        Subsystem::ParseBuffer,
                        "vector recovery stored-block read allowance",
                    )
                })
                .transpose()?;
            // get_document_bytes decompresses before exposing length: this
            // allowance is not a hard bound on dependency-owned decompression.
            // One cached block lets the typed read reuse that decompression.
            let store = segment.get_store_reader(1)?;
            while scorer.doc() != tantivy::TERMINATED {
                anyhow::ensure!(!cancel.is_cancelled(), "Vector recovery cancelled");
                let doc_id = scorer.doc();
                scorer.advance();
                if segment.is_deleted(doc_id) {
                    continue;
                }
                let raw = store.get_document_bytes(doc_id)?;
                let bytes = raw.len();
                anyhow::ensure!(
                    bytes <= MAX_RECOVERY_RECORD_BYTES,
                    "Stored document {segment_ord}:{doc_id} exceeds vector recovery record limit: {bytes} bytes > {MAX_RECOVERY_RECORD_BYTES}; active data preserved"
                );
                drop(raw); // Do not pin a decompressed block for each page row.
                if page_records > 0
                    && (page_records == MAX_PAGE_RECORDS
                        || page_bytes
                            .checked_add(bytes)
                            .is_none_or(|n| n > MAX_PAGE_BYTES))
                {
                    self.flush_recovered_page(
                        project,
                        stage,
                        &mut groups,
                        &mut page_guards,
                        &mut processed,
                        &mut coverage,
                        cancel,
                    )
                    .await?;
                    page_bytes = 0;
                    page_records = 0;
                }
                // Valid Engram rows have a fixed field set. Cover the typed
                // document, extracted text and grouping/identity copies before
                // allocating them. Native malformed-value allocation is separate.
                let retained_bytes = (bytes as u64)
                    .checked_mul(3)
                    .and_then(|n| n.checked_add(8192))
                    .ok_or_else(|| anyhow::anyhow!("Recovery document size overflow"))?;
                let retained = self
                    .memory_budget
                    .as_ref()
                    .map(|budget| {
                        AllocationGuard::try_new(
                            budget,
                            retained_bytes,
                            Subsystem::ParseBuffer,
                            "vector recovery retained document",
                        )
                    })
                    .transpose()?;
                let stored: tantivy::TantivyDocument = store.get(doc_id)?;
                let string = |field| -> anyhow::Result<String> {
                    stored
                        .get_first(field)
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            anyhow::anyhow!("Stored document is missing a required text field")
                        })
                };
                let number = |field| -> anyhow::Result<u64> {
                    stored
                        .get_first(field)
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| {
                            anyhow::anyhow!("Stored document is missing a required numeric field")
                        })
                };
                let doc = IndexDoc {
                    generation: number(self.fields.generation)?,
                    chunk_id: number(self.fields.chunk_id)?,
                    path: RelPath::new(&string(self.fields.path)?),
                    language: string(self.fields.language)?,
                    content: string(self.fields.content)?,
                    namespace: string(self.fields.namespace)?,
                    author: stored
                        .get_first(self.fields.author)
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    timestamp: stored
                        .get_first(self.fields.timestamp)
                        .and_then(|v| v.as_u64()),
                    start_line: number(self.fields.start_line)?.try_into()?,
                    end_line: number(self.fields.end_line)?.try_into()?,
                    doc_id: string(self.fields.doc_id)?,
                    content_hash: string(self.fields.content_hash)?,
                };
                anyhow::ensure!(
                    stored.get_first(self.fields.pk).and_then(|v| v.as_str())
                        == Some(
                            build_pk(project, &doc.namespace, doc.generation, &doc.doc_id).as_str()
                        ),
                    "Stored primary key does not match its identity fields"
                );
                groups.entry(doc.namespace.clone()).or_default().push(doc);
                if let Some(guard) = retained {
                    page_guards.push(guard);
                }
                page_bytes += bytes;
                page_records += 1;
            }
        }
        self.flush_recovered_page(
            project,
            stage,
            &mut groups,
            &mut page_guards,
            &mut processed,
            &mut coverage,
            cancel,
        )
        .await?;
        Ok((processed, coverage.finalize().to_hex().to_string()))
    }

    async fn flush_recovered_page(
        &self,
        project: &str,
        stage: &str,
        groups: &mut BTreeMap<String, Vec<IndexDoc>>,
        guards: &mut Vec<AllocationGuard>,
        processed: &mut usize,
        coverage: &mut blake3::Hasher,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        for docs in std::mem::take(groups).into_values() {
            anyhow::ensure!(!cancel.is_cancelled(), "Vector recovery cancelled");
            self.embed_and_upsert_vectors_to_table(project, &docs, cancel, Some(stage))
                .await?;
            anyhow::ensure!(!cancel.is_cancelled(), "Vector recovery cancelled");
            self.verify_recovered_vector_rows(project, stage, &docs)
                .await?;
            for doc in &docs {
                for value in [
                    build_pk(project, &doc.namespace, doc.generation, &doc.doc_id),
                    doc.content_hash.clone(),
                ] {
                    coverage.update(&(value.len() as u64).to_le_bytes());
                    coverage.update(value.as_bytes());
                }
            }
            *processed += docs.len();
        }
        guards.clear();
        Ok(())
    }
}
