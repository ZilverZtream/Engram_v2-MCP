use super::*;
use std::collections::HashMap;
use tokio::sync::Semaphore;

fn metrics(score: f32) -> CentralityMetrics {
    CentralityMetrics {
        pagerank: HashMap::from([("source-node".into(), score)]),
    }
}

#[test]
fn pagerank_retention_failure_does_not_spin_and_new_evidence_can_refresh() {
    let cache = Arc::new(PageRankCache::default());
    let workers = Arc::new(Semaphore::new(2));
    drop(
        cache
            .start(1, workers.clone().try_acquire_owned().unwrap())
            .unwrap(),
    );
    assert!(
        cache
            .start(1, workers.clone().try_acquire_owned().unwrap())
            .is_none(),
        "Repeated requests must not immediately repeat a failed expensive computation"
    );
    assert_eq!(workers.available_permits(), 2);
    cache.state.lock().unwrap().retry_after = Some(Instant::now() - Duration::from_secs(1));
    drop(
        cache
            .start(1, workers.clone().try_acquire_owned().unwrap())
            .unwrap(),
    );
    cache
        .start(2, workers.clone().try_acquire_owned().unwrap())
        .unwrap()
        .complete(metrics(2.0));
    assert_eq!(
        cache.get(2).unwrap().pagerank["source-node"],
        2.0,
        "New source evidence must not inherit an older generation's failure cooldown"
    );
}

#[test]
fn pagerank_retention_releases_superseded_maps_but_preserves_a_request_held_result() {
    let cache = Arc::new(PageRankCache::default());
    let workers = Arc::new(Semaphore::new(2));
    cache
        .start(1, workers.clone().try_acquire_owned().unwrap())
        .unwrap()
        .complete(metrics(1.0));
    let old_request = cache.get(1).unwrap();
    let old_map = Arc::downgrade(&old_request);
    for generation in 2..=300 {
        assert!(cache.get(generation).is_none());
        cache
            .start(generation, workers.clone().try_acquire_owned().unwrap())
            .unwrap()
            .complete(metrics(generation as f32));
        let current = cache.get(generation).unwrap();
        assert_eq!(current.pagerank["source-node"], generation as f32);
        let released = Arc::downgrade(&current);
        drop(current);
        assert!(cache.get(generation + 1).is_none());
        assert!(
            released.upgrade().is_none(),
            "Obsolete generation data must be released without revisiting its key"
        );
    }
    assert_eq!(old_request.pagerank["source-node"], 1.0);
    drop(old_request);
    assert!(old_map.upgrade().is_none());
    assert_eq!(workers.available_permits(), 2);
}

#[test]
fn pagerank_retention_late_completion_expiry_and_runtime_isolation_preserve_exact_generation() {
    let cache = Arc::new(PageRankCache::default());
    let workers = Arc::new(Semaphore::new(2));
    let old = cache
        .start(1, workers.clone().try_acquire_owned().unwrap())
        .unwrap();
    assert!(cache.get(2).is_none());
    assert!(
        cache
            .start(2, workers.clone().try_acquire_owned().unwrap())
            .is_none(),
        "One worker per runtime"
    );
    old.complete(metrics(1.0));
    assert!(cache.get(1).is_none());
    assert!(
        cache.get(2).is_none(),
        "A late old result cannot boost the new generation"
    );
    cache
        .start(2, workers.clone().try_acquire_owned().unwrap())
        .unwrap()
        .complete(metrics(2.0));
    let held = cache.get(2).unwrap();
    let another_runtime = Arc::new(PageRankCache::default());
    assert!(another_runtime.get(2).is_none());
    another_runtime
        .start(2, workers.clone().try_acquire_owned().unwrap())
        .unwrap()
        .complete(metrics(20.0));
    assert_eq!(
        another_runtime.get(2).unwrap().pagerank["source-node"],
        20.0
    );
    cache.state.lock().unwrap().value.as_mut().unwrap().0 = Instant::now() - TTL;
    assert!(
        cache.get(2).is_none(),
        "Expired data cannot boost a fresh request"
    );
    assert_eq!(
        held.pagerank["source-node"], 2.0,
        "Expiry cannot invalidate a response already using its Arc"
    );
    cache
        .start(2, workers.clone().try_acquire_owned().unwrap())
        .unwrap()
        .complete(metrics(3.0));
    assert_eq!(cache.get(2).unwrap().pagerank["source-node"], 3.0);
}

#[tokio::test]
async fn pagerank_retention_worker_admission_survives_caller_cancellation() -> anyhow::Result<()> {
    let workers = Arc::new(Semaphore::new(2));
    let first = Arc::new(PageRankCache::default());
    let second = Arc::new(PageRankCache::default());
    let work = first
        .start(1, workers.clone().try_acquire_owned()?)
        .unwrap();
    let other_work = second
        .start(1, workers.clone().try_acquire_owned()?)
        .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let notice = started.clone();
    let (release, wait) = std::sync::mpsc::channel();
    let child = tokio::task::spawn_blocking(move || {
        notice.notify_one();
        wait.recv_timeout(Duration::from_secs(5))?;
        work.complete(metrics(1.0));
        Ok::<_, anyhow::Error>(())
    });
    tokio::time::timeout(Duration::from_secs(5), started.notified()).await?;
    child.abort(); // A running blocking worker retains its actual admission.
    assert_eq!(workers.available_permits(), 0);
    assert!(workers.clone().try_acquire_owned().is_err());
    assert!(first.get(1).is_none());
    release.send(())?;
    tokio::time::timeout(Duration::from_secs(5), child).await???;
    assert_eq!(workers.available_permits(), 1);
    assert_eq!(first.get(1).unwrap().pagerank["source-node"], 1.0);
    drop(other_work);
    assert_eq!(workers.available_permits(), 2);
    Ok(())
}
