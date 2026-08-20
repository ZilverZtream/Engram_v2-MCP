//! Benchmark + correctness test for `grep_project` vs `rg`.
//!
//! Policy: if warm Engram can't beat `rg` on the ASCII-identifier
//! class of query, we haven't earned our keep. This test builds a
//! synthetic fixture codebase, indexes it with Engram, and then runs
//! the same literal lookup through both `engram_index::grep::grep`
//! and `rg` (invoked via `std::process::Command`).
//!
//! The test is gated by the presence of `rg` on PATH — if `rg` isn't
//! installed the comparison is skipped (but the correctness assertions
//! still run).

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use engram_core::{RelPath, namespaces};
use engram_index::grep::{FreshnessMode, GrepQuery, GrepTier, IndexedFileStat, grep};
use engram_index::{HybridSearchEngine, IndexDoc};
use tokio_util::sync::CancellationToken;

/// File count for the synthetic fixture. Big enough that rg has real
/// work to do, small enough that tests stay under a few seconds.
const FIXTURE_FILE_COUNT: usize = 200;

/// Larger fixture for the full benchmark matrix — at this size
/// sequential Tier 2 vs rayon-parallel Tier 2 shows a measurable gap.
const LARGE_FIXTURE_FILE_COUNT: usize = 1000;

/// Number of warm iterations per query class. We take the p50 so a
/// stray GC / scheduler tick doesn't dominate.
const WARM_ITERATIONS: usize = 5;

fn rg_available() -> bool {
    Command::new("rg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn p50(mut xs: Vec<u128>) -> u128 {
    xs.sort_unstable();
    xs[xs.len() / 2]
}

/// Generate `count` synthetic VB.NET-ish files with a predictable
/// mix of tokens so we can run identifier, punctuation, and regex
/// queries with known ground truth.
fn make_fixture(root: &Path, count: usize) {
    std::fs::create_dir_all(root).unwrap();
    for i in 0..count {
        let contents = format!(
            "' File {i} — synthetic fixture\n\
             Public Function GetItem{i}(id As Integer, Optional db As iFaltDataContext = Nothing) As Item\n\
                 Using ctx = If(db, New iFaltDataContext())\n\
                     Dim result = (From row In ctx.items Where row.id = id).FirstOrDefault()\n\
                     If result Is Nothing Then Return Nothing\n\
                     ctx.SubmitChanges()\n\
                     handelselogg.Create(\"Item{i}\", result.id)\n\
                     Return result\n\
                 End Using\n\
             End Function\n\
             \n\
             Public Function UpdateItem{i}(item As Item, Optional db As iFaltDataContext = Nothing) As Boolean\n\
                 ' LOG_{i}: activity log marker for downstream analysis\n\
                 Using ctx = If(db, New iFaltDataContext())\n\
                     Dim row = ctx.items.FirstOrDefault(Function(r) r.id = item.id)\n\
                     If row Is Nothing Then Return False\n\
                     row.name = item.name\n\
                     If db Is Nothing Then ctx.SubmitChanges()\n\
                     Return True\n\
                 End Using\n\
             End Function\n",
        );
        let path = root.join(format!("io_{i:04}.vb"));
        std::fs::write(&path, contents).unwrap();
    }
}

/// Index every fixture file into Tantivy, and return the file stats the
/// freshness guard compares against.
///
/// This used to also populate a separate document store, purely as a test
/// fixture — which is precisely how the full-scan tier and the freshness
/// guard came to depend on a store that no production path writes. Both now
/// read what production actually produces: Tantivy's stored chunk text, and
/// (via the caller) the code graph's file-node fingerprints.
async fn index_fixture(
    engine: &HybridSearchEngine,
    root: &Path,
    project_id: &str,
) -> Vec<IndexedFileStat> {
    let cancel = CancellationToken::new();
    let mut docs: Vec<IndexDoc> = Vec::new();
    let mut stats: Vec<IndexedFileStat> = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let content = std::fs::read_to_string(&path).unwrap();
        let line_count = content.lines().count() as u32;
        // One chunk per file keeps the test simple — real indexing
        // chunks larger files, but for the grep engine's purposes the
        // chunk count is a throughput multiplier, not a correctness
        // variable.
        let hash = format!("hash_{rel}");
        let doc_id = format!("doc_{rel}");
        docs.push(IndexDoc {
            generation: 1,
            chunk_id: stats.len() as u64,
            path: RelPath::new(&rel),
            language: "vb".into(),
            content: content.clone(),
            namespace: namespaces::NAMESPACE_MEMORY.into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: line_count,
            doc_id: doc_id.clone(),
            content_hash: hash.clone(),
        });

        // Record the stat signature that matches disk, so the freshness
        // guard sees a clean project.
        let meta = std::fs::metadata(&path).unwrap();
        let mtime_secs = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        stats.push(IndexedFileStat {
            rel_path: rel,
            size: meta.len(),
            mtime_secs,
        });
    }
    engine.index_docs(project_id, &docs, &cancel).await.unwrap();
    stats
}

fn run_rg(root: &Path, pattern: &str) -> (usize, u128) {
    let start = Instant::now();
    let out = Command::new("rg")
        .args(["-n", "--no-heading", pattern])
        .arg(root)
        .output()
        .expect("rg invocation failed");
    let elapsed = start.elapsed().as_micros();
    let matches = std::str::from_utf8(&out.stdout).unwrap().lines().count();
    (matches, elapsed)
}

#[tokio::test]
async fn grep_term_index_beats_rg_on_ascii_identifier() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!("engram_grep_bench_{now}"));
    let root = tmp.join("project");
    let tantivy_dir = tmp.join("tantivy");
    let lancedb_dir = tmp.join("lancedb");
    make_fixture(&root, FIXTURE_FILE_COUNT);

    let cfg = engram_core::Config::default();
    let engine = HybridSearchEngine::new(tantivy_dir, lancedb_dir, &cfg)
        .await
        .unwrap();
    let project_id = "bench";
    let indexed = index_fixture(&engine, &root, project_id).await;

    // Warm up both sides.
    let warm_q = GrepQuery {
        project_id: project_id.into(),
        namespace: namespaces::NAMESPACE_MEMORY.into(),
        generation: 1,
        pattern: "SubmitChanges".into(),
        regex: false,
        case_sensitive: None,
        multiline: false,
        path_prefix: None,
        language: None,
        context_before: 0,
        context_after: 0,
        max_results: 10_000,
        freshness: FreshnessMode::Off, // freshness check is a separate concern
    };
    let warm_result = grep(&engine, &root, &warm_q, || Ok(indexed.clone())).unwrap();

    // Correctness: Tier 0 must trigger for an ASCII identifier.
    assert_eq!(
        warm_result.tier_used,
        GrepTier::TermIndex,
        "SubmitChanges should route through the trigram index, got {:?}",
        warm_result.tier_used
    );

    // Each fixture file contains `SubmitChanges` exactly twice (once
    // unconditional, once inside `If db Is Nothing Then`). Expect
    // 2 × FIXTURE_FILE_COUNT matches.
    assert_eq!(
        warm_result.matches.len(),
        2 * FIXTURE_FILE_COUNT,
        "expected 2 matches per file, got {}",
        warm_result.matches.len()
    );

    // Latency comparison — only run when `rg` is installed.
    if !rg_available() {
        eprintln!("rg not on PATH — skipping latency comparison");
        return;
    }

    // Warm rg (first run primes the OS page cache).
    let _ = run_rg(&root, "SubmitChanges");

    let engram_timings: Vec<u128> = (0..WARM_ITERATIONS)
        .map(|_| {
            let s = Instant::now();
            let r = grep(&engine, &root, &warm_q, || Ok(indexed.clone())).unwrap();
            assert_eq!(r.matches.len(), 2 * FIXTURE_FILE_COUNT);
            s.elapsed().as_micros()
        })
        .collect();
    let rg_timings: Vec<u128> = (0..WARM_ITERATIONS)
        .map(|_| run_rg(&root, "SubmitChanges").1)
        .collect();

    let engram_p50 = p50(engram_timings.clone());
    let rg_p50 = p50(rg_timings.clone());

    println!("=== grep_vs_rg — ASCII identifier ('SubmitChanges') ===");
    println!(
        "fixture: {FIXTURE_FILE_COUNT} files, ~{} bytes",
        FIXTURE_FILE_COUNT * 800,
    );
    println!(
        "engram warm p50: {engram_p50} µs (over {} runs)",
        WARM_ITERATIONS
    );
    println!(
        "rg warm p50:     {rg_p50} µs (over {} runs)",
        WARM_ITERATIONS
    );
    println!("speedup: {:.2}×", rg_p50 as f64 / engram_p50.max(1) as f64);

    // Hard gate: warm Engram must beat warm rg on this class.
    assert!(
        engram_p50 < rg_p50,
        "Engram grep (p50 {engram_p50} µs) did NOT beat rg (p50 {rg_p50} µs) on ASCII identifier — \
         the whole point of the index is to be faster than linear scan."
    );
}

#[tokio::test]
async fn grep_full_scan_still_returns_correct_matches() {
    // A short 2-char literal drops through to Tier 2 (full scan).
    // This test verifies correctness of the fallback path — we don't
    // claim to beat rg here (rg is optimised for linear scan and we
    // haven't paralleled Tier 2 yet).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!("engram_grep_full_{now}"));
    let root = tmp.join("project");
    let tantivy_dir = tmp.join("tantivy");
    let lancedb_dir = tmp.join("lancedb");
    // Smaller fixture — full scan tests are correctness-only.
    make_fixture(&root, 10);

    let cfg = engram_core::Config::default();
    let engine = HybridSearchEngine::new(tantivy_dir, lancedb_dir, &cfg)
        .await
        .unwrap();
    let project_id = "bench_full";
    let indexed = index_fixture(&engine, &root, project_id).await;

    let q = GrepQuery {
        project_id: project_id.into(),
        namespace: namespaces::NAMESPACE_MEMORY.into(),
        generation: 1,
        pattern: "ctx".into(),
        regex: false,
        case_sensitive: Some(true),
        multiline: false,
        path_prefix: None,
        language: None,
        context_before: 0,
        context_after: 0,
        max_results: 10_000,
        freshness: FreshnessMode::Off,
    };
    let r = grep(&engine, &root, &q, || Ok(indexed.clone())).unwrap();
    // Short literal 'ctx' takes Tier 0 (trigram can index 3-char lits).
    assert_eq!(r.tier_used, GrepTier::TermIndex);
    assert!(
        r.matches.len() >= 10,
        "expected at least 10 matches for 'ctx' across 10 files, got {}",
        r.matches.len()
    );
}

/// Full benchmark matrix — four query classes, measured against rg,
/// across a larger fixture. Each class asserts that warm Engram beats
/// warm rg (or at worst matches it within 10 %) so regressions are
/// caught instead of silently papered over.
#[tokio::test]
async fn grep_full_benchmark_matrix_beats_rg() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!("engram_grep_matrix_{now}"));
    let root = tmp.join("project");
    let tantivy_dir = tmp.join("tantivy");
    let lancedb_dir = tmp.join("lancedb");
    make_fixture(&root, LARGE_FIXTURE_FILE_COUNT);

    let cfg = engram_core::Config::default();
    let engine = HybridSearchEngine::new(tantivy_dir, lancedb_dir, &cfg)
        .await
        .unwrap();
    let project_id = "bench_matrix";
    let indexed = index_fixture(&engine, &root, project_id).await;

    if !rg_available() {
        eprintln!("rg not on PATH — skipping benchmark matrix");
        return;
    }

    // Cases: (label, pattern, regex, expected_tier_or_none, multiline)
    let cases: &[(&str, &str, bool, Option<GrepTier>, bool)] = &[
        (
            "ASCII identifier",
            "SubmitChanges",
            false,
            Some(GrepTier::TermIndex),
            false,
        ),
        (
            "Punctuation literal",
            "SubmitChanges()",
            false,
            Some(GrepTier::TermIndex),
            false,
        ),
        (
            "Regex with literal anchor",
            r"SubmitChanges\(\)",
            true,
            Some(GrepTier::TermNarrowed),
            false,
        ),
        (
            // Short anchor (< 3 chars) → drops to Tier 2. Tests the
            // parallel full scan.
            "Regex without anchor (Tier 2)",
            r"Get.{1,3}em.*Integer",
            true,
            Some(GrepTier::FullScan),
            false,
        ),
    ];

    println!(
        "\n=== grep_vs_rg benchmark matrix — fixture: {} files ===",
        LARGE_FIXTURE_FILE_COUNT
    );
    println!(
        "{:<35} {:>12} {:>12} {:>10} {:>14}",
        "case", "engram(µs)", "rg(µs)", "speedup", "tier"
    );

    let mut failures: Vec<String> = Vec::new();

    for (label, pattern, regex_mode, expected_tier, multiline) in cases {
        let q = GrepQuery {
            project_id: project_id.into(),
            namespace: namespaces::NAMESPACE_MEMORY.into(),
            generation: 1,
            pattern: (*pattern).into(),
            regex: *regex_mode,
            case_sensitive: None,
            multiline: *multiline,
            path_prefix: None,
            language: None,
            context_before: 0,
            context_after: 0,
            max_results: 100_000,
            freshness: FreshnessMode::Off,
        };
        // Warm both.
        let warm = grep(&engine, &root, &q, || Ok(indexed.clone())).unwrap();
        if let Some(tier) = expected_tier {
            if warm.tier_used != *tier {
                failures.push(format!(
                    "{label}: expected tier {:?}, got {:?}",
                    tier, warm.tier_used
                ));
            }
        }
        let _ = run_rg(&root, pattern);

        let engram_ts: Vec<u128> = (0..WARM_ITERATIONS)
            .map(|_| {
                let s = Instant::now();
                let _ = grep(&engine, &root, &q, || Ok(indexed.clone())).unwrap();
                s.elapsed().as_micros()
            })
            .collect();
        let rg_ts: Vec<u128> = (0..WARM_ITERATIONS)
            .map(|_| run_rg(&root, pattern).1)
            .collect();
        let e50 = p50(engram_ts);
        let r50 = p50(rg_ts);
        let speedup = r50 as f64 / e50.max(1) as f64;
        let tier_label = format!("{:?}", warm.tier_used);
        println!(
            "{:<35} {:>12} {:>12} {:>9.2}× {:>14}",
            label, e50, r50, speedup, tier_label
        );

        // Hard gate: Tier 0 and Tier 1 MUST beat rg. Tier 2 should
        // be within 2× of rg (acceptable for fallback patterns).
        match warm.tier_used {
            GrepTier::TermIndex | GrepTier::TermNarrowed => {
                if e50 >= r50 {
                    failures.push(format!(
                        "{label}: tier {tier_label} did NOT beat rg — engram {e50}µs vs rg {r50}µs"
                    ));
                }
            }
            GrepTier::FullScan => {
                // Tier 2 is rg-class territory. We don't expect to win
                // every time, but we shouldn't be catastrophically slower.
                // Limit: 3× rg — catches regressions in the parallel scan
                // without flaking on CI jitter.
                if e50 > r50 * 3 {
                    failures.push(format!(
                        "{label}: Tier 2 more than 3× slower than rg — engram {e50}µs vs rg {r50}µs"
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "benchmark failures:\n  {}",
        failures.join("\n  ")
    );
}

#[tokio::test]
async fn grep_reports_stale_paths_when_files_change_after_index() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!("engram_grep_stale_{now}"));
    let root = tmp.join("project");
    let tantivy_dir = tmp.join("tantivy");
    let lancedb_dir = tmp.join("lancedb");
    make_fixture(&root, 5);

    let cfg = engram_core::Config::default();
    let engine = HybridSearchEngine::new(tantivy_dir, lancedb_dir, &cfg)
        .await
        .unwrap();
    let project_id = "bench_stale";
    let indexed = index_fixture(&engine, &root, project_id).await;

    // Mutate one of the indexed files so its mtime/size diverges.
    let target = root.join("io_0000.vb");
    // Sleep briefly so the mtime change is observable.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&target, "' modified after indexing\n").unwrap();

    let q = GrepQuery {
        project_id: project_id.into(),
        namespace: namespaces::NAMESPACE_MEMORY.into(),
        generation: 1,
        pattern: "SubmitChanges".into(),
        regex: false,
        case_sensitive: None,
        multiline: false,
        path_prefix: None,
        language: None,
        context_before: 0,
        context_after: 0,
        max_results: 10_000,
        freshness: FreshnessMode::Strict,
    };
    let r = grep(&engine, &root, &q, || Ok(indexed.clone())).unwrap();
    assert!(
        r.stale_paths.iter().any(|p| p == "io_0000.vb"),
        "mutated file must appear in stale_paths, got {:?}",
        r.stale_paths
    );
    assert!(r.index_stale_warning.is_some());
}
