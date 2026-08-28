//! Row-4 audit (docs/audits/04-concept-and-consumer-discovery.md) slice 4.
//!
//! Live repro on OciusX (2026-08-28): `grep_project personalliggare`
//! (case-insensitive) returned 151 chunks / 46 files and reported
//! `complete`, yet `Site/App_Code/api-json/api-broker.vb` — whose only
//! occurrence is the comment `' PERSONALLIGGARE` — was missing; the same
//! pattern with `case_sensitive: true` and upper-case text found it. The
//! content field is trigram-tokenised WITHOUT lowercasing, so the
//! term-index tier can only reach chunks that contain the pattern's exact
//! trigrams. A case-insensitive search must reach every case variant and
//! must not claim completeness otherwise.

use engram_core::{RelPath, namespaces};
use engram_index::grep::{FreshnessMode, GrepQuery, GrepTier, IndexedFileStat, grep};
use engram_index::{HybridSearchEngine, IndexDoc};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const FILES: &[(&str, &str)] = &[
    (
        "Site/App_Code/lower.vb",
        "Public Class lower\n    Dim personalliggare_id As Integer = 1\nEnd Class\n",
    ),
    (
        "Site/App_Code/api-json/api-broker.vb",
        "Public Class broker\n    ' PERSONALLIGGARE\n    Dim x As Integer = 2\nEnd Class\n",
    ),
    (
        "Site/modules/logs.aspx.vb",
        "Partial Class logs\n    e.Result = _io.InstallationsObjektProjektPropertiesLog.GetBySearch(a)\nEnd Class\n",
    ),
    (
        "Site/App_Code/unrelated.vb",
        "Public Class unrelated\n    Dim y As Integer = 3\nEnd Class\n",
    ),
];

async fn build(root: &Path) -> (HybridSearchEngine, Vec<IndexedFileStat>) {
    let tantivy_dir = root.join("tantivy");
    let lancedb_dir = root.join("lancedb");
    let project_root = root.join("project");
    let cfg = engram_core::Config::default();
    let engine = HybridSearchEngine::new(tantivy_dir, lancedb_dir, &cfg)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let mut docs = Vec::new();
    let mut stats = Vec::new();
    for (i, (rel, content)) in FILES.iter().enumerate() {
        let abs = project_root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, content).unwrap();
        docs.push(IndexDoc {
            generation: 1,
            chunk_id: i as u64,
            path: RelPath::new(rel),
            language: "vb".into(),
            content: (*content).to_string(),
            namespace: namespaces::NAMESPACE_MEMORY.into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: content.lines().count() as u32,
            doc_id: format!("doc_{i}"),
            content_hash: format!("hash_{i}"),
        });
        let meta = std::fs::metadata(&abs).unwrap();
        stats.push(IndexedFileStat {
            rel_path: (*rel).to_string(),
            size: meta.len(),
            mtime_secs: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            file_hash: None,
        });
    }
    engine.index_docs("p", &docs, &cancel).await.unwrap();
    (engine, stats)
}

fn query(pattern: &str, case_sensitive: Option<bool>) -> GrepQuery {
    GrepQuery {
        project_id: "p".into(),
        namespace: namespaces::NAMESPACE_MEMORY.into(),
        generation: 1,
        pattern: pattern.into(),
        regex: false,
        case_sensitive,
        multiline: false,
        path_prefix: None,
        language: None,
        context_before: 0,
        context_after: 0,
        max_results: 1000,
        freshness: FreshnessMode::Off,
    }
}

fn files_of(r: &engram_index::grep::GrepResult) -> Vec<String> {
    let mut f: Vec<String> = r
        .matches
        .iter()
        .map(|m| m.file_path.replace('\\', "/"))
        .collect();
    f.sort();
    f.dedup();
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case_insensitive_literal_reaches_every_case_variant_on_the_term_index_tier() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (engine, stats) = build(tmp.path()).await;
    let root = tmp.path().join("project");

    let r = grep(
        &engine,
        &root,
        &query("personalliggare", Some(false)),
        || Ok(stats.clone()),
    )
    .unwrap();
    assert_eq!(
        r.tier_used,
        GrepTier::TermIndex,
        "the literal must stay on the indexed tier (the fix is not a full scan)"
    );
    assert_eq!(
        files_of(&r),
        vec![
            "Site/App_Code/api-json/api-broker.vb".to_string(),
            "Site/App_Code/lower.vb".to_string(),
        ],
        "the upper-case-only chunk must be reached: {:?}",
        r.matches
    );

    // Mixed case inside an identifier — the audit's second live miss.
    let r = grep(
        &engine,
        &root,
        &query("installationsobjekt", Some(false)),
        || Ok(stats.clone()),
    )
    .unwrap();
    assert_eq!(files_of(&r), vec!["Site/modules/logs.aspx.vb".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case_sensitive_literal_still_matches_exact_case_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (engine, stats) = build(tmp.path()).await;
    let root = tmp.path().join("project");

    let r = grep(
        &engine,
        &root,
        &query("PERSONALLIGGARE", Some(true)),
        || Ok(stats.clone()),
    )
    .unwrap();
    assert_eq!(
        files_of(&r),
        vec!["Site/App_Code/api-json/api-broker.vb".to_string()]
    );
    let r = grep(
        &engine,
        &root,
        &query("personalliggare", Some(true)),
        || Ok(stats.clone()),
    )
    .unwrap();
    assert_eq!(files_of(&r), vec!["Site/App_Code/lower.vb".to_string()]);
}
