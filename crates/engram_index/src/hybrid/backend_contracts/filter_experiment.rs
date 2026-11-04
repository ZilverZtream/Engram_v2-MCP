//! Bounded native-filter experiment. It does not enable a runtime expression cache.
use crate::hybrid::snapshots::{Catalog, Selection, Snapshot};
use crate::vector;
use anyhow::{Result, ensure};
use arrow_array::{Float32Array, StringArray, UInt64Array};
use datafusion_expr::{Expr, col, lit};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, HasQuery, QueryBase, QueryFilter};
use lancedb::{DistanceType, Table};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

const PATHS: usize = 512;
const DIM: usize = 384;
const ITERATIONS: usize = 20;

#[derive(Serialize)]
struct InputRow {
    id: String,
    path: String,
    generation: u64,
    namespace: &'static str,
    vector: Vec<f32>,
}

#[derive(Debug)]
struct Hit {
    id: String,
    path: String,
    generation: u64,
    namespace: String,
    distance: f32,
}

#[derive(Clone, Copy, Debug, Serialize)]
enum Mode { Sql, Typed, CompositeSql }

#[derive(Serialize)]
struct Observation {
    cpu_ns: Option<u64>,
    private_bytes: Option<usize>,
    resident_bytes: Option<usize>,
}

#[cfg(windows)]
fn observe() -> Result<Observation> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime { low: u32, high: u32 }
    #[repr(C)]
    struct Memory {
        cb: u32, faults: u32, peak_ws: usize, ws: usize,
        peak_paged: usize, paged: usize, peak_nonpaged: usize, nonpaged: usize,
        pagefile: usize, peak_pagefile: usize, private: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(process: *mut c_void, created: *mut FileTime, exited: *mut FileTime,
            kernel: *mut FileTime, user: *mut FileTime) -> i32;
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: *mut c_void, counters: *mut Memory, bytes: u32) -> i32;
    }
    let (mut created, mut exited, mut kernel, mut user) = (
        FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
    // Both FFI outputs contain only integer fields.
    let mut memory: Memory = unsafe { std::mem::zeroed() };
    memory.cb = std::mem::size_of::<Memory>() as u32;
    let process = unsafe { GetCurrentProcess() };
    ensure!(unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } != 0,
        "Process CPU counters unavailable: {}", std::io::Error::last_os_error());
    ensure!(unsafe { GetProcessMemoryInfo(process, &mut memory, memory.cb) } != 0,
        "Process memory counters unavailable: {}", std::io::Error::last_os_error());
    let ticks = |value: FileTime| (u64::from(value.high) << 32) + u64::from(value.low);
    Ok(Observation { cpu_ns: Some((ticks(kernel) + ticks(user)) * 100),
        private_bytes: Some(memory.private), resident_bytes: Some(memory.ws) })
}

#[cfg(not(windows))]
fn observe() -> Result<Observation> {
    Ok(Observation { cpu_ns: None, private_bytes: None, resident_bytes: None })
}

fn balanced(mut terms: Vec<Expr>) -> Expr {
    match terms.len() {
        0 => lit(false),
        1 => terms.pop().expect("one term"),
        count => {
            let right = terms.split_off(count / 2);
            balanced(terms).or(balanced(right))
        }
    }
}

fn typed_selector(snapshot: &Snapshot) -> Result<Expr> {
    Ok(match &snapshot.selection {
        Selection::Legacy(generation) => col("generation").eq(lit(*generation)),
        Selection::Files(files) => {
            // This prototype never accepts an unbounded application selection.
            ensure!(files.len() <= PATHS && files.keys().map(String::len).sum::<usize>() <= 64 * 1024,
                "Filter experiment exceeded its owned fixture bounds");
            let mut groups: BTreeMap<u64, Vec<Expr>> = BTreeMap::new();
            for (path, generation) in files {
                groups.entry(*generation).or_default().push(lit(path.clone()));
            }
            balanced(groups.into_iter().map(|(generation, paths)|
                col("generation").eq(lit(generation)).and(col("path").in_list(paths, false))).collect())
        }
    })
}

fn composite_selector(snapshot: &Snapshot) -> Result<String> {
    Ok(match &snapshot.selection {
        Selection::Legacy(_) => snapshot.sql().to_owned(),
        Selection::Files(files) if files.is_empty() => "false".into(),
        Selection::Files(files) => {
            ensure!(files.len() <= PATHS, "Composite experiment selection exceeds its bound");
            let values = files.iter().map(|(path, generation)|
                format!("'{generation}:{}'", path.replace('\'', "''"))).collect::<Vec<_>>().join(",");
            // Decimal UInt64 has no colon; the first colon separates generation
            // from the complete path without collisions or hash truncation.
            format!("path IS NOT NULL AND generation IS NOT NULL AND concat(CAST(generation AS STRING), ':', path) IN ({values})")
        }
    })
}

// Preserve the previous selector as the measured counterfactual even when the
// runtime chooses a different representation. Correctness uses the Rust oracle.
fn grouped_selector(snapshot: &Snapshot) -> String {
    let Selection::Files(files) = &snapshot.selection else { return snapshot.sql().to_owned(); };
    let mut groups: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
    for (path, revision) in files { groups.entry(*revision).or_default().push(path); }
    let terms = groups.into_iter().map(|(revision, paths)| {
        let values = paths.into_iter().map(|path| format!("'{}'", path.replace('\'', "''"))).collect::<Vec<_>>().join(",");
        format!("(generation = {} AND path IN ({values}))", crate::sql_literals::u64_literal(revision))
    }).collect::<Vec<_>>();
    fn join(terms: &[String]) -> String {
        match terms.len() {
            0 => "false".into(), 1 => terms[0].clone(),
            n => { let (a,b) = terms.split_at(n / 2); format!("({} OR {})", join(a), join(b)) },
        }
    }
    join(&terms)
}

async fn matching_pks(table: &Table, filter: String) -> Result<BTreeSet<String>> {
    let mut stream = table.query().only_if(filter).execute().await?;
    let mut ids = BTreeSet::new();
    while let Some(batch) = stream.try_next().await? {
        let column = batch.column_by_name("pk").and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| anyhow::anyhow!("Missing complete-match identity"))?;
        for i in 0..batch.num_rows() { ensure!(ids.insert(column.value(i).to_owned()), "Duplicate physical result identity"); }
    }
    Ok(ids)
}

fn filter(mode: Mode, sql: &str, typed: &Expr, composite: &str, namespace: &str) -> QueryFilter {
    match mode {
        Mode::Sql => QueryFilter::Sql(format!("namespace = '{}' AND ({sql})", namespace.replace('\'', "''"))),
        Mode::Typed => QueryFilter::Datafusion(typed.clone().and(col("namespace").eq(lit(namespace)))),
        Mode::CompositeSql => QueryFilter::Sql(format!("namespace = '{}' AND ({composite})", namespace.replace('\'', "''"))),
    }
}

async fn search(table: &Table, filter: QueryFilter, query: &[f32]) -> Result<Vec<Hit>> {
    let mut request = table.query().nearest_to(query)?.distance_type(DistanceType::Cosine).limit(10);
    ensure!(request.mut_query().prefilter, "Selection must precede top-K");
    request.mut_query().filter = Some(filter);
    let mut stream = request.execute().await?;
    let mut hits = Vec::new();
    while let Some(batch) = stream.try_next().await? {
        let strings = |name: &str| -> Result<&StringArray> {
            batch.column_by_name(name).and_then(|column| column.as_any().downcast_ref())
                .ok_or_else(|| anyhow::anyhow!("Missing string result column {name}"))
        };
        let ids = strings("doc_id")?;
        let paths = strings("path")?;
        let namespaces = strings("namespace")?;
        let generations = batch.column_by_name("generation").and_then(|column| column.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| anyhow::anyhow!("Missing UInt64 generation"))?;
        let distances = batch.column_by_name("_distance").and_then(|column| column.as_any().downcast_ref::<Float32Array>())
            .ok_or_else(|| anyhow::anyhow!("Missing vector distance"))?;
        for index in 0..batch.num_rows() {
            hits.push(Hit { id: ids.value(index).into(), path: paths.value(index).into(),
                namespace: namespaces.value(index).into(), generation: generations.value(index),
                distance: distances.value(index) });
        }
    }
    Ok(hits)
}

fn equivalent(left: &[Hit], right: &[Hit]) -> Result<()> {
    ensure!(left.len() == right.len(), "Different complete result lengths");
    for (a, b) in left.iter().zip(right) {
        ensure!(a.id == b.id && a.path == b.path && a.namespace == b.namespace
            && a.generation == b.generation && (a.distance - b.distance).abs() <= 0.000001,
            "SQL/typed complete result mismatch: {a:?} versus {b:?}");
    }
    Ok(())
}

fn validate_current(hits: &[Hit], paths: &[String]) -> Result<()> {
    let expected: BTreeSet<_> = (0..10).map(|index| format!("current-{index:04}")).collect();
    ensure!(hits.len() == 10 && hits.iter().map(|hit| hit.id.clone()).collect::<BTreeSet<_>>() == expected,
        "Closer obsolete/foreign rows must not displace eligible top-K results");
    for hit in hits {
        let index: usize = hit.id.trim_start_matches("current-").parse()?;
        ensure!(hit.namespace == "memory" && hit.path == paths[index]
            && hit.generation == (index / 4 + 2) as u64, "Incorrect independently expected source selection");
    }
    Ok(())
}

fn batch(rows: &[InputRow], namespace: &str) -> Result<arrow_array::RecordBatch> {
    let rows: Vec<_> = rows.iter().filter(|row| row.namespace == namespace).collect();
    vector::create_record_batch_with_gens("project", namespace,
        &rows.iter().map(|row| row.generation).collect::<Vec<_>>(),
        &rows.iter().map(|row| engram_core::build_pk("project", namespace, row.generation, &row.id)).collect::<Vec<_>>(),
        &rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>(),
        &rows.iter().map(|row| engram_core::ContentHash::compute(row.id.as_bytes()).0).collect::<Vec<_>>(),
        &(0..rows.len() as u64).collect::<Vec<_>>(),
        &rows.iter().map(|row| row.path.clone()).collect::<Vec<_>>(),
        &vec!["rust".into(); rows.len()], &vec![None; rows.len()], &vec![None; rows.len()],
        &rows.iter().map(|row| row.vector.clone()).collect::<Vec<_>>(), DIM)
}

// A separate oracle checks membership and metadata before top-K. Agreement
// between two SQL expressions alone would not establish the desired result.
#[tokio::test]
async fn native_revision_selector_layouts_preserve_filtered_top_k_and_unsigned_boundaries() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let paths: Vec<String> = (0..PATHS).map(|i| match i {
            0 => "scope/quote'colon:12.rs".into(),
            1 => r"scope/back\slash_%.rs".into(),
            2 => "scope/Ångström.rs".into(),
            3 => "scope/UPPER.RS".into(),
            _ => format!("{}/components/implementation/contracts/representative/nested/source/area/file{i:04}.rs", if i % 2 == 0 { "scope" } else { "other" }),
        }).collect();
        let author = |i: usize| match i % 3 {
            0 => Some("O'Brien".to_string()), 1 => Some("another".to_string()), _ => None,
        };
        let timestamp = |i: usize| match i % 5 {
            0 => None, 1 => Some(0), 2 => Some(i64::MAX as u64),
            3 => Some(1u64 << 63), _ => Some(u64::MAX),
        };
        let language = |i: usize| if i % 3 == 0 { "python" } else { "rust" };
        let checks = [
            ("all", "true".to_string()),
            ("prefix", "path LIKE 'scope/%'".into()),
            ("exclude", "path NOT LIKE 'scope/%'".into()),
            ("suffix", "(path LIKE '%.rs' OR path LIKE '%.RS')".into()),
            ("literal_wildcards", r"path LIKE 'scope/back\\slash\_\%.rs' ESCAPE '\'".into()),
            ("language", "language IN ('rust')".into()),
            ("author", "author = 'O''Brien'".into()),
            ("after", format!("timestamp >= {}", crate::sql_literals::u64_literal(1u64 << 63))),
            ("before", format!("timestamp < {}", crate::sql_literals::u64_literal(u64::MAX))),
            ("combined", format!("path LIKE 'scope/%' AND language = 'python' AND author = 'O''Brien' AND timestamp >= {} AND timestamp < {}",
                crate::sql_literals::u64_literal(1u64 << 63), crate::sql_literals::u64_literal(u64::MAX))),
        ];
        let allowed = |check: &str, i: usize| -> bool {
            match check {
                "all" => true,
                "prefix" => paths[i].starts_with("scope/"),
                "exclude" => !paths[i].starts_with("scope/"),
                "suffix" => paths[i].ends_with(".rs") || paths[i].ends_with(".RS"),
                "literal_wildcards" => i == 1,
                "language" => language(i) == "rust",
                "author" => author(i).as_deref() == Some("O'Brien"),
                "after" => timestamp(i).is_some_and(|v| v >= 1u64 << 63),
                "before" => timestamp(i).is_some_and(|v| v < u64::MAX),
                "combined" => paths[i].starts_with("scope/") && language(i) == "python"
                    && author(i).as_deref() == Some("O'Brien")
                    && timestamp(i).is_some_and(|v| v >= 1u64 << 63 && v < u64::MAX),
                _ => unreachable!(),
            }
        };
        let vector = |angle: f32| {
            let mut values = vec![0.0; DIM]; values[0] = 1.0; values[1] = angle; values
        };
        let connection = vector::connect(&temporary.path().join("vectors")).await?;
        let mut measurements = Vec::new();
        // The boundary layout retains generation zero alongside every unsigned
        // boundary. Other layouts vary physical fragmentation at fixed size.
        for (groups, selected_count) in [(1usize, PATHS), (4, PATHS), (16, PATHS), (128, PATHS), (5, PATHS), (17, PATHS), (16, 16)] {
            let boundary = groups == 5 || groups == 17;
            let revisions: Vec<u64> = if boundary {
                (0..(groups as u64 - 3)).chain([i64::MAX as u64, 1u64 << 63, u64::MAX]).collect()
            } else { (1..=groups as u64).collect() };
            let catalog = Catalog::new(temporary.path().join(format!("catalog-{groups}-{selected_count}")), None);
            let selected_paths = &paths[..selected_count];
            let mut previous = 0;
            for (group, &revision) in revisions.iter().enumerate() {
                if revision == 0 { continue; }
                let changed: Vec<_> = selected_paths.iter().enumerate().filter(|(i, _)| i % groups == group)
                    .map(|(_, p)| p.clone()).collect();
                catalog.prepare("project", "memory", previous, revision, selected_paths, &changed, &[])?;
                catalog.seal("project", "memory", revision)?;
                previous = revision;
            }
            let snapshot = catalog.pin("project", "memory", previous)?;
            let mut rows = Vec::new();
            for (i, path) in paths.iter().enumerate() {
                let revision = revisions[i % groups];
                rows.push(InputRow { id: format!("eligible-{i:04}"), path: path.clone(), generation: revision,
                    namespace: "memory", vector: vector(0.1 + i as f32 * 0.001) });
                // The logical ID deliberately stays the same across physical
                // revisions. Only exact PK membership proves correct selection.
                rows.push(InputRow { id: format!("eligible-{i:04}"), path: path.clone(), generation: if revision == 0 { 1 } else { 0 },
                    namespace: "memory", vector: vector(0.0) });
                if i < 16 {
                    rows.push(InputRow { id: format!("foreign-{i:04}"), path: path.clone(), generation: revision,
                        namespace: "history", vector: vector(0.0) });
                }
            }
            let table = vector::create_new_table(&connection, &format!("matrix_{groups}_{selected_count}"), DIM).await?;
            let mut batches = Vec::new();
            for namespace in ["memory", "history"] {
                let selected: Vec<_> = rows.iter().filter(|r| r.namespace == namespace).collect();
                let indices: Vec<usize> = selected.iter().map(|r| r.id.rsplit('-').next().unwrap().parse().unwrap()).collect();
                batches.push(vector::create_record_batch_with_gens("project", namespace,
                    &selected.iter().map(|r| r.generation).collect::<Vec<_>>(),
                    &selected.iter().map(|r| engram_core::build_pk("project", namespace, r.generation, &r.id)).collect::<Vec<_>>(),
                    &selected.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
                    &selected.iter().map(|r| engram_core::ContentHash::compute(r.id.as_bytes()).0).collect::<Vec<_>>(),
                    &(0..selected.len() as u64).collect::<Vec<_>>(),
                    &selected.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
                    &indices.iter().map(|&i| language(i).to_string()).collect::<Vec<_>>(),
                    &indices.iter().map(|&i| author(i)).collect::<Vec<_>>(),
                    &indices.iter().map(|&i| timestamp(i)).collect::<Vec<_>>(),
                    &selected.iter().map(|r| r.vector.clone()).collect::<Vec<_>>(), DIM)?);
            }
            vector::upsert_vectors(&table, batches).await?;
            let version = table.version().await?;
            let grouped = grouped_selector(&snapshot);
            let sql = grouped.as_str();
            let composite = composite_selector(&snapshot)?;
            let query = vector(0.0);
            for (check, condition) in &checks {
                let all_expected: BTreeSet<_> = (0..selected_count).filter(|&i| allowed(check, i))
                    .map(|i| engram_core::build_pk("project", "memory", revisions[i % groups], &format!("eligible-{i:04}"))).collect();
                let expected: BTreeSet<_> = (0..selected_count).filter(|&i| allowed(check, i)).take(10)
                    .map(|i| format!("eligible-{i:04}")).collect();
                let mut baseline: Option<Vec<Hit>> = None;
                for selector in [sql, composite.as_str(), snapshot.sql()] {
                    let filter = format!("namespace = 'memory' AND ({selector}) AND ({condition})");
                    ensure!(matching_pks(&table, filter.clone()).await? == all_expected,
                        "Complete selection oracle failed: groups={groups},check={check}");
                    let hits = search(&table, QueryFilter::Sql(filter), &query).await?;
                    ensure!(hits.len() == expected.len() && hits.iter().map(|h| h.id.clone()).collect::<BTreeSet<_>>() == expected,
                        "Independent top-K oracle failed: groups={groups},check={check},hits={hits:?}");
                    for hit in &hits {
                        let i: usize = hit.id.rsplit('-').next().unwrap().parse()?;
                        ensure!(hit.path == paths[i] && hit.generation == revisions[i % groups] && hit.namespace == "memory",
                            "Selected physical identity changed");
                    }
                    if let Some(previous) = &baseline { equivalent(previous, &hits)?; }
                    else { baseline = Some(hits); }
                }
            }
            // Alternate order and consume complete streams. These measurements
            // establish only same-dataset selector cost, not app performance.
            for round in 0..4 {
                for mode in if round % 2 == 0 { [false, true] } else { [true, false] } {
                    let selector = if mode { composite.as_str() } else { sql };
                    let before = observe()?;
                    let started = Instant::now();
                    for _ in 0..8 {
                        let hits = search(&table, QueryFilter::Sql(format!("namespace = 'memory' AND ({selector})")), &query).await?;
                        ensure!(hits.len() == 10 && hits.iter().all(|h| h.id.starts_with("eligible-")));
                    }
                    let after = observe()?;
                    measurements.push(serde_json::json!({"groups":groups,"selected_paths":selected_count,"round":round,"composite":mode,
                        "queries":8,"seconds":started.elapsed().as_secs_f64(),
                        "cpu_ns":before.cpu_ns.zip(after.cpu_ns).map(|(a,b)| b-a),
                        "sql_bytes":selector.len(),"before":before,"after":after}));
                }
            }
            ensure!(table.version().await? == version && table.count_rows(None).await? == rows.len());
        }
        println!("NATIVE_SELECTOR_LAYOUT_MATRIX {}", serde_json::json!({"paths":PATHS,"filter_cases":checks.len(),
            "layouts":7,"measurements":measurements,"limits":"Bounded owned native datasets; independent complete physical-PK and top-K oracles. Runtime selector membership checked; component timing does not establish whole-app resource improvement."}));
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn native_sql_and_typed_source_filters_preserve_top_k_and_measure_query_cost() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let paths: Vec<String> = (0..PATHS).map(|index| match index {
            0 => "quote'path.rs".into(), 1 => r"folder\path.rs".into(), 2 => "folder/Ångström.rs".into(),
            _ => format!("Source{index:04}.rs"),
        }).collect();
        let catalog = Catalog::new(temporary.path().join("catalog"), None);
        let legacy = catalog.pin("project", "memory", 1)?;
        for group in 0..PATHS / 4 {
            let generation = group as u64 + 2;
            catalog.prepare("project", "memory", generation - 1, generation,
                &paths, &paths[group * 4..group * 4 + 4], &[])?;
            catalog.seal("project", "memory", generation)?;
            catalog.protect_for_gc("project", "memory", generation)?;
        }
        let selected = catalog.pin("project", "memory", (PATHS / 4 + 1) as u64)?;
        let vector = |angle: f32| {
            let mut values = vec![0.0; DIM]; values[0] = 1.0; values[1] = angle; values
        };
        let mut rows = Vec::with_capacity(PATHS * 2 + 18);
        for (index, path) in paths.iter().enumerate() {
            rows.push(InputRow { id: format!("current-{index:04}"), path: path.clone(),
                generation: (index / 4 + 2) as u64, namespace: "memory",
                vector: vector(0.1 + index as f32 * 0.0003) });
            rows.push(InputRow { id: format!("old-{index:04}"), path: path.clone(), generation: 1,
                namespace: "memory", vector: vector(0.01 + index as f32 * 0.0002) });
            if index < 16 {
                rows.push(InputRow { id: format!("foreign-{index:04}"), path: path.clone(),
                    generation: (index / 4 + 2) as u64, namespace: "history",
                    vector: vector(0.005 + index as f32 * 0.0002) });
            }
        }
        rows.push(InputRow { id: "removed-only".into(), path: "removed-only.rs".into(),
            generation: 1, namespace: "memory", vector: vector(0.0) });
        rows.push(InputRow { id: "high-generation".into(), path: "high.rs".into(),
            generation: u64::MAX - 1, namespace: "memory", vector: vector(0.8) });
        let input_hash = blake3::hash(&serde_json::to_vec(&rows)?).to_hex().to_string();
        let connection = vector::connect(&temporary.path().join("vectors")).await?;
        let table = vector::create_new_table(&connection, "filter_experiment", DIM).await?;
        vector::upsert_vectors(&table, vec![batch(&rows, "memory")?, batch(&rows, "history")?]).await?;
        let version = table.version().await?;
        let query = vector(0.0);
        let sql = grouped_selector(&selected);
        let composite = composite_selector(&selected)?;
        let before_build = observe()?;
        let started = Instant::now();
        let typed = typed_selector(&selected)?;
        let build_seconds = started.elapsed().as_secs_f64();
        let after_build = observe()?;
        let sql_hits = search(&table, filter(Mode::Sql, &sql, &typed, &composite, "memory"), &query).await?;
        let typed_hits = search(&table, filter(Mode::Typed, &sql, &typed, &composite, "memory"), &query).await?;
        let composite_hits = search(&table, filter(Mode::CompositeSql, &sql, &typed, &composite, "memory"), &query).await?;
        validate_current(&sql_hits, &paths)?;
        validate_current(&typed_hits, &paths)?;
        equivalent(&sql_hits, &typed_hits)?;
        validate_current(&composite_hits, &paths)?;
        equivalent(&sql_hits, &composite_hits)?;

        // Keep the original view while advancing and collecting catalog state.
        let empty_generation = (PATHS / 4 + 2) as u64;
        catalog.prepare("project", "memory", empty_generation - 1, empty_generation, &paths, &[], &paths)?;
        catalog.seal("project", "memory", empty_generation)?;
        let empty = catalog.pin("project", "memory", empty_generation)?;
        catalog.protect_for_gc("project", "memory", empty_generation)?;
        let high_catalog = Catalog::new(temporary.path().join("high-catalog"), None);
        high_catalog.prepare_full("project", "memory", 0, u64::MAX - 1, &["high.rs".into()])?;
        high_catalog.seal("project", "memory", u64::MAX - 1)?;
        let high = high_catalog.pin("project", "memory", u64::MAX - 1)?;
        for snapshot in [&legacy, &empty, &high] {
            let expression = typed_selector(snapshot)?;
            let composite = composite_selector(snapshot)?;
            let a = search(&table, filter(Mode::Sql, snapshot.sql(), &expression, &composite, "memory"), &query).await?;
            let b = search(&table, filter(Mode::Typed, snapshot.sql(), &expression, &composite, "memory"), &query).await?;
            let c = search(&table, filter(Mode::CompositeSql, snapshot.sql(), &expression, &composite, "memory"), &query).await?;
            equivalent(&a, &b)?;
            equivalent(&a, &c)?;
            if Arc::ptr_eq(snapshot, &empty) { ensure!(a.is_empty(), "Empty selection returned rows"); }
            if Arc::ptr_eq(snapshot, &legacy) {
                ensure!(a.len() == 10 && a.iter().all(|hit| hit.generation == 1), "Old view did not survive publication/GC");
            }
            if Arc::ptr_eq(snapshot, &high) {
                ensure!(a.len() == 1 && a[0].generation == u64::MAX - 1, "UInt64 generations changed meaning");
            }
        }
        for namespace in ["absent", "history", "memory"] {
            let a = search(&table, filter(Mode::Sql, &sql, &typed, &composite, namespace), &query).await?;
            let b = search(&table, filter(Mode::Typed, &sql, &typed, &composite, namespace), &query).await?;
            let c = search(&table, filter(Mode::CompositeSql, &sql, &typed, &composite, namespace), &query).await?;
            equivalent(&a, &b)?;
            equivalent(&a, &c)?;
            ensure!(a.iter().all(|hit| hit.namespace == namespace), "Namespace leaked across queries");
            if namespace == "absent" { ensure!(a.is_empty()); }
            if namespace == "memory" { validate_current(&a, &paths)?; }
        }
        // Warm both forms. No embeddings, indexing or diagnostic output are timed.
        for mode in [Mode::Sql, Mode::Typed, Mode::CompositeSql] {
            search(&table, filter(mode, &sql, &typed, &composite, "memory"), &query).await?;
        }
        let clone_before = observe()?;
        let clone_started = Instant::now();
        for _ in 0..1000 { std::hint::black_box(typed.clone()); }
        let clone_seconds = clone_started.elapsed().as_secs_f64();
        let clone_after = observe()?;
        let mut measurements = Vec::new();
        let orders = [
            [Mode::Sql, Mode::Typed, Mode::CompositeSql], [Mode::Typed, Mode::CompositeSql, Mode::Sql],
            [Mode::CompositeSql, Mode::Sql, Mode::Typed], [Mode::CompositeSql, Mode::Typed, Mode::Sql],
            [Mode::Typed, Mode::Sql, Mode::CompositeSql], [Mode::Sql, Mode::CompositeSql, Mode::Typed],
        ];
        for (round, modes) in orders.into_iter().enumerate() {
            for mode in modes {
                let before = observe()?;
                let started = Instant::now();
                for _ in 0..ITERATIONS {
                    let hits = search(&table, filter(mode, &sql, &typed, &composite, "memory"), &query).await?;
                    validate_current(&hits, &paths)?;
                    equivalent(&sql_hits, &hits)?;
                }
                let seconds = started.elapsed().as_secs_f64();
                let after = observe()?;
                let cpu_ns = before.cpu_ns.zip(after.cpu_ns).map(|(a, b)| b - a);
                measurements.push(serde_json::json!({"round":round,"mode":mode,"iterations":ITERATIONS,
                    "seconds":seconds,"cpu_ns":cpu_ns,"before":before,"after":after}));
            }
        }
        ensure!(table.version().await? == version && table.count_rows(None).await? == rows.len(),
            "Read-only filter experiment changed its native data");
        println!("NATIVE_FILTER_EXPERIMENT {}", serde_json::json!({
            "paths":PATHS,"physical_revision_groups":PATHS/4,"rows":rows.len(),"dimension":DIM,
            "fixture_blake3":input_hash,"sql_bytes":sql.len(),"composite_sql_bytes":composite.len(),"build_seconds":build_seconds,
            "before_build":before_build,"after_build":after_build,"clone_iterations":1000,
            "clone_seconds":clone_seconds,"clone_before":clone_before,"clone_after":clone_after,
            "measurements":measurements,
            "limits":"Owned native dataset only; cached selector construction excluded, clone/namespace composition plus complete streams included in query timing. Process memory observations are not retained-object size or formal allocation accounting. No runtime cache enabled; no whole-app CPU cause or improvement established."
        }));
        Ok(())
    }.await;
    temporary.close()?;
    result
}
