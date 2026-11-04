//! Compare equivalent source retrieval on one frozen, fragmented owned index.
//! This is a cost experiment, not a runtime reader cache or an RSS claim.
use super::*;
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;
use tantivy::{IndexReader, ReloadPolicy, Searcher};
use tantivy::query::Query;

#[cfg(all(windows, feature="vector"))]
fn process_memory_observation() -> Result<serde_json::Value> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct Memory {
        size: u32, faults: u32, peak_resident: usize, resident: usize,
        peak_paged_pool: usize, paged_pool: usize,
        peak_nonpaged_pool: usize, nonpaged_pool: usize,
        commit: usize, peak_commit: usize, private: usize,
    }
    #[link(name="kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
    }
    #[link(name="psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: *mut c_void, counters: *mut Memory, size: u32) -> i32;
    }
    let mut memory = Memory::default();
    memory.size = std::mem::size_of::<Memory>() as u32;
    let size = memory.size;
    let mut handles = 0;
    ensure!(unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut memory, size) } != 0,
        "Process memory observation failed: {}", std::io::Error::last_os_error());
    ensure!(unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) } != 0,
        "Process handle observation failed: {}", std::io::Error::last_os_error());
    Ok(serde_json::json!({"private_bytes":memory.private,"resident_bytes":memory.resident,
        "peak_commit_bytes":memory.peak_commit,"handles":handles,
        "charged_paged_pool_bytes":memory.paged_pool,"charged_nonpaged_pool_bytes":memory.nonpaged_pool}))
}

#[cfg(all(not(windows), feature="vector"))]
fn process_memory_observation() -> Result<serde_json::Value> {
    Ok(serde_json::json!({"available":false,"reason":"This experiment samples Windows process counters"}))
}

#[cfg(feature="vector")]
async fn native_cache_observation(engine: &HybridSearchEngine, generation: u64) -> Result<serde_json::Value> {
    let epoch = engine.pin_vector_epoch("project")?;
    let table = engine.open_vector_epoch_table(&epoch).await?;
    let dataset = table.dataset().ok_or_else(|| anyhow::anyhow!("Expected owned native table"))?.get().await?;
    let session = dataset.session();
    let metadata = session.metadata_cache_stats().await;
    let index = session.index_cache_stats().await;
    Ok(serde_json::json!({"generation":generation,"fragments":dataset.count_fragments(),
        "native_metadata_cache":{"weighted_bytes":metadata.size_bytes,"entries":metadata.num_entries,"hits":metadata.hits,"misses":metadata.misses},
        "native_index_cache":{"weighted_bytes":index.size_bytes,"entries":index.num_entries,"hits":index.hits,"misses":index.misses},
        "process":process_memory_observation()?}))
}

/// Observe real native cache retention without changing its capacities, holding
/// an old reader, forcing compaction, or suppressing normal collection.
#[cfg(feature="vector")]
#[tokio::test]
async fn native_fragmented_edit_memory_observation_keeps_selected_views_complete() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let engine = HybridSearchEngine::new(temporary.path().join("tantivy"), temporary.path().join("vectors"),
            &engram_core::Config { embedding_backend:"local".into(), ..Default::default() }).await?;
        let cancel = CancellationToken::new();
        let make = |file, chunk, generation| {
            let mut doc = document(file, chunk, generation);
            doc.chunk_id = (file * CHUNKS + chunk) as u64;
            doc
        };
        let initial: Vec<_> = (0..FILES).flat_map(|file| (0..CHUNKS).map(move |chunk| make(file, chunk, 1))).collect();
        engine.index_docs("project", &initial, &cancel).await?;
        let mut generations = vec![1u64; FILES];
        let query = |generation, top_k| HybridQuery { project_id:"project".into(),namespace:"memory".into(),generation,
            text:"needle".into(),top_k,fts_mode:"loose".into(),include_path_prefixes:None,exclude_path_prefixes:None,
            include_path_suffixes:None,language_filters:None,author_filter:None,date_after:None,date_before:None,use_mmr:true };
        let mut observations = Vec::new();
        observations.push(native_cache_observation(&engine, 1).await?);
        let started = Instant::now();
        let cpu_before = cpu_ns()?;
        for edit in 0..FILES - 1 {
            let generation = edit as u64 + 2;
            let path = RelPath::new(&format!("src/file{edit:04}.rs"));
            engine.prepare_source_snapshot("project", generation - 1, generation, &[path], &[], &cancel).await?;
            let changed: Vec<_> = (0..CHUNKS).map(|chunk| make(edit, chunk, generation)).collect();
            engine.index_docs("project", &changed, &cancel).await?;
            engine.seal_source_snapshot("project", generation)?;
            engine.purge_old_generations("project", generation).await?;
            generations[edit] = generation;
            // Match the application's two default search requests and one
            // native vector request per edit, retaining full candidate windows.
            for _ in 0..2 {
                let hits = engine.search_ranked_prefix(&query(generation, 200), None, &cancel, 3).await?;
                ensure!(hits.len() == 3);
                for hit in hits {
                    let expected = generations.iter().enumerate().flat_map(|(file, &physical)|
                        (0..CHUNKS).map(move |chunk| make(file, chunk, physical)))
                        .find(|doc| doc.doc_id == hit.doc_id).ok_or_else(|| anyhow::anyhow!("Unexpected logical source hit"))?;
                    ensure!(hit.pk == build_pk("project", "memory", expected.generation, &expected.doc_id));
                }
            }
            ensure!(engine.vector_search(&query(generation, 10), &cancel).await?.len() == 10);
            if [8,16,32,48,63].contains(&(edit + 1)) {
                observations.push(native_cache_observation(&engine, generation).await?);
                let expected: BTreeSet<_> = generations.iter().enumerate().flat_map(|(file, &physical)|
                    (0..CHUNKS).map(move |chunk| make(file, chunk, physical)))
                    .map(|doc| build_pk("project", "memory", doc.generation, &doc.doc_id)).collect();
                let all = query(generation, FILES * CHUNKS + 1);
                let lexical = engine.lexical_search(&all)?;
                let native = engine.vector_search(&all, &cancel).await?;
                ensure!(lexical.len() == expected.len() && native.len() == expected.len());
                ensure!(lexical.into_iter().map(|hit|hit.pk).collect::<BTreeSet<_>>() == expected);
                ensure!(native.into_iter().map(|hit|hit.pk).collect::<BTreeSet<_>>() == expected,
                    "Native memory observations must preserve the full independent physical selection oracle");
            }
        }
        let frozen_generation = FILES as u64;
        let frozen_query = query(frozen_generation, 200);
        let reference = engine.search_ranked_prefix(&frozen_query, None, &cancel, 3).await?
            .into_iter().map(|hit|hit.pk).collect::<Vec<_>>();
        let mut query_only_observations = vec![serde_json::json!({"groups":0,
            "state":native_cache_observation(&engine, frozen_generation).await?})];
        let query_only_started = Instant::now();
        let query_only_cpu_before = cpu_ns()?;
        for group in 1..=50 {
            for _ in 0..2 {
                let hits = engine.search_ranked_prefix(&frozen_query, None, &cancel, 3).await?;
                ensure!(hits.into_iter().map(|hit|hit.pk).collect::<Vec<_>>() == reference,
                    "Repeated reads of the same frozen source must retain ordered results");
            }
            ensure!(engine.vector_search(&query(frozen_generation,10), &cancel).await?.len() == 10);
            if [10,25,50].contains(&group) {
                query_only_observations.push(serde_json::json!({"groups":group,
                    "state":native_cache_observation(&engine, frozen_generation).await?}));
            }
        }
        println!("NATIVE_FRAGMENT_MEMORY_EXPERIMENT {}",serde_json::json!({"files":FILES,"chunks_per_file":CHUNKS,
            "distinct_edits":FILES-1,"seconds":started.elapsed().as_secs_f64(),"cpu_ns":cpu_before.zip(cpu_ns()?).map(|(x,y)|y-x),
            "observations":observations,"query_only":{"seconds":query_only_started.elapsed().as_secs_f64(),
                "cpu_ns":query_only_cpu_before.zip(cpu_ns()?).map(|(x,y)|y-x),"observations":query_only_observations},
            "limits":"Owned engine component, not whole MCP application. Native cache weights are estimates, not allocator/RSS accounting. No changed cache capacities, forced compaction, held old readers or hard memory limit. All checkpoint lexical/native physical-PK sets checked independently, frozen query ordering verified. No system pool ownership or SSD-wear claim."}));
        // Compare cache policies on the identical frozen physical store in one
        // binary. Keep providers, source selection, queries and readers shared;
        // replace only the control's native connection/session. This is an
        // in-process cost observation, not a new runtime connection path.
        let mut default_cache_control = engine.clone();
        let control_session = Arc::new(lancedb::Session::default());
        let control_connection = lancedb::connect(engine._lance_dir.to_string_lossy().as_ref())
            .session(control_session.clone()).execute().await?;
        default_cache_control.native_store = Arc::new(native_store::Store::new(control_connection, control_session));
        for _ in 0..3 {
            default_cache_control.search_ranked_prefix(&frozen_query, None, &cancel, 3).await?;
            default_cache_control.vector_search(&query(frozen_generation, 10), &cancel).await?;
        }
        let mut cache_policy_costs = Vec::new();
        // Reverse the order between matched pairs to expose time/order drift.
        for bounded in [true, false, false, true, false, true, true, false] {
            let candidate = if bounded { &engine } else { &default_cache_control };
            let before = cpu_ns()?;
            let clock = Instant::now();
            for _ in 0..3 {
                for _ in 0..2 {
                    let hits = candidate.search_ranked_prefix(&frozen_query, None, &cancel, 3).await?;
                    ensure!(hits.into_iter().map(|hit|hit.pk).collect::<Vec<_>>() == reference,
                        "Cache policy must preserve the ordered source results");
                }
                ensure!(candidate.vector_search(&query(frozen_generation,10), &cancel).await?.len() == 10);
            }
            let elapsed = clock.elapsed().as_secs_f64();
            let cpu = before.zip(cpu_ns()?).map(|(a,b)| b-a);
            cache_policy_costs.push(serde_json::json!({"policy":if bounded {"engram_bounded"} else {"lance_default"},
                "groups":3,"seconds":elapsed,"cpu_ns":cpu,
                "state":native_cache_observation(candidate, frozen_generation).await?}));
        }
        println!("NATIVE_CACHE_POLICY_COST {}",serde_json::json!({
            "observations":cache_policy_costs,
            "limits":"Same frozen store and binary; interleaved warm reads with no edits. Native connection/session is the only counterfactual. This small working set does not fill production budgets; no global performance or RAM plateau claim."}));
        Ok(())
    }.await;
    temporary.close()?;
    result
}

const FILES: usize = 64;
const CHUNKS: usize = 16;
const EDITS: usize = 40;
const ITERATIONS: usize = 24;

#[derive(Clone, Copy, Debug, serde::Serialize)]
enum Mode { PerCallReaderLogicalRecovery, SharedReaderLogicalRecovery, RequestSearcherPhysicalRecovery, PerCallReaderPhysicalRecovery }

#[cfg(windows)]
fn cpu_ns() -> Result<Option<u64>> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime { low: u32, high: u32 }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(process: *mut c_void, created: *mut FileTime, exited: *mut FileTime,
            kernel: *mut FileTime, user: *mut FileTime) -> i32;
    }
    let (mut created, mut exited, mut kernel, mut user) = (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
    ensure!(unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) } != 0,
        "Process CPU observation failed: {}", std::io::Error::last_os_error());
    let ticks = |t: FileTime| (u64::from(t.high) << 32) + u64::from(t.low);
    Ok(Some((ticks(kernel) + ticks(user)) * 100))
}

#[cfg(not(windows))]
fn cpu_ns() -> Result<Option<u64>> { Ok(None) }

fn document(file: usize, chunk: usize, generation: u64) -> IndexDoc {
    let content = format!("needle source file{file} chunk{chunk} revision{generation}");
    IndexDoc { generation, chunk_id: chunk as u64, path: RelPath::new(&format!("src/file{file:04}.rs")),
        language: "rust".into(), content: content.clone(), namespace: "memory".into(),
        author: None, timestamp: None, start_line: chunk as u32 + 1, end_line: chunk as u32 + 1,
        doc_id: format!("file{file:04}-chunk{chunk:04}"), content_hash: ContentHash::compute(content.as_bytes()).0 }
}

fn logical_lookup(engine: &HybridSearchEngine, view: &snapshots::Lease, doc_id: &str) -> BooleanQuery {
    BooleanQuery::new(vec![
        (Occur::Must, Box::new(TermQuery::new(Term::from_field_text(engine.fields.project_id, "project"), IndexRecordOption::Basic))),
        (Occur::Must, Box::new(TermQuery::new(Term::from_field_text(engine.fields.namespace, "memory"), IndexRecordOption::Basic))),
        (Occur::Must, Box::new(TermQuery::new(Term::from_field_text(engine.fields.doc_id, doc_id), IndexRecordOption::Basic))),
        (Occur::Must, view.query(&engine.fields)),
    ])
}

fn retrieve(engine: &HybridSearchEngine, cached: &IndexReader, view: &snapshots::Lease,
    query: &dyn Query, mode: Mode, expected: &BTreeMap<String, (String, String)>) -> Result<Vec<(String, String)>> {
    let request_searcher = match mode {
        Mode::PerCallReaderLogicalRecovery | Mode::PerCallReaderPhysicalRecovery => engine.tantivy_index.reader()?.searcher(),
        _ => cached.searcher(),
    };
    let mut output = Vec::new();
    for (_, address) in request_searcher.search(query, &TopDocs::with_limit(8))? {
        let hit: tantivy::TantivyDocument = request_searcher.doc(address)?;
        let id = hit.get_first(engine.fields.doc_id).and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Missing ID"))?;
        let pk = hit.get_first(engine.fields.pk).and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Missing PK"))?;
        let recovery_searcher: Searcher = match mode {
            Mode::PerCallReaderLogicalRecovery | Mode::PerCallReaderPhysicalRecovery => engine.tantivy_index.reader()?.searcher(),
            Mode::SharedReaderLogicalRecovery => cached.searcher(),
            Mode::RequestSearcherPhysicalRecovery => request_searcher.clone(),
        };
        let recovery_query: Box<dyn Query> = match mode {
            Mode::RequestSearcherPhysicalRecovery | Mode::PerCallReaderPhysicalRecovery => Box::new(TermQuery::new(Term::from_field_text(engine.fields.pk, pk), IndexRecordOption::Basic)),
            _ => Box::new(logical_lookup(engine, view, id)),
        };
        let recovered = recovery_searcher.search(&recovery_query, &TopDocs::with_limit(2))?;
        ensure!(recovered.len() == 1, "Recovery was absent or ambiguous");
        let doc: tantivy::TantivyDocument = recovery_searcher.doc(recovered[0].1)?;
        let actual_pk = doc.get_first(engine.fields.pk).and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Missing recovered PK"))?;
        let text = doc.get_first(engine.fields.content).and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Missing recovered text"))?;
        let wanted = expected.get(id).ok_or_else(|| anyhow::anyhow!("Unexpected result identity"))?;
        ensure!(actual_pk == pk && actual_pk == wanted.0 && text == wanted.1,
            "Recovery selected a stale physical revision or changed source text");
        output.push((actual_pk.to_owned(), blake3::hash(text.as_bytes()).to_hex().to_string()));
    }
    ensure!(output.len() == 8);
    output.sort();
    Ok(output)
}

#[tokio::test]
async fn fragmented_source_reader_and_physical_recovery_cost_preserves_exact_content() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let engine = HybridSearchEngine::new(temporary.path().join("tantivy"), temporary.path().join("vectors"),
            &engram_core::Config { embedding_backend: "fts_only".into(), ..Default::default() }).await?;
        let cancel = CancellationToken::new();
        let initial: Vec<_> = (0..FILES).flat_map(|file| (0..CHUNKS).map(move |chunk| document(file, chunk, 1))).collect();
        engine.index_docs("project", &initial, &cancel).await?;
        let old = engine.pin_source_generation("project", 1)?;
        for file in 0..EDITS {
            let generation = file as u64 + 2;
            let path = RelPath::new(&format!("src/file{file:04}.rs"));
            engine.prepare_source_snapshot("project", generation - 1, generation, &[path], &[], &cancel).await?;
            let changed: Vec<_> = (0..CHUNKS).map(|chunk| document(file, chunk, generation)).collect();
            engine.index_docs("project", &changed, &cancel).await?;
            engine.seal_source_snapshot("project", generation)?;
        }
        let current = engine.snapshots.pin("project", "memory", EDITS as u64 + 1)?;
        let expected: BTreeMap<_, _> = (0..FILES).flat_map(|file| (0..CHUNKS).map(move |chunk|
            document(file, chunk, if file < EDITS { file as u64 + 2 } else { 1 })))
            .map(|doc| (doc.doc_id.clone(), (build_pk("project", "memory", doc.generation, &doc.doc_id), doc.content))).collect();
        let query = BooleanQuery::new(vec![
            (Occur::Must, Box::new(TermQuery::new(Term::from_field_text(engine.fields.content_words, "needle"), IndexRecordOption::Basic))),
            (Occur::Must, current.query(&engine.fields)),
        ]);
        let cached: IndexReader = engine.tantivy_index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        cached.reload()?;
        let searcher = cached.searcher();
        let complete = searcher.search(&query, &TopDocs::with_limit(FILES * CHUNKS + 1))?;
        let all_pks: BTreeSet<String> = complete.into_iter().map(|(_, address)| -> Result<_> {
            let doc: tantivy::TantivyDocument = searcher.doc(address)?;
            Ok(doc.get_first(engine.fields.pk).and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Missing PK"))?.to_owned())
        }).collect::<Result<_>>()?;
        ensure!(all_pks == expected.values().map(|(pk, _)| pk.clone()).collect(), "Complete selected revision set differs from independent oracle");
        let a = Mode::PerCallReaderLogicalRecovery;
        let b = Mode::SharedReaderLogicalRecovery;
        let c = Mode::RequestSearcherPhysicalRecovery;
        let d = Mode::PerCallReaderPhysicalRecovery;
        let reference = retrieve(&engine, &cached, &current, &query, a, &expected)?;
        for mode in [b, c, d] { ensure!(retrieve(&engine, &cached, &current, &query, mode, &expected)? == reference); }
        let mut observations = Vec::new();
        for (round, order) in [[a,b,c,d], [b,d,a,c], [c,a,d,b], [d,c,b,a]].into_iter().enumerate() {
            for mode in order {
                let before = cpu_ns()?;
                let started = Instant::now();
                for _ in 0..ITERATIONS {
                    ensure!(retrieve(&engine, &cached, &current, &query, mode, &expected)? == reference);
                }
                observations.push(serde_json::json!({"round":round,"mode":mode,"requests":ITERATIONS,"recoveries_per_request":8,
                    "seconds":started.elapsed().as_secs_f64(),"cpu_ns":before.zip(cpu_ns()?).map(|(x,y)|y-x)}));
            }
        }
        let old_query = old._snapshot.query(&engine.fields);
        ensure!(searcher.search(&old_query, &tantivy::collector::Count)? == FILES * CHUNKS,
            "New reader must retain the complete pinned old physical view");
        println!("SOURCE_READER_RECOVERY_EXPERIMENT {}", serde_json::json!({"files":FILES,"chunks_per_file":CHUNKS,"distinct_edits":EDITS,
            "segments":searcher.segment_readers().len(),"observations":observations,
            "limits":"One frozen owned segmented index, no new runtime cache. Complete selected physical-PK oracle and exact recovered text/hash equivalence. Component cost is not application CPU/RSS or kernel-pool attribution."}));
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[cfg(feature = "vector")]
struct DiverseProvider;
#[cfg(feature = "vector")]
#[async_trait::async_trait]
impl engram_ml::Embedder for DiverseProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut result = vec![0.0; 384];
        result[usize::from(text.contains("diverse"))] = 1.0;
        Ok(result)
    }
    fn dimension(&self) -> usize { 384 }
    fn model_tag(&self) -> String { "fixture-controlled-diversity-v1".into() }
}

#[tokio::test]
async fn selected_source_rank_prefix_preserves_full_candidate_diversity_and_later_pages() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let engine = HybridSearchEngine::new(temporary.path().join("tantivy"), temporary.path().join("vectors"),
            &engram_core::Config { embedding_backend: if cfg!(feature = "vector") { "local" } else { "fts_only" }.into(),
                ..Default::default() }).await?;
        #[cfg(feature = "vector")]
        let engine = {
            let mut engine = engine;
            engine.embedder = Arc::new(DiverseProvider);
            engine.query_embedder = Arc::new(DiverseProvider);
            engine
        };
        let cancel = CancellationToken::new();
        let docs: Vec<_> = (0..25).map(|i| {
            let content = if i == 24 { "needle diverse clan unit24".to_string() } else { format!("needle near clan unit{i:02}") };
            IndexDoc { generation: 1, chunk_id: i, path: RelPath::new(&format!("src/{i:02}.rs")), language: "rust".into(),
                content: content.clone(), namespace: "memory".into(), author: None, timestamp: None,
                start_line: 1, end_line: 1, doc_id: format!("item-{i:02}"), content_hash: ContentHash::compute(content.as_bytes()).0 }
        }).collect();
        engine.index_docs("project", &docs, &cancel).await?;
        let mut q = HybridQuery { project_id: "project".into(), namespace: "memory".into(), generation: 1,
            text: "needle".into(), top_k: 10, fts_mode: "loose".into(), include_path_prefixes: None,
            exclude_path_prefixes: None, include_path_suffixes: None, language_filters: None,
            author_filter: None, date_after: None, date_before: None, use_mmr: true };
        for use_mmr in [true, false] {
            q.use_mmr = use_mmr;
            let complete = engine.search(&q, None, &cancel).await?;
            ensure!(complete.len() == 10);
            #[cfg(feature = "vector")]
            if use_mmr {
                ensure!(complete[0].doc_id != "item-24" && complete[1].doc_id == "item-24",
                    "A second result must retain the diverse candidate outside the first ten");
            }
            for prefix in [0, 1, 2, 3, 7, 10, 20] {
                let observed = engine.search_ranked_prefix(&q, None, &cancel, prefix).await?;
                let expected = &complete[..prefix.min(complete.len())];
                ensure!(observed.len() == expected.len());
                for (a,b) in observed.iter().zip(expected) {
                    ensure!(a.pk == b.pk && a.path == b.path && a.doc_id == b.doc_id && a.score == b.score
                        && a.snippet == b.snippet && a.start_line == b.start_line && a.end_line == b.end_line,
                        "Requested prefix changed ranking, source identity or recovered content");
                }
            }
        }
        // Fewer candidates than q.top_k must retain the original non-MMR branch.
        q.top_k = 40;
        q.use_mmr = true;
        let complete = engine.search(&q, None, &cancel).await?;
        let prefix = engine.search_ranked_prefix(&q, None, &cancel, 3).await?;
        ensure!(prefix.iter().map(|h| &h.pk).collect::<Vec<_>>() == complete.iter().take(3).map(|h| &h.pk).collect::<Vec<_>>());
        q.include_path_prefixes = Some(vec!["absent/".into()]);
        ensure!(engine.search_ranked_prefix(&q, None, &cancel, 3).await?.is_empty());
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[cfg(feature = "vector")]
#[tokio::test]
async fn selected_source_mmr_preserves_maximum_diversity_stable_ties_and_missing_vectors() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let mut engine = HybridSearchEngine::new(temporary.path().join("tantivy"), temporary.path().join("vectors"),
            &engram_core::Config { embedding_backend: "fts_only".into(), ..Default::default() }).await?;
        let hit = |id: u64, name: &str, score: f32| HybridHit { pk: format!("project:memory:1:{name}"),
            chunk_id: id, path: RelPath::new(&format!("{name}.rs")), score, centrality: 0.0,
            snippet: None, doc_id: name.into(), start_line: 1, end_line: 1,
            snippet_truncated: false, timestamp: None };
        let items = vec![hit(1,"A",1.0),hit(2,"B",0.9),hit(7,"F",0.85),hit(3,"C",0.8),
            hit(5,"E",0.8),hit(4,"D",0.7),hit(6,"G",0.6)];
        for force_memory_pressure in [false, true] {
            engine.memory_budget = force_memory_pressure.then(|| Arc::new(MemoryBudget::new(1)));
            for (scaled, nan) in [(false,false),(true,false),(true,true)] {
                let mut vectors = std::collections::HashMap::from([
                    (1,vec![1.0,0.0]),(2,vec![0.0,1.0]),(3,vec![if scaled {2.0} else {1.0},0.0]),
                    (4,vec![0.8,0.6]),(5,vec![1.0,0.0]),(6,vec![-1.0,0.0]),
                ]);
                if nan { vectors.insert(7,vec![f32::NAN,0.0]); }
                // F is selected after maxima against A and B already exist.
                // Missing/NaN F must not erase them; D must outrank C/E even
                // though C/E are dissimilar to the most recently selected G.
                let expected = ["A","B","F","G","D","C","E"];
                for count in [0,1,2,3,4,5,7,9] {
                    let selected = engine.mmr_rerank(items.clone(), &vectors, count, 0.5);
                    ensure!(selected.iter().map(|h|h.doc_id.as_str()).collect::<Vec<_>>() == expected[..count.min(expected.len())],
                        "Diversification lost earlier similarities, clamping or stable tie order");
                    for value in &selected {
                        ensure!(items.iter().any(|original| original.pk == value.pk && original.score == value.score));
                    }
                }
                let relevance_only = engine.mmr_rerank(items.clone(), &vectors, items.len(), 1.0);
                ensure!(relevance_only.iter().map(|h|h.doc_id.as_str()).collect::<Vec<_>>() == ["A","B","F","C","E","D","G"]);
            }
        }
        Ok(())
    }.await;
    temporary.close()?;
    result
}
