//! Opt-in native-only evidence. No production cache policy changes. Each case
//! runs in a fresh test process with the same query/observation sequence.
use super::*;
use anyhow::{Result, ensure};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const INITIAL_ROWS: usize = 64;
const VERSIONS: usize = 40;

#[repr(C)]
#[derive(Default)]
struct ProcessMemory {
    cb: u32,
    faults: u32,
    peak_resident: usize,
    resident: usize,
    peak_paged: usize,
    paged: usize,
    peak_nonpaged: usize,
    nonpaged: usize,
    pagefile: usize,
    peak_pagefile: usize,
    private: usize,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
}
#[link(name = "psapi")]
unsafe extern "system" {
    fn GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        memory: *mut ProcessMemory,
        bytes: u32,
    ) -> i32;
}
fn process_private() -> Result<usize> {
    let mut memory = ProcessMemory {
        cb: std::mem::size_of::<ProcessMemory>() as u32,
        ..Default::default()
    };
    // Current process, correctly sized/aligned stack-owned PM_COUNTERS_EX.
    let bytes = memory.cb;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut memory, bytes) };
    ensure!(
        ok != 0,
        "process memory query failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(memory.private)
}

type Rows = BTreeMap<String, (String, String, Vec<f32>, u64)>;

fn batch(rows: &Rows) -> Result<RecordBatch> {
    let pks: Vec<_> = rows.keys().cloned().collect();
    create_record_batch_with_gens(
        "project",
        "memory",
        &rows.values().map(|v| v.3).collect::<Vec<_>>(),
        &pks,
        &pks,
        &rows.values().map(|v| v.0.clone()).collect::<Vec<_>>(),
        &(0..rows.len() as u64).collect::<Vec<_>>(),
        &rows.values().map(|v| v.1.clone()).collect::<Vec<_>>(),
        &vec!["rust".to_owned(); rows.len()],
        &vec![None; rows.len()],
        &vec![None; rows.len()],
        &rows.values().map(|v| v.2.clone()).collect::<Vec<_>>(),
        4,
    )
}

async fn verify(table: &Table, expected: &Rows) -> Result<()> {
    let batches: Vec<RecordBatch> = table
        .vector_search(&[1., 0., 0., 0.])?
        .distance_type(lancedb::DistanceType::L2)
        .limit(expected.len() + 1)
        .execute()
        .await?
        .try_collect()
        .await?;
    let mut actual = Rows::new();
    for batch in batches {
        let strings = |column| {
            batch
                .column_by_name(column)
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
        };
        let pk = strings("pk");
        let hash = strings("content_hash");
        let path = strings("path");
        let generation = batch
            .column_by_name("generation")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let vectors = batch
            .column_by_name("vector")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        for row in 0..batch.num_rows() {
            let vector = vectors.value(row);
            let vector = vector.as_any().downcast_ref::<Float32Array>().unwrap();
            ensure!(
                actual
                    .insert(
                        pk.value(row).into(),
                        (
                            hash.value(row).into(),
                            path.value(row).into(),
                            vector.values().to_vec(),
                            generation.value(row)
                        )
                    )
                    .is_none(),
                "Duplicate vector after version/cache changes"
            );
        }
    }
    ensure!(
        &actual == expected,
        "Version/cache changes returned stale or incomplete vectors"
    );
    ensure!(
        table.count_rows(None).await? == expected.len(),
        "Physical row-count oracle failed"
    );
    Ok(())
}

async fn warm(table: &Table) -> Result<()> {
    let rows: Vec<RecordBatch> = table
        .vector_search(&[1., 0., 0., 0.])?
        .limit(10)
        .execute()
        .await?
        .try_collect()
        .await?;
    ensure!(!rows.is_empty());
    drop(rows);
    let rows: Vec<RecordBatch> = table
        .query()
        .only_if("pk = 'base-0000'")
        .execute()
        .await?
        .try_collect()
        .await?;
    ensure!(rows.iter().map(RecordBatch::num_rows).sum::<usize>() == 1);
    drop(rows);
    Ok(())
}

async fn passive(table: &Table, session: &lancedb::Session, step: usize) -> Result<Value> {
    let dataset = table
        .dataset()
        .ok_or_else(|| anyhow::anyhow!("native dataset missing"))?
        .get()
        .await?;
    let fragments = dataset.get_fragments().len();
    drop(dataset);
    let version = table.version().await?;
    let rows = table.count_rows(None).await?;
    Ok(
        json!({"step":step,"version":version,"live_rows":rows,"current_fragments":fragments,
        "approximate_session_items":session.approx_num_items(),"private_bytes":process_private()?,
        "cache_maintenance_requested":false}),
    )
}

#[test]
#[ignore = "explicit bounded native version/cache measurement; requires case environment"]
fn version_churn_metadata_cost() -> Result<()> {
    let mode = std::env::var("ENGRAM_S15_NATIVE_CASE")?;
    let metadata_bytes = std::env::var("ENGRAM_S15_METADATA_BYTES")?.parse::<usize>()?;
    ensure!(["overwrite", "append"].contains(&mode.as_str()));
    ensure!([32 * 1024 * 1024, 128 * 1024, 32 * 1024].contains(&metadata_bytes));
    let owned = tempfile::tempdir()?;
    let mut lifetime = None;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let root = owned.path().join("native");
        let connection = connect_with_cache_limits(&root,64*1024*1024,metadata_bytes).await?;
        let writer = create_new_table(&connection,"versions",4).await?;
        let original: Rows = (0..INITIAL_ROWS).map(|n|(format!("base-{n:04}"),
            ("original".into(),format!("source/{n}.rs"),vec![1.,n as f32/INITIAL_ROWS as f32,0.,0.],1))).collect();
        let mut expected = original.clone();
        upsert_vectors(&writer,vec![batch(&original)?]).await?;
        let old = connection.open_table("versions").execute().await?;
        old.checkout(old.version().await?).await?;
        let session = old.dataset().unwrap().get().await?.session();
        lifetime=Some(Arc::downgrade(&session));
        let mut observations = Vec::new();
        verify(&old,&original).await?; warm(&writer).await?;
        observations.push(passive(&writer,&session,0).await?);
        for step in 1..=VERSIONS {
            let changed = if mode=="overwrite" {
                expected.values_mut().for_each(|v|v.0=format!("version-{step:02}"));
                expected.clone()
            } else {
                let pk = format!("added-{step:04}");
                let value = (format!("version-{step:02}"),format!("source/added-{step}.rs"),vec![1.,step as f32/100.,0.,0.],step as u64+1);
                expected.insert(pk.clone(),value.clone());Rows::from([(pk,value)])
            };
            upsert_vectors(&writer,vec![batch(&changed)?]).await?;
            let latest = connection.open_table("versions").execute().await?;
            warm(&latest).await?;
            if [10,20,40].contains(&step) {
                verify(&latest,&expected).await?;
                observations.push(passive(&latest,&session,step).await?);
            }
        }
        let latest = connection.open_table("versions").execute().await?;
        let before_intrusive_items = session.approx_num_items();
        let before_intrusive_private = process_private()?;
        // Deliberately intrusive ONLY after the passive history is captured.
        let index = session.index_cache_stats().await;
        let metadata = session.metadata_cache_stats().await;
        let after_stats = json!({"maintenance_requested":true,"before_items":before_intrusive_items,
            "after_items":session.approx_num_items(),"before_private_bytes":before_intrusive_private,
            "after_private_bytes":process_private()?,
            "index":{"entries":index.num_entries,"bytes":index.size_bytes,"hits":index.hits,"misses":index.misses},
            "metadata":{"entries":metadata.num_entries,"bytes":metadata.size_bytes,"hits":metadata.hits,"misses":metadata.misses}});
        verify(&latest,&expected).await?;verify(&old,&original).await?;
        drop(old);
        let after_old_drop = json!({"items":session.approx_num_items(),"private_bytes":process_private()?});
        drop(latest);drop(writer);drop(connection);drop(session);
        let reopened = connect_with_cache_limits(&root,64*1024*1024,metadata_bytes).await?;
        verify(&reopened.open_table("versions").execute().await?,&expected).await?;
        drop(reopened);
        println!("S15_NATIVE_VERSION_CACHE {}",json!({"mode":mode,"metadata_budget_bytes":metadata_bytes,
            "initial_rows":INITIAL_ROWS,"versions":VERSIONS,"passive":observations,
            "intrusive_stats":after_stats,"after_old_dataset_drop":after_old_drop,
            "latest_values_old_snapshot_and_cold_reopen_verified":true,
            "limits":"Fresh test process per case; explicit stats getters run cache maintenance only after passive observations. Weighted bytes are not private memory. No production capacity change or S15 plateau claim."}));
        Ok::<_,anyhow::Error>(())
    });
    drop(runtime);
    if let Some(weak) = lifetime {
        ensure!(
            weak.strong_count() == 0,
            "Native session owner survived close"
        );
    }
    result?;
    owned.close()?;
    Ok(())
}
