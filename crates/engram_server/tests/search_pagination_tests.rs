#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_index::IndexDoc;
use engram_server::{AppState, Engram, SearchMemoryRequest};
use rmcp::handler::server::tool::Parameters;
use tokio_util::sync::CancellationToken;

async fn fixture(big: usize) -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Entry.rs"), "pub fn entry() {}\n").unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().into_owned(),
            project_name: "paging".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let engine = state.get_project_cached(&pid).unwrap().search;
    for (ns, count) in [("quality_gate", big), ("memory_bank", 2)] {
        let mut docs = Vec::new();
        for i in 0..count {
            docs.push(IndexDoc {
                generation: 0,
                chunk_id: 0,
                doc_id: format!("{ns}:{i:04}"),
                content_hash: format!("{ns}-{i}"),
                path: engram_core::RelPath::new(&format!("__{ns}/{i:04}.md")),
                content: format!("pagingmarker record {i:04}"),
                language: "markdown".into(),
                namespace: ns.into(),
                author: None,
                timestamp: Some(if i % 2 == 0 { 1_000 } else { 2_000 }),
                start_line: 1,
                end_line: 1,
            });
        }
        engine
            .index_docs(&pid, &docs, &CancellationToken::new())
            .await
            .unwrap();
    }
    (tmp, engram, pid)
}
async fn page(engram: &Engram, pid: &str, offset: usize, size: usize) -> String {
    let result = engram
        .handle_search_memory(SearchMemoryRequest {
            project_id: pid.into(),
            query: "pagingmarker".into(),
            search_scope: "knowledge".into(),
            semantic: false,
            include_user_memory: false,
            offset,
            max_results: size,
            ..Default::default()
        })
        .await
        .unwrap();
    result
        .content
        .iter()
        .filter_map(|x| x.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}
fn ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("doc_id: ").map(str::to_owned))
        .collect()
}
#[tokio::test]
async fn uneven_namespaces_fill_pages_keep_small_source_and_form_one_prefix() {
    let (_tmp, e, pid) = fixture(20).await;
    let full = ids(&page(&e, &pid, 0, 22).await);
    assert_eq!(full.len(), 22);
    let first = ids(&page(&e, &pid, 0, 8).await);
    assert_eq!(first.len(), 8);
    assert_eq!(first, full[..8]);
    assert_eq!(
        first
            .iter()
            .filter(|id| id.starts_with("memory_bank:"))
            .count(),
        2
    );
    let mut union = first;
    union.extend(ids(&page(&e, &pid, 8, 8).await));
    union.extend(ids(&page(&e, &pid, 16, 8).await));
    assert_eq!(union, full);
    assert_eq!(
        union.iter().collect::<std::collections::HashSet<_>>().len(),
        22
    );
    assert!(ids(&page(&e, &pid, 22, 8).await).is_empty());
}
#[tokio::test]
async fn hard_cap_never_rewinds_and_tail_page_is_short() {
    let (_tmp, e, pid) = fixture(208).await;
    let full = ids(&page(&e, &pid, 0, 200).await);
    assert_eq!(full.len(), 200);
    let tail = page(&e, &pid, 195, 20).await;
    assert_eq!(ids(&tail), full[195..]);
    assert!(tail.contains("result_cap_reached: 200"));
    assert!(!tail.contains("offset=200 for the next page"));
    for offset in [200, 201, usize::MAX] {
        let output = page(&e, &pid, offset, 20).await;
        assert!(ids(&output).is_empty(), "offset {offset} repeated a page");
        assert!(output.contains("result_cap_reached: 200"));
        assert!(output.contains("No search was run"));
    }
    assert_eq!(ids(&page(&e, &pid, 0, 0).await), full[..1]);
    assert_eq!(ids(&page(&e, &pid, 199, 0).await), full[199..]);
}
#[test]
fn offset_sanitizer_preserves_position_until_cap_sentinel() {
    for (offset, size, expected) in [
        (195, 20, 195),
        (199, 0, 199),
        (200, 20, 200),
        (usize::MAX, usize::MAX, 200),
    ] {
        let r = SearchMemoryRequest {
            offset,
            max_results: size,
            ..Default::default()
        };
        assert_eq!(r.sanitized_offset(), expected);
        assert!((1..=200).contains(&r.sanitized_max_results()));
    }
}
