use super::*;
use anyhow::{Result, ensure};

fn doc(generation: u64, text: &str) -> IndexDoc {
    IndexDoc { generation, chunk_id: 1, path: RelPath::new("source.rs"), language: "rust".into(),
        content: text.into(), namespace: "memory".into(), author: None, timestamp: None,
        start_line: 1, end_line: 1, doc_id: "shared:logical:id".into(), content_hash: ContentHash::compute(text.as_bytes()).0 }
}

fn query(generation: u64) -> HybridQuery {
    HybridQuery { project_id: "project".into(), namespace: "memory".into(), generation,
        text: "needle".into(), top_k: 10, fts_mode: "loose".into(), include_path_prefixes: None,
        exclude_path_prefixes: None, include_path_suffixes: None, language_filters: None,
        author_filter: None, date_after: None, date_before: None, use_mmr: false }
}

#[tokio::test]
async fn selected_source_recovery_keeps_exact_old_identity_and_rejects_foreign_or_corrupt_evidence() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let cfg = engram_core::Config { embedding_backend: "fts_only".into(), ..Default::default() };
        let engine = HybridSearchEngine::new(temporary.path().join("tantivy"), temporary.path().join("vectors"), &cfg).await?;
        let other_root = HybridSearchEngine::new(temporary.path().join("other-tantivy"), temporary.path().join("other-vectors"), &cfg).await?;
        let cancel = CancellationToken::new();
        engine.index_docs("project", &[doc(1, "needle old exact body")], &cancel).await?;
        let old = engine.pin_source_generation("project", 1)?;
        let foreign_project = engine.pin_source_generation("other", 1)?;
        let foreign_root = other_root.pin_source_generation("project", 1)?;
        let old_hit = engine.lexical_search(&query(1))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("Old hit missing"))?;
        ensure!(engine.recover_selected_source_hit("project", &old, &old_hit)?.ok_or_else(|| anyhow::anyhow!("Old body missing"))?.2 == "needle old exact body");
        ensure!(engine.recover_selected_source_hit("project", &foreign_project, &old_hit).is_err());
        ensure!(engine.recover_selected_source_hit("project", &foreign_root, &old_hit).is_err());
        ensure!(engine.recover_selected_source_hit("other", &old, &old_hit).is_err());
        for pk in ["other:memory:1:shared:logical:id", "project:history:1:shared:logical:id",
            "project:memory:01:shared:logical:id", "project:memory:not-a-number:shared:logical:id", "project:memory:1:another"] {
            let mut forged = old_hit.clone(); forged.pk = pk.into();
            ensure!(engine.recover_selected_source_hit("project", &old, &forged).is_err());
        }
        engine.prepare_source_snapshot("project", 1, 2, &[RelPath::new("source.rs")], &[], &cancel).await?;
        engine.index_docs("project", &[doc(2, "needle new body")], &cancel).await?;
        engine.seal_source_snapshot("project", 2)?;
        let current = engine.pin_source_generation("project", 2)?;
        let new_hit = engine.lexical_search(&query(2))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("New hit missing"))?;
        ensure!(engine.recover_selected_source_hit("project", &old, &new_hit).is_err());
        ensure!(engine.recover_selected_source_hit("project", &current, &old_hit).is_err());
        engine.purge_old_generations("project", 2).await?;
        ensure!(engine.recover_selected_source_hit("project", &old, &old_hit)?.ok_or_else(|| anyhow::anyhow!("Held old body lost"))?.2 == "needle old exact body");

        // Malformed stored fields must not be trusted just because the PK text
        // looks canonical. Each case rewrites only this owned corrupt fixture.
        for corruption in ["project", "namespace", "doc_id", "chunk_id", "generation", "path", "line", "missing"] {
            let mut writer = engine.tantivy_index.writer::<tantivy::TantivyDocument>(50_000_000)?;
            writer.delete_term(Term::from_field_text(engine.fields.pk, &old_hit.pk));
            if corruption != "missing" {
                writer.add_document(doc!(
                    engine.fields.pk => old_hit.pk.as_str(),
                    engine.fields.project_id => if corruption == "project" { "other" } else { "project" },
                    engine.fields.namespace => if corruption == "namespace" { "history" } else { "memory" },
                    engine.fields.doc_id => if corruption == "doc_id" { "other" } else { old_hit.doc_id.as_str() },
                    engine.fields.chunk_id => if corruption == "chunk_id" { 2u64 } else { 1u64 },
                    engine.fields.generation => if corruption == "generation" { 2u64 } else { 1u64 },
                    engine.fields.path => if corruption == "path" { "different.rs" } else { "source.rs" },
                    engine.fields.content => "corrupt row",
                    engine.fields.start_line => if corruption == "line" { 2u64 } else { 1u64 },
                    engine.fields.end_line => 1u64,
                ))?;
            }
            writer.commit()?;
            writer.wait_merging_threads()?;
            let recovered = engine.recover_selected_source_hit("project", &old, &old_hit);
            if corruption == "missing" {
                ensure!(recovered?.is_none(), "Missing exact PK must not fall back to the new logical revision");
            } else { ensure!(recovered.is_err(), "Corrupt stored {corruption} was accepted"); }
        }
        Ok(())
    }.await;
    temporary.close()?;
    result
}
