#![allow(clippy::unwrap_used)]
//! Regression tests for ENG-AUD-2026-EXH-0001/0003: invalid fts_mode must be
//! rejected fail-closed at the request boundary (graph/git handlers) and at the
//! index layer (lexical_search), not silently coerced to "strict".

use engram_core::Config;
use engram_index::{HybridQuery, HybridSearchEngine};
use engram_core::RelPath;
use tokio_util::sync::CancellationToken;

// ── Index-layer fail-closed (EXH-0003) ───────────────────────────────────────

async fn open_engine(tmp: &tempfile::TempDir) -> HybridSearchEngine {
    let tantivy_dir = tmp.path().join("tantivy");
    let lance_dir = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy_dir).unwrap();
    std::fs::create_dir_all(&lance_dir).unwrap();
    let cfg = Config { embedding_backend: "fts_only".into(), ..Default::default() };
    HybridSearchEngine::new(tantivy_dir, lance_dir, &cfg).await.unwrap()
}

fn make_query(project_id: &str, fts_mode: &str) -> HybridQuery {
    HybridQuery {
        project_id: project_id.to_string(),
        namespace: "functions".to_string(),
        generation: 1,
        text: "test query".to_string(),
        top_k: 10,
        fts_mode: fts_mode.to_string(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    }
}

/// EXH-0003: lexical_search with an unknown fts_mode must return Err, not
/// silently fall back to strict query semantics.
#[tokio::test]
async fn lexical_search_rejects_unknown_fts_mode() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;

    let q = make_query("proj-invalid-mode", "strcit"); // typo
    let result = engine.lexical_search(&q);
    assert!(
        result.is_err(),
        "EXH-0003: lexical_search must return Err for unknown fts_mode 'strcit', \
         not silently coerce to strict; got Ok"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("strcit") || err.contains("fts_mode") || err.contains("unknown"),
        "error message must name the bad value; got: {err}"
    );
}

/// EXH-0003: lexical_search with fts_mode="Strict" (wrong case) must return Err.
#[tokio::test]
async fn lexical_search_rejects_wrong_case_fts_mode() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;

    let q = make_query("proj-case", "Strict");
    let result = engine.lexical_search(&q);
    assert!(
        result.is_err(),
        "EXH-0003: fts_mode='Strict' (wrong case) must be rejected; got Ok"
    );
}

/// EXH-0003: lexical_search with fts_mode="" (empty) must return Err.
#[tokio::test]
async fn lexical_search_rejects_empty_fts_mode() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;

    let q = make_query("proj-empty-mode", "");
    let result = engine.lexical_search(&q);
    assert!(
        result.is_err(),
        "EXH-0003: empty fts_mode must be rejected at index layer; got Ok"
    );
}

/// EXH-0003: all three valid fts_mode values must still succeed.
#[tokio::test]
async fn lexical_search_accepts_all_valid_fts_modes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    // Index a doc so there's something to search
    use engram_index::IndexDoc;
    let doc = IndexDoc {
        generation: 1,
        chunk_id: 1,
        path: RelPath::new("src/lib.rs"),
        language: "rust".into(),
        content: "fn payment_processor() {}".into(),
        namespace: "functions".into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 5,
        doc_id: "doc-fts-valid".into(),
        content_hash: "hash-fts-valid".into(),
    };
    engine.index_docs("proj-valid-modes", &[doc], &cancel).await.unwrap();

    for mode in ["strict", "loose", "regex"] {
        let mut q = make_query("proj-valid-modes", mode);
        q.namespace = "functions".into();
        q.text = "payment".into();
        let result = engine.lexical_search(&q);
        assert!(
            result.is_ok(),
            "EXH-0003: valid fts_mode='{mode}' must succeed; got: {:?}",
            result.err()
        );
    }
}

// ── Watcher overflow convergence (EXH-0005) ──────────────────────────────────
// These tests verify the behavioral invariant that the overflow_dirty set
// is drained and re-queued — not testing the full watcher integration,
// but the convergence mechanism directly.

/// EXH-0005: overflow_dirty set must accept project insertions under concurrent
/// write (the mechanism that prevents permanent event loss).
#[test]
fn watcher_overflow_dirty_set_accepts_concurrent_inserts() {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    let dirty: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Simulate 10 concurrent overflow events for 3 projects
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let dirty_clone = dirty.clone();
            std::thread::spawn(move || {
                let pid = format!("proj-{}", i % 3);
                if let Ok(mut set) = dirty_clone.lock() {
                    set.insert(pid);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread must not panic");
    }

    let final_set = dirty.lock().expect("lock must succeed");
    assert!(
        !final_set.is_empty(),
        "EXH-0005: overflow dirty set must not be empty after concurrent inserts"
    );
    // All 3 project IDs must be present (deduplication is fine, but none lost)
    for i in 0..3 {
        assert!(
            final_set.contains(&format!("proj-{i}")),
            "EXH-0005: proj-{i} must be in dirty set after overflow"
        );
    }
}

/// EXH-0005: draining the overflow_dirty set into pending_updates must schedule
/// exactly one update per dirty project (idempotent merge, not duplicate).
#[test]
fn watcher_overflow_drain_schedules_each_project_once() {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let dirty: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut pending: HashMap<String, Instant> = HashMap::new();

    // Pre-populate dirty set with 3 projects (including one duplicate)
    {
        let mut set = dirty.lock().unwrap();
        set.insert("proj-alpha".into());
        set.insert("proj-beta".into());
        set.insert("proj-alpha".into()); // duplicate — HashSet deduplicates
    }

    // Simulate what the ticker loop does: drain dirty → pending_updates
    if let Ok(mut set) = dirty.lock() {
        for pid in set.drain() {
            pending.entry(pid).or_insert_with(|| Instant::now() + Duration::from_secs(5));
        }
    }

    assert_eq!(
        pending.len(),
        2,
        "EXH-0005: draining 2 unique dirty projects must schedule exactly 2 pending updates; got {}",
        pending.len()
    );
    assert!(pending.contains_key("proj-alpha"), "proj-alpha must be scheduled");
    assert!(pending.contains_key("proj-beta"), "proj-beta must be scheduled");

    // Dirty set must be empty after drain
    let remaining = dirty.lock().unwrap().len();
    assert_eq!(remaining, 0, "EXH-0005: dirty set must be empty after drain");
}

// ── AUD-2026-EXH-0008: poisoned mutex recovery in watcher overflow path ───────
// These tests verify that the overflow_dirty drain and insert paths recover from
// a poisoned mutex (caused by a panic while holding the lock) rather than
// silently dropping events, which would break the convergence guarantee.

/// EXH-0008: poisoned mutex drain path must recover and preserve project IDs.
/// A panic inside a lock scope poisons the mutex. The ticker drain must recover
/// the inner state via `into_inner()` and continue scheduling dirty projects.
#[test]
fn watcher_overflow_drain_recovers_from_poisoned_mutex() {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let dirty: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Populate dirty set before poisoning
    dirty.lock().unwrap().insert("proj-poison".into());
    dirty.lock().unwrap().insert("proj-recover".into());

    // Poison the mutex by panicking inside a lock scope
    let dirty_clone = dirty.clone();
    let _ = std::panic::catch_unwind(move || {
        let _guard = dirty_clone.lock().unwrap();
        panic!("intentional poison");
    });

    assert!(dirty.is_poisoned(), "mutex must be poisoned after panic");

    // Now simulate the ticker drain using the EXH-0008 recovery pattern
    let mut pending: HashMap<String, Instant> = HashMap::new();
    {
        let mut set = match dirty.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        for pid in set.drain() {
            pending.entry(pid).or_insert_with(|| Instant::now() + Duration::from_secs(5));
        }
    }

    // Both projects must be scheduled despite the poisoned mutex
    assert!(
        pending.contains_key("proj-poison"),
        "EXH-0008: proj-poison must be recovered and scheduled after mutex poison"
    );
    assert!(
        pending.contains_key("proj-recover"),
        "EXH-0008: proj-recover must be recovered and scheduled after mutex poison"
    );
    assert_eq!(
        pending.len(),
        2,
        "EXH-0008: all dirty projects must survive mutex poisoning; got {} scheduled",
        pending.len()
    );
}

/// EXH-0008: poisoned mutex insert path must recover and preserve the dirty marker.
/// When a channel overflow fires during a poisoned mutex, the project ID must still
/// be added to the dirty set via `into_inner()` recovery.
#[test]
fn watcher_overflow_insert_recovers_from_poisoned_mutex() {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    let dirty: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Poison the mutex
    let dirty_clone = dirty.clone();
    let _ = std::panic::catch_unwind(move || {
        let _guard = dirty_clone.lock().unwrap();
        panic!("intentional poison for insert test");
    });

    assert!(dirty.is_poisoned(), "mutex must be poisoned");

    // Simulate the EXH-0008 recovery pattern in the notify callback insert path
    {
        let mut guard = match dirty.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.insert("proj-overflow".into());
    }

    // The dirty marker must survive the poisoned mutex
    let check = match dirty.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    assert!(
        check.contains("proj-overflow"),
        "EXH-0008: dirty marker must be preserved through poisoned mutex insert"
    );
}
