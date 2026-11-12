#![allow(clippy::unwrap_used)]
use engram_core::{JobRecord, Registry};
use engram_server::services::job_progress::{Reporter, describe};
use std::sync::Arc;

fn job() -> JobRecord {
    JobRecord {
        job_id: "progress-test".into(),
        kind: "index_project".into(),
        project_id: None,
        status: "running".into(),
        message: "indexing".into(),
        progress_pct: 0,
        estimated_time_remaining_ms: None,
        created_at_ms: 1000,
        updated_at_ms: 1000,
    }
}

#[test]
fn phase_counts_and_eta_survive_reopen_and_do_not_claim_whole_job_completion() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("registry.redb");
    {
        let registry = Arc::new(Registry::open(&path).unwrap());
        registry.put_job(&job()).unwrap();
        let reporter = Reporter::new(registry.clone(), "progress-test");
        reporter.observe_at("source_indexing", Some(0), Some(10), Some("files"), 1000);
        reporter.observe_at("source_indexing", Some(2), Some(10), Some("files"), 3000);
        let (job, observation) = registry
            .get_job_observation("progress-test")
            .unwrap()
            .unwrap();
        let observation = observation.unwrap();
        assert_eq!(observation.completed, Some(2));
        assert_eq!(observation.percent, Some(20));
        assert_eq!(observation.estimated_time_remaining_ms, Some(8000));
        assert!(
            !describe(&job, Some(&observation))["job_complete"]
                .as_bool()
                .unwrap()
        );
        reporter.observe_at("source_indexing", Some(10), Some(10), Some("files"), 3500);
        let (job, phase) = registry
            .get_job_observation("progress-test")
            .unwrap()
            .unwrap();
        assert_eq!(phase.as_ref().unwrap().percent, Some(100));
        assert_eq!(job.progress_pct, 99);
        assert_eq!(describe(&job, phase.as_ref())["job_complete"], false);
        reporter.observe_at("graph_processing", None, None, None, 3501);
    }
    {
        let registry = Registry::open(&path).unwrap();
        let (job, phase) = registry
            .get_job_observation("progress-test")
            .unwrap()
            .unwrap();
        let phase = phase.unwrap();
        assert_eq!(phase.phase, "graph_processing");
        assert_eq!(phase.completed, None);
        assert_eq!(phase.estimated_time_remaining_ms, None);
        assert_eq!(job.status, "running");
        assert_eq!(registry.cleanup_orphaned_jobs().unwrap(), 1);
        let (job, phase) = registry
            .get_job_observation("progress-test")
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "aborted");
        assert_eq!(phase.unwrap().phase, "graph_processing");
    }
    temp.close().unwrap();
}

#[test]
fn terminal_and_deleted_jobs_cannot_be_revived_by_late_progress() {
    let temp = tempfile::tempdir().unwrap();
    {
        let registry = Arc::new(Registry::open(&temp.path().join("registry.redb")).unwrap());
        let mut record = job();
        registry.put_job(&record).unwrap();
        let reporter = Reporter::new(registry.clone(), &record.job_id);
        reporter.observe_at("source_indexing", Some(0), Some(20), Some("files"), 1000);
        reporter.observe_at("source_indexing", Some(5), Some(20), Some("files"), 2000);
        record.status = "cancelled".into();
        record.updated_at_ms = 2001;
        registry.put_job(&record).unwrap();
        reporter.observe_at("source_indexing", Some(20), Some(20), Some("files"), 4000);
        let (record, phase) = registry
            .get_job_observation(&record.job_id)
            .unwrap()
            .unwrap();
        let output = describe(&record, phase.as_ref());
        assert_eq!(record.status, "cancelled");
        assert_eq!(output["completed"], 5);
        assert_eq!(output["job_complete"], false);
        assert_eq!(
            output["estimated_time_remaining_ms"],
            serde_json::Value::Null
        );
        registry.delete_job(&record.job_id).unwrap();
        reporter.observe_at("source_indexing", Some(20), Some(20), Some("files"), 5000);
        assert!(
            registry
                .get_job_observation(&record.job_id)
                .unwrap()
                .is_none()
        );
    }
    temp.close().unwrap();
}

#[test]
fn throttled_counts_unknown_totals_and_clock_changes_do_not_invent_eta() {
    let temp = tempfile::tempdir().unwrap();
    {
        let registry = Arc::new(Registry::open(&temp.path().join("registry.redb")).unwrap());
        registry.put_job(&job()).unwrap();
        let reporter = Reporter::new(registry.clone(), "progress-test");
        reporter.observe_at("scan", None, None, None, 1000);
        reporter.observe_at("files", Some(0), Some(100), Some("files"), 2000);
        reporter.observe_at("files", Some(1), Some(100), Some("files"), 2100);
        let observation = || {
            registry
                .get_job_observation("progress-test")
                .unwrap()
                .unwrap()
                .1
                .unwrap()
        };
        assert_eq!(observation().completed, Some(0));
        reporter.observe_at("files", Some(10), Some(100), Some("files"), 3000);
        assert_eq!(observation().estimated_time_remaining_ms, Some(9000));
        reporter.observe_at("files", Some(11), Some(100), Some("files"), 1500);
        assert_eq!(observation().estimated_time_remaining_ms, None);
        reporter.observe_at("files", Some(12), Some(100), Some("files"), 2500);
        assert_eq!(observation().estimated_time_remaining_ms, Some(88000));
        reporter.observe_at("empty", Some(0), Some(0), Some("files"), 3500);
        assert_eq!(observation().percent, None);
        assert_eq!(observation().estimated_time_remaining_ms, None);
        let legacy = job();
        let output = describe(&legacy, None);
        assert_eq!(output["coverage"], "phase_unavailable_for_this_job");
        assert_eq!(output["phase_progress_pct"], serde_json::Value::Null);
    }
    temp.close().unwrap();
}
