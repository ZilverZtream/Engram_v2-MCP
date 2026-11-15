use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

fn cards() -> Vec<Arc<ChangeCard>> {
    ["photo upload", "policy validation", "image display"]
        .into_iter()
        .enumerate()
        .map(|(i, title)| {
            Arc::new(ChangeCard {
                oid: format!("{i:040x}"),
                title: title.into(),
                description: String::new(),
                author: "Fixture".into(),
                timestamp: 1,
                files: vec!["Feature.js".into()],
                file_count: 1,
                vector: Vec::new(),
            })
        })
        .collect()
}

#[tokio::test]
async fn disabled_zero_and_invalid_budget_do_not_start_semantic_work() {
    let started = AtomicBool::new(false);
    for (enabled, budget, expected) in [
        (false, 5000, SemanticOutcome::VectorsDisabled),
        (true, 0, SemanticOutcome::ZeroBudget),
    ] {
        let baseline = cards();
        let result = rank_with_semantic_budget(baseline.clone(), enabled, budget, || {
            started.store(true, Ordering::SeqCst);
            async { Ok(Vec::new()) }
        })
        .await
        .unwrap();
        assert_eq!(result.outcome, expected);
        assert_eq!(result.ranked.len(), baseline.len());
        assert!(
            result
                .ranked
                .iter()
                .zip(&baseline)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        );
    }
    assert!(
        rank_with_semantic_budget(cards(), true, 90_001, || {
            started.store(true, Ordering::SeqCst);
            async { Ok(Vec::new()) }
        })
        .await
        .is_err()
    );
    assert!(!started.load(Ordering::SeqCst));
    assert_eq!(checked_semantic_timeout_ms(None).unwrap(), 5000);
}

#[tokio::test(start_paused = true)]
async fn document_and_query_embeddings_share_one_deadline_and_preserve_baseline() {
    struct Pending<'a>(&'a AtomicBool);
    impl Drop for Pending<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = AtomicBool::new(false);
    let phases = AtomicUsize::new(0);
    let baseline = cards();
    let start = tokio::time::Instant::now();
    let result = rank_with_semantic_budget(baseline.clone(), true, 100, || async {
        let _pending = Pending(&dropped);
        tokio::time::sleep(Duration::from_millis(60)).await;
        phases.store(1, Ordering::SeqCst); // document embeddings complete
        tokio::time::sleep(Duration::from_millis(60)).await;
        phases.store(2, Ordering::SeqCst); // query embeddings would finish after120ms
        Ok(Vec::new())
    })
    .await
    .unwrap();
    assert_eq!(result.outcome, SemanticOutcome::TimedOut { budget_ms: 100 });
    assert!(start.elapsed() >= Duration::from_millis(100));
    assert!(start.elapsed() < Duration::from_millis(120));
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(result.ranked.len(), baseline.len());
    assert!(
        result
            .ranked
            .iter()
            .zip(&baseline)
            .all(|(a, b)| Arc::ptr_eq(a, b))
    );
    tokio::time::advance(Duration::from_millis(200)).await;
    assert_eq!(phases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_failure_preserves_all_lexical_matches_and_success_can_find_nonlexical_matches() {
    let baseline = cards();
    let result = rank_with_semantic_budget(baseline.clone(), true, 1000, || async {
        anyhow::bail!("owned provider unavailable")
    })
    .await
    .unwrap();
    assert!(matches!(result.outcome, SemanticOutcome::Failed { .. }));
    assert_eq!(result.ranked.len(), baseline.len());
    let expected = baseline[2].clone();
    // A lexical miss must not suppress an otherwise available semantic match.
    let result = rank_with_semantic_budget(Vec::new(), true, 1000, || async {
        Ok(vec![expected.clone()])
    })
    .await
    .unwrap();
    assert_eq!(result.outcome, SemanticOutcome::Completed);
    assert!(Arc::ptr_eq(&result.ranked[0], &expected));
}
