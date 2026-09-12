//! External audit 2026-08-29 P0-3 (owner: keep looping) — release 27 live:
//! the first user call after a restart still took 9 s because the primes ran
//! one project after another in registry order and the call landed while the
//! big project's prime was in flight. The most recently updated project is
//! primed first; update time is a proxy, not a client activity measurement.

use engram_core::ProjectRecord;
use engram_server::actors::warmup::warm_order;

fn rec(id: &str, updated: u64) -> ProjectRecord {
    ProjectRecord {
        project_id: id.into(),
        project_name: id.into(),
        directory: format!("C:/x/{id}"),
        project_type: "general".into(),
        created_at_ms: 0,
        updated_at_ms: updated,
        reindex_required_since_ms: None,
    }
}

#[test]
fn the_most_recently_updated_project_is_primed_first() {
    let ordered = warm_order(vec![rec("old", 10), rec("newest", 300), rec("mid", 200)]);
    let ids: Vec<&str> = ordered.iter().map(|r| r.project_id.as_str()).collect();
    assert_eq!(ids, vec!["newest", "mid", "old"]);
}

#[test]
fn equal_update_times_select_the_same_projects_regardless_of_registry_order() {
    let first = warm_order(vec![rec("beta", 100), rec("alpha", 100), rec("old", 1)]);
    let reversed = warm_order(vec![rec("old", 1), rec("alpha", 100), rec("beta", 100)]);
    let ids = |records: Vec<ProjectRecord>| {
        records.into_iter().map(|r| r.project_id).collect::<Vec<_>>()
    };
    assert_eq!(ids(first), ids(reversed));
}
