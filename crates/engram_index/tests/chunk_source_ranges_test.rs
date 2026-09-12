use engram_core::{Config, RelPath};
use engram_index::{chunking::semantic_chunk_lines, parsing::ExtractedSymbol, HybridSearchEngine, IndexDoc};
use engram_index::grep::{grep, FreshnessMode, GrepQuery, GrepTier};
use tokio_util::sync::CancellationToken;

fn source() -> String {
    (1..=40).map(|line| format!("L{line:03}: Token Token\n")).collect()
}

#[test]
fn overlap_ranges_describe_every_stored_line_including_short_first_chunks() {
    for newline in ["\n", "\r\n"] {
        let source = source().replace('\n', newline);
        let lines: Vec<_> = source.lines().collect();
        for limit in [35, 80, 140] {
            let symbols = [ExtractedSymbol {
                name:"Block".into(), kind:"function".into(), start_line:4, end_line:14, metadata:None,
            }];
            let chunks = semantic_chunk_lines(&source, limit, &symbols);
            assert!(chunks.len() > 2);
            for mut chunk in chunks {
                let expected = lines[(chunk.start_line - 1) as usize..chunk.end_line as usize].join("\n") + "\n";
                assert_eq!(chunk.content, expected, "range {}..{} must describe stored content", chunk.start_line, chunk.end_line);
                chunk.set_doc_id("Source.vb");
                assert_eq!(chunk.doc_id, engram_core::DocIdStr::compute("Source.vb", chunk.start_line, chunk.end_line, &chunk.content_hash));
            }
        }
    }
}

#[tokio::test]
async fn all_grep_tiers_report_source_lines_once_and_retain_retrievable_documents() {
    let temp = tempfile::tempdir().unwrap();
    let config = Config { embedding_backend:"fts_only".into(), ..Default::default() };
    let engine = HybridSearchEngine::new(temp.path().join("tantivy"), temp.path().join("lancedb"), &config).await.unwrap();
    let source = source();
    let docs: Vec<_> = semantic_chunk_lines(&source, 80, &[]).into_iter().enumerate().map(|(index, mut chunk)| {
        chunk.set_doc_id("Source.vb");
        IndexDoc {
            generation:1, chunk_id:index as u64, path:RelPath::new("Source.vb"), language:"vb".into(),
            content:chunk.content, namespace:"memory".into(), author:None, timestamp:None,
            start_line:chunk.start_line, end_line:chunk.end_line, doc_id:chunk.doc_id.0, content_hash:chunk.content_hash.0,
        }
    }).collect();
    engine.index_docs("p", &docs, &CancellationToken::new()).await.unwrap();
    for (pattern, regex, multiline, tier, count) in [
        ("Token", false, false, GrepTier::TermIndex, 40),
        ("Token.*", true, false, GrepTier::TermNarrowed, 40),
        ("Token\\nL", true, true, GrepTier::TermNarrowed, 39),
        ("[T][o][k][e][n]", true, true, GrepTier::FullScan, 80),
        ("[xyz]Token", true, false, GrepTier::FullScan, 0),
    ] {
        let query = GrepQuery {
            project_id:"p".into(), namespace:"memory".into(), generation:1, pattern:pattern.into(),
            regex, multiline, case_sensitive:Some(true), path_prefix:None, language:None,
            context_before:1, context_after:1, max_results:1000, freshness:FreshnessMode::Off,
        };
        let result = grep(&engine, temp.path(), &query, || Ok(Vec::new())).unwrap();
        assert_eq!(result.tier_used, tier);
        assert_eq!(result.matches.len(), count, "overlap must not duplicate source occurrences: {tier:?}");
        for found in &result.matches {
            assert_eq!(source.lines().nth(found.line as usize - 1).unwrap(), found.line_text);
            assert_eq!(&found.line_text[(found.column - 1) as usize..][..5], "Token");
            let id = found.doc_id.as_ref().unwrap();
            let doc = docs.iter().find(|doc| &doc.doc_id == id).unwrap();
            assert!(found.line >= doc.start_line && found.line <= doc.end_line);
            assert!(engine.get_doc_by_doc_id("p", "memory", 1, id).unwrap().is_some());
        }
        let again = grep(&engine, temp.path(), &query, || Ok(Vec::new())).unwrap();
        assert_eq!(result.matches.iter().map(|m| (&m.doc_id,m.line,m.column)).collect::<Vec<_>>(),
                   again.matches.iter().map(|m| (&m.doc_id,m.line,m.column)).collect::<Vec<_>>());
    }
}

#[tokio::test]
async fn character_classes_optional_groups_and_encoded_escapes_cannot_invent_required_literals() {
    let temp = tempfile::tempdir().unwrap();
    let config = Config { embedding_backend:"fts_only".into(), ..Default::default() };
    let engine = HybridSearchEngine::new(temp.path().join("tantivy"), temp.path().join("lancedb"), &config).await.unwrap();
    let content = "a\nbar\nAfoo\n";
    engine.index_docs("p", &[IndexDoc {
        generation:1, chunk_id:1, path:RelPath::new("Source.txt"), language:"text".into(),
        content:content.into(), namespace:"memory".into(), author:None, timestamp:None,
        start_line:1, end_line:3, doc_id:"escape-doc".into(), content_hash:"escape-hash".into(),
    }], &CancellationToken::new()).await.unwrap();
    for (pattern, expected_line) in [("[abc]",1), ("(longprefix)?bar",2), ("(absent|bar)",2), (r"\x41foo",3)] {
        let query = GrepQuery {
            project_id:"p".into(), namespace:"memory".into(), generation:1, pattern:pattern.into(),
            regex:true, multiline:false, case_sensitive:Some(true), path_prefix:None, language:None,
            context_before:0, context_after:0, max_results:100, freshness:FreshnessMode::Off,
        };
        let result = grep(&engine, temp.path(), &query, || Ok(Vec::new())).unwrap();
        assert!(result.matches.iter().any(|found| found.line == expected_line), "{pattern}: {:?}", result.matches);
    }
}
