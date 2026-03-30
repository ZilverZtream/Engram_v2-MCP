#![allow(clippy::unwrap_used)]
//! D5/DS1 — copy-forward hash/read failure semantics.
//!
//! Proves that when a file read or UTF-8 decode fails during incremental
//! indexing, the behavior is explicit fail-open:
//!   - The job returns Ok (job does not abort)
//!   - The unreadable file appears in `skipped_files` (no silent data loss)
//!   - Other files in the same batch are indexed normally

use engram_core::{Config, RelPath};
use engram_index::HybridSearchEngine;
use std::io::Write;
use tokio_util::sync::CancellationToken;

async fn open_engine(tmp: &tempfile::TempDir) -> HybridSearchEngine {
    let tantivy_dir = tmp.path().join("tantivy");
    let lance_dir = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy_dir).unwrap();
    std::fs::create_dir_all(&lance_dir).unwrap();
    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    HybridSearchEngine::new(tantivy_dir, lance_dir, &cfg)
        .await
        .unwrap()
}

/// D5/DS1: when a file contains non-UTF8 bytes, `index_files` must:
///   1. Return Ok(stats) — fail-open, job does not abort
///   2. Include the unreadable file in stats.skipped_files
///   3. Successfully index sibling valid files in the same batch
#[tokio::test]
async fn ds1_non_utf8_file_is_skipped_not_fatal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;

    let project_dir = tmp.path().join("proj");
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create a valid file that should be indexed.
    let good_file = project_dir.join("good.rs");
    std::fs::write(&good_file, b"fn hello_world() { println!(\"hi\"); }").unwrap();

    // Create a file with non-UTF8 bytes to simulate a binary/corrupt file
    // that slips past the binary-detection heuristic (e.g. .md extension).
    let bad_file = project_dir.join("corrupt.md");
    {
        let mut f = std::fs::File::create(&bad_file).unwrap();
        // Write bytes that are invalid UTF-8 (0xff 0xfe starts a UTF-16 BOM, but
        // together with 0x80-0xBF continuations they are invalid UTF-8).
        f.write_all(b"\xff\xfe\x80invalid-utf8-content\x81\x82")
            .unwrap();
    }

    let cancel = CancellationToken::new();
    let files = vec![good_file.clone(), bad_file.clone()];

    let result = engine
        .index_files(
            "ds1-proj",
            "memory",
            1,
            &project_dir,
            files,
            2048,
            &cancel,
            |_, _| {},
        )
        .await;

    assert!(
        result.is_ok(),
        "D5/DS1: index_files must return Ok even when a file fails UTF-8 decode; got: {:?}",
        result.unwrap_err()
    );

    let stats = result.unwrap();

    // The corrupt file must appear in skipped_files (no silent data loss).
    let skipped_paths: Vec<&str> = stats
        .skipped_files
        .iter()
        .map(|(p, _)| p.as_str())
        .collect();
    assert!(
        skipped_paths.iter().any(|p| p.contains("corrupt")),
        "D5/DS1: corrupt.md must appear in skipped_files; got: {:?}",
        skipped_paths
    );

    // The valid file must have been indexed (appears as a doc in the engine).
    let count = engine.count_docs("ds1-proj").unwrap();
    assert!(
        count > 0,
        "D5/DS1: valid sibling file must be indexed even when another file fails; docs=0"
    );
}

/// D5/DS1: when a file disappears between scan-time and read-time (race
/// condition), `index_files` must return Ok and record the file in
/// skipped_files rather than panicking or aborting.
#[tokio::test]
async fn ds1_disappeared_file_is_skipped_not_fatal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;

    let project_dir = tmp.path().join("proj2");
    std::fs::create_dir_all(&project_dir).unwrap();

    // Good file stays throughout.
    let good_file = project_dir.join("stays.rs");
    std::fs::write(&good_file, b"fn stays() {}").unwrap();

    // Vanishing file: we pass a path that doesn't exist yet at index time.
    let vanished_file = project_dir.join("vanished.rs");
    // Do NOT create the file — simulating a file that was listed but disappeared.

    let cancel = CancellationToken::new();
    let files = vec![good_file.clone(), vanished_file.clone()];

    let result = engine
        .index_files(
            "ds1-vanish-proj",
            "memory",
            1,
            &project_dir,
            files,
            2048,
            &cancel,
            |_, _| {},
        )
        .await;

    assert!(
        result.is_ok(),
        "D5/DS1: index_files must return Ok even when a listed file does not exist; got: {:?}",
        result.unwrap_err()
    );

    let stats = result.unwrap();

    // The vanished file must appear in skipped_files.
    let skipped_paths: Vec<&str> = stats
        .skipped_files
        .iter()
        .map(|(p, _)| p.as_str())
        .collect();
    assert!(
        !skipped_paths.is_empty(),
        "D5/DS1: vanished file must appear in skipped_files; skipped_files is empty"
    );

    // Good file must still be indexed.
    let count = engine.count_docs("ds1-vanish-proj").unwrap();
    assert!(
        count > 0,
        "D5/DS1: valid file must be indexed even when sibling vanishes; docs=0"
    );
}
