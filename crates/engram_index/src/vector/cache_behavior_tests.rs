//! Cache pressure must not change durable vectors or mix independent roots.
use super::*;
use anyhow::{Result, ensure};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use std::collections::BTreeMap;

const ROWS: usize = 256;
const TABLES: usize = 12;
const CACHE_BYTES: usize = 32 * 1024;

fn batch(label: &str) -> Result<RecordBatch> {
    let pks: Vec<_> = (0..ROWS).map(|n| format!("shared-{n:04}")).collect();
    let hashes = vec![label.to_owned(); ROWS];
    let chunks: Vec<_> = (0..ROWS as u64).collect();
    let paths: Vec<_> = (0..ROWS).map(|n| format!("src/item{n}.rs")).collect();
    let languages = vec!["rust".to_owned(); ROWS];
    let absent = vec![None; ROWS];
    let vectors: Vec<_> = (0..ROWS)
        .map(|n| vec![1.0, n as f32 / ROWS as f32, 0.0, 0.0])
        .collect();
    create_record_batch(
        "project",
        "memory",
        1,
        &pks,
        &pks,
        &hashes,
        &chunks,
        &paths,
        &languages,
        &absent,
        &vec![None; ROWS],
        &vectors,
        4,
    )
}

async fn check_rows(table: &Table, label: &str) -> Result<()> {
    let nearest: Vec<RecordBatch> = table
        .vector_search(&[1.0, 0.0, 0.0, 0.0])?
        .distance_type(lancedb::DistanceType::L2)
        .limit(3)
        .execute()
        .await?
        .try_collect()
        .await?;
    let ordered: Vec<_> = nearest
        .iter()
        .flat_map(|batch| {
            let pks = batch
                .column_by_name("pk")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            (0..batch.num_rows())
                .map(|row| pks.value(row).to_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    ensure!(
        ordered == ["shared-0000", "shared-0001", "shared-0002"],
        "Eviction or reopening changed the nearest-vector ordering"
    );
    let batches: Vec<RecordBatch> = table
        .vector_search(&[1.0, 0.0, 0.0, 0.0])?
        .distance_type(lancedb::DistanceType::L2)
        .limit(ROWS + 1)
        .execute()
        .await?
        .try_collect()
        .await?;
    let mut actual = BTreeMap::new();
    for batch in batches {
        let pk = batch
            .column_by_name("pk")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let hash = batch
            .column_by_name("content_hash")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let path = batch
            .column_by_name("path")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for row in 0..batch.num_rows() {
            ensure!(
                actual
                    .insert(
                        pk.value(row).to_owned(),
                        (hash.value(row).to_owned(), path.value(row).to_owned())
                    )
                    .is_none(),
                "Cache pressure must not duplicate vectors"
            );
        }
    }
    let expected: BTreeMap<_, _> = (0..ROWS)
        .map(|n| {
            (
                format!("shared-{n:04}"),
                (label.to_owned(), format!("src/item{n}.rs")),
            )
        })
        .collect();
    ensure!(
        actual == expected,
        "Cache pressure changed the complete vector result"
    );

    // Load a real scalar index as well as the flat vector reader metadata.
    let batches: Vec<RecordBatch> = table
        .query()
        .only_if("pk = 'shared-0127'")
        .execute()
        .await?
        .try_collect()
        .await?;
    ensure!(batches.iter().map(RecordBatch::num_rows).sum::<usize>() == 1);
    let hash = batches
        .iter()
        .find(|batch| batch.num_rows() != 0)
        .unwrap()
        .column_by_name("content_hash")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    ensure!(
        hash.value(0) == label,
        "Indexed lookup crossed a root or snapshot"
    );
    Ok(())
}

async fn session(table: &Table) -> Result<Arc<lancedb::Session>> {
    Ok(table
        .dataset()
        .ok_or_else(|| anyhow::anyhow!("Expected local native dataset"))?
        .get()
        .await?
        .session())
}

/// Small budgets exercise the same connection path without filling production
/// caches. The uncapped control proves that this working set exceeds both
/// budgets; cache misses and exact queries prove reloading still works.
#[test]
fn pressure_preserves_vectors_old_readers_and_cold_reopen() -> Result<()> {
    // The runtime must shut down all native workers before TempDir teardown,
    // including during unwinding. Weak references do not prolong sessions.
    let temporary = tempfile::tempdir()?;
    let mut lifetimes = Vec::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let root = temporary.path().join("vectors");
        let connection = connect_with_cache_limits(&root, CACHE_BYTES, CACHE_BYTES).await?;
        for n in 0..TABLES {
            let table = create_new_table(&connection, &format!("vectors_{n:02}"), 4).await?;
            upsert_vectors(&table, vec![batch("original")?]).await?;
            table.create_index(&["pk"], lancedb::index::Index::BTree(Default::default()))
                .execute().await?;
        }
        let old = connection.open_table("vectors_00").execute().await?;
        old.checkout(old.version().await?).await?;
        let bounded = session(&old).await?;
        lifetimes.push(Arc::downgrade(&bounded));

        // A separate uncapped reader observes the identical on-disk working
        // set. This is a small diagnostic control, not the runtime path.
        let control = lancedb::connect(root.to_string_lossy().as_ref()).execute().await?;
        let control_table = control.open_table("vectors_00").execute().await?;
        let control_session = session(&control_table).await?;
        lifetimes.push(Arc::downgrade(&control_session));
        for n in 0..TABLES {
            check_rows(&control.open_table(&format!("vectors_{n:02}")).execute().await?, "original").await?;
        }
        let control_index = control_session.index_cache_stats().await;
        let control_metadata = control_session.metadata_cache_stats().await;
        ensure!(control_index.size_bytes > CACHE_BYTES && control_metadata.size_bytes > CACHE_BYTES,
            "Fixture must exceed both cache budgets: index={}, metadata={}",
            control_index.size_bytes, control_metadata.size_bytes);
        drop(control_session);
        drop(control_table);
        drop(control);

        for _ in 0..2 {
            for n in 0..TABLES {
                check_rows(&connection.open_table(&format!("vectors_{n:02}")).execute().await?, "original").await?;
            }
        }
        let before_index = bounded.index_cache_stats().await;
        let before_metadata = bounded.metadata_cache_stats().await;
        ensure!(before_index.size_bytes > 0 && before_index.size_bytes <= CACHE_BYTES);
        ensure!(before_metadata.size_bytes > 0 && before_metadata.size_bytes <= CACHE_BYTES);
        // Revisit the full working set. New misses with retained bytes still
        // bounded establish reload pressure, rather than an unfilled cache.
        for n in 0..TABLES {
            check_rows(&connection.open_table(&format!("vectors_{n:02}")).execute().await?, "original").await?;
        }
        let after_index = bounded.index_cache_stats().await;
        let after_metadata = bounded.metadata_cache_stats().await;
        ensure!(after_index.misses > before_index.misses && after_index.size_bytes <= CACHE_BYTES);
        ensure!(after_metadata.misses > before_metadata.misses && after_metadata.size_bytes <= CACHE_BYTES);

        let writer = connection.open_table("vectors_00").execute().await?;
        upsert_vectors(&writer, vec![batch("updated")?]).await?;
        check_rows(&writer, "updated").await?;
        check_rows(&old, "original").await?;
        drop(writer);
        drop(old);
        drop(bounded);
        drop(connection);

        let reopened = connect_with_cache_limits(&root, CACHE_BYTES, CACHE_BYTES).await?;
        for n in 0..TABLES {
            let table = reopened.open_table(&format!("vectors_{n:02}")).execute().await?;
            lifetimes.push(Arc::downgrade(&session(&table).await?));
            check_rows(&table, if n == 0 { "updated" } else { "original" }).await?;
        }
        println!("NATIVE_CACHE_PRESSURE {}", serde_json::json!({
            "budget_each_bytes":CACHE_BYTES,"tables":TABLES,"rows_each":ROWS,
            "control":{"index_bytes":control_index.size_bytes,"metadata_bytes":control_metadata.size_bytes},
            "bounded":{"index_bytes":after_index.size_bytes,"metadata_bytes":after_metadata.size_bytes,
                "index_reload_misses":after_index.misses-before_index.misses,
                "metadata_reload_misses":after_metadata.misses-before_metadata.misses},
            "limits":"Weighted retained-cache accounting only; not process or transient memory"}));
        Ok(())
    });
    drop(runtime);
    ensure!(
        lifetimes.iter().all(|weak| weak.upgrade().is_none()),
        "Native sessions must be released after readers, connections and runtime close"
    );
    temporary.close()?;
    result
}

/// The ordinary application connection must preserve root isolation and keep
/// its session only while a connection or reader still owns it.
#[test]
fn application_connections_isolate_roots_and_release_sessions() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let mut lifetimes = Vec::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let left = connect(&temporary.path().join("left")).await?;
        let right = connect(&temporary.path().join("right")).await?;
        let a = create_new_table(&left, "vectors", 4).await?;
        let b = create_new_table(&right, "vectors", 4).await?;
        upsert_vectors(&a, vec![batch("left")?]).await?;
        upsert_vectors(&b, vec![batch("right")?]).await?;
        let a_session = session(&a).await?;
        let b_session = session(&b).await?;
        lifetimes.push(Arc::downgrade(&a_session));
        lifetimes.push(Arc::downgrade(&b_session));
        ensure!(
            !Arc::ptr_eq(&a_session, &b_session),
            "Independent roots must own independent cache lifetimes"
        );
        drop(a_session);
        drop(b_session);
        for _ in 0..3 {
            check_rows(&a, "left").await?;
            check_rows(&b, "right").await?;
        }
        drop(left);
        drop(right);
        ensure!(
            lifetimes.iter().all(|weak| weak.upgrade().is_some()),
            "An active reader must remain usable after its connection closes"
        );
        check_rows(&a, "left").await?;
        check_rows(&b, "right").await?;
        Ok(())
    });
    drop(runtime);
    ensure!(lifetimes.iter().all(|weak| weak.upgrade().is_none()));
    temporary.close()?;
    result
}
