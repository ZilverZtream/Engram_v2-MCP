#![allow(clippy::unwrap_used)]
//! Behavioral tests for the production Registry (Subsystem 3 / Subsystem 8).
//!
//! Covers all previously untested Registry methods:
//!  - `put_project` / `get_project` / `list_projects` / `delete_project`
//!  - `delete_all_for_project` (cross-table cascade)
//!  - `put_memory_section` / `get_memory_section` / `list_memory_sections` / `delete_memory_section`
//!  - `put_repo_rule` / `list_repo_rules` / `delete_repo_rule`
//!  - `put_watch` / `list_watches`
//!  - `put_job` / `get_job` / `list_jobs` / `delete_job`
//!  - `cleanup_orphaned_jobs`

use engram_core::{JobRecord, MemorySection, ProjectRecord, Registry, RepoRule, WatchRecord};

fn open_registry(tmp: &tempfile::TempDir) -> Registry {
    Registry::open(&tmp.path().join("reg.redb")).expect("Registry::open must succeed")
}

fn make_project(id: &str, name: &str) -> ProjectRecord {
    ProjectRecord {
        project_id: id.to_string(),
        project_name: name.to_string(),
        project_type: "csharp".to_string(),
        directory: "/code/project".to_string(),
        created_at_ms: 1_000_000,
        updated_at_ms: 1_000_000,
        reindex_required_since_ms: None,
    }
}

fn make_memory_section(id: &str, title: &str) -> MemorySection {
    MemorySection {
        section_id: id.to_string(),
        title: title.to_string(),
        content: "section content here".to_string(),
        updated_at_ms: 1_000_000,
        created_at_ms: 1_000_000,
        author: None,
        kind: None,
        review_after_ms: None,
        tags: Vec::new(),
        related_files: Vec::new(),
    }
}

fn make_repo_rule(id: &str, pattern: &str) -> RepoRule {
    RepoRule {
        rule_id: id.to_string(),
        file_pattern: pattern.to_string(),
        rule_text: "Do not use global state".to_string(),
        priority: 1,
        updated_at_ms: 1_000_000,
    }
}

fn make_watch(id: &str, dir: &str) -> WatchRecord {
    WatchRecord {
        watch_id: id.to_string(),
        directory: dir.to_string(),
        enabled: true,
        updated_at_ms: 1_000_000,
    }
}

fn make_job(id: &str, project_id: Option<&str>, status: &str) -> JobRecord {
    JobRecord {
        job_id: id.to_string(),
        kind: "indexing".to_string(),
        project_id: project_id.map(str::to_string),
        status: status.to_string(),
        message: "".to_string(),
        progress_pct: 0,
        estimated_time_remaining_ms: None,
        created_at_ms: 1_000_000,
        updated_at_ms: 1_000_000,
    }
}

// ── ProjectRecord CRUD ────────────────────────────────────────────────────────

/// put_project followed by get_project must return the same record.
#[test]
fn registry_put_and_get_project_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let proj = make_project("proj-001", "My Project");
    reg.put_project(&proj).expect("put_project must succeed");

    let retrieved = reg
        .get_project("proj-001")
        .expect("get_project must not error");
    assert!(
        retrieved.is_some(),
        "get_project must return the record after put"
    );

    let r = retrieved.unwrap();
    assert_eq!(r.project_id, "proj-001");
    assert_eq!(r.project_name, "My Project");
    assert_eq!(r.project_type, "csharp");
}

/// get_project for an unknown project_id must return None, not Err.
#[test]
fn registry_get_project_unknown_returns_none() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let result = reg
        .get_project("no-such-project")
        .expect("get_project must not error");
    assert!(
        result.is_none(),
        "get_project for unknown id must return None"
    );
}

/// list_projects must return all inserted projects sorted by project_name.
#[test]
fn registry_list_projects_sorted_by_name() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_project(&make_project("p1", "Zebra"))
        .expect("put p1");
    reg.put_project(&make_project("p2", "Alpha"))
        .expect("put p2");
    reg.put_project(&make_project("p3", "Mango"))
        .expect("put p3");

    let projects = reg.list_projects().expect("list_projects must not error");
    assert_eq!(projects.len(), 3, "must return 3 projects");
    assert_eq!(
        projects[0].project_name, "Alpha",
        "must be sorted: Alpha first"
    );
    assert_eq!(
        projects[1].project_name, "Mango",
        "must be sorted: Mango second"
    );
    assert_eq!(
        projects[2].project_name, "Zebra",
        "must be sorted: Zebra third"
    );
}

/// delete_project must remove the record; subsequent get_project returns None.
#[test]
fn registry_delete_project_makes_get_return_none() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_project(&make_project("p-del", "ToDelete"))
        .expect("put");
    assert!(
        reg.get_project("p-del").expect("get").is_some(),
        "must exist before delete"
    );

    reg.delete_project("p-del")
        .expect("delete_project must succeed");

    let after = reg.get_project("p-del").expect("get after delete");
    assert!(after.is_none(), "project must be gone after delete");
}

// ── delete_all_for_project — cascade delete ───────────────────────────────────

/// delete_all_for_project must remove the project AND all associated
/// memory sections, repo rules, watches, and meta — but not other projects.
#[test]
fn registry_delete_all_for_project_cascades_across_tables() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let proj_a = "proj-cascade-a";
    let proj_b = "proj-cascade-b";

    reg.put_project(&make_project(proj_a, "A")).expect("put A");
    reg.put_project(&make_project(proj_b, "B")).expect("put B");

    reg.put_memory_section(proj_a, &make_memory_section("sec-1", "Section 1"))
        .expect("put memory section");
    reg.put_repo_rule(proj_a, &make_repo_rule("rule-1", "*.cs"))
        .expect("put repo rule");
    reg.set_meta(proj_a, "active_generation", "5")
        .expect("set meta");

    reg.delete_all_for_project(proj_a)
        .expect("delete_all_for_project must succeed");

    // proj_a must be gone
    assert!(
        reg.get_project(proj_a).expect("get").is_none(),
        "project A must be removed after delete_all"
    );
    // proj_a memory sections must be gone
    let sections = reg.list_memory_sections(proj_a).expect("list");
    assert!(
        sections.is_empty(),
        "memory sections for A must be gone after delete_all"
    );
    // proj_a repo rules must be gone
    let rules = reg.list_repo_rules(proj_a).expect("list");
    assert!(
        rules.is_empty(),
        "repo rules for A must be gone after delete_all"
    );
    // proj_a meta must be gone
    let meta = reg.get_meta(proj_a, "active_generation").expect("get meta");
    assert!(meta.is_none(), "meta for A must be gone after delete_all");

    // proj_b must NOT be affected
    assert!(
        reg.get_project(proj_b).expect("get").is_some(),
        "project B must survive delete_all_for_project(A)"
    );
}

// ── MemorySection CRUD ────────────────────────────────────────────────────────

/// put_memory_section followed by get_memory_section must round-trip correctly.
#[test]
fn registry_put_and_get_memory_section_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let sec = make_memory_section("sec-001", "Architecture Notes");
    reg.put_memory_section("proj-mem", &sec)
        .expect("put_memory_section must succeed");

    let retrieved = reg
        .get_memory_section("proj-mem", "sec-001")
        .expect("get_memory_section must not error");
    assert!(retrieved.is_some(), "must return section after put");

    let r = retrieved.unwrap();
    assert_eq!(r.section_id, "sec-001");
    assert_eq!(r.title, "Architecture Notes");
    assert_eq!(r.content, "section content here");
}

/// get_memory_section for unknown section must return None.
#[test]
fn registry_get_memory_section_unknown_returns_none() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let result = reg
        .get_memory_section("proj-mem", "no-such-section")
        .expect("must not error");
    assert!(result.is_none(), "unknown memory section must return None");
}

/// list_memory_sections must return only sections for the specified project.
#[test]
fn registry_list_memory_sections_project_isolation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_memory_section("proj-A", &make_memory_section("s1", "Zebra"))
        .expect("put A/s1");
    reg.put_memory_section("proj-A", &make_memory_section("s2", "Alpha"))
        .expect("put A/s2");
    reg.put_memory_section("proj-B", &make_memory_section("s3", "Other"))
        .expect("put B/s3");

    let a_sections = reg.list_memory_sections("proj-A").expect("list A");
    assert_eq!(a_sections.len(), 2, "proj-A must have 2 memory sections");

    let b_sections = reg.list_memory_sections("proj-B").expect("list B");
    assert_eq!(b_sections.len(), 1, "proj-B must have 1 memory section");
}

/// delete_memory_section must remove the section; subsequent get returns None.
#[test]
fn registry_delete_memory_section_removes_record() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let sec = make_memory_section("sec-del", "To Remove");
    reg.put_memory_section("proj-del", &sec).expect("put");
    assert!(
        reg.get_memory_section("proj-del", "sec-del")
            .expect("get")
            .is_some(),
        "must exist before delete"
    );

    reg.delete_memory_section("proj-del", "sec-del")
        .expect("delete_memory_section must succeed");

    let after = reg
        .get_memory_section("proj-del", "sec-del")
        .expect("get after delete");
    assert!(after.is_none(), "section must be gone after delete");
}

// ── RepoRule CRUD ─────────────────────────────────────────────────────────────

/// put_repo_rule followed by list_repo_rules must return the inserted rule.
#[test]
fn registry_put_repo_rule_and_list_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_repo_rule("proj-rules", &make_repo_rule("rule-001", "*.cs"))
        .expect("put_repo_rule must succeed");
    reg.put_repo_rule("proj-rules", &make_repo_rule("rule-002", "*.aspx"))
        .expect("put second rule");

    let rules = reg
        .list_repo_rules("proj-rules")
        .expect("list_repo_rules must not error");
    assert_eq!(rules.len(), 2, "must return 2 repo rules");

    let patterns: Vec<&str> = rules.iter().map(|r| r.file_pattern.as_str()).collect();
    assert!(patterns.contains(&"*.cs"), "must contain *.cs rule");
    assert!(patterns.contains(&"*.aspx"), "must contain *.aspx rule");
}

/// delete_repo_rule must remove the specific rule without affecting others.
#[test]
fn registry_delete_repo_rule_removes_only_that_rule() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_repo_rule("proj-r", &make_repo_rule("keep-rule", "*.rs"))
        .expect("put keep");
    reg.put_repo_rule("proj-r", &make_repo_rule("del-rule", "*.tmp"))
        .expect("put del");

    reg.delete_repo_rule("proj-r", "del-rule")
        .expect("delete_repo_rule must succeed");

    let rules = reg.list_repo_rules("proj-r").expect("list");
    assert_eq!(rules.len(), 1, "must have exactly 1 rule after delete");
    assert_eq!(
        rules[0].rule_id, "keep-rule",
        "remaining rule must be 'keep-rule'"
    );
}

// ── WatchRecord ───────────────────────────────────────────────────────────────

/// put_watch followed by list_watches must return the inserted record.
#[test]
fn registry_put_watch_and_list_watches_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_watch("proj-w", &make_watch("watch-001", "/code/src"))
        .expect("put watch");
    reg.put_watch("proj-w", &make_watch("watch-002", "/code/tests"))
        .expect("put watch 2");

    let watches = reg
        .list_watches("proj-w")
        .expect("list_watches must not error");
    assert_eq!(watches.len(), 2, "must return 2 watches");

    let dirs: Vec<&str> = watches.iter().map(|w| w.directory.as_str()).collect();
    assert!(dirs.contains(&"/code/src"), "must contain /code/src watch");
    assert!(
        dirs.contains(&"/code/tests"),
        "must contain /code/tests watch"
    );
}

/// list_watches for a project with no watches must return empty vec.
#[test]
fn registry_list_watches_empty_project_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let watches = reg
        .list_watches("proj-no-watches")
        .expect("list_watches must not error");
    assert!(watches.is_empty(), "no watches → empty list");
}

// ── JobRecord CRUD ────────────────────────────────────────────────────────────

/// put_job followed by get_job must round-trip correctly.
#[test]
fn registry_put_and_get_job_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let job = make_job("job-001", Some("proj-abc"), "running");
    reg.put_job(&job).expect("put_job must succeed");

    let retrieved = reg.get_job("job-001").expect("get_job must not error");
    assert!(
        retrieved.is_some(),
        "get_job must return the record after put"
    );

    let r = retrieved.unwrap();
    assert_eq!(r.job_id, "job-001");
    assert_eq!(r.status, "running");
    assert_eq!(r.project_id.as_deref(), Some("proj-abc"));
}

/// get_job for unknown job_id must return None, not Err.
#[test]
fn registry_get_job_unknown_returns_none() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let result = reg.get_job("no-such-job").expect("get_job must not error");
    assert!(result.is_none(), "unknown job_id must return None");
}

/// list_jobs with no filter must return all inserted jobs.
#[test]
fn registry_list_jobs_no_filter_returns_all() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_job(&make_job("j1", Some("proj-1"), "completed"))
        .expect("put j1");
    reg.put_job(&make_job("j2", Some("proj-2"), "running"))
        .expect("put j2");
    reg.put_job(&make_job("j3", None, "failed"))
        .expect("put j3");

    let all = reg.list_jobs(None).expect("list_jobs must not error");
    assert_eq!(all.len(), 3, "list_jobs(None) must return all 3 jobs");
}

/// list_jobs with a project_id filter must return only that project's jobs.
#[test]
fn registry_list_jobs_filtered_by_project_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_job(&make_job("j-a1", Some("proj-A"), "running"))
        .expect("put j-a1");
    reg.put_job(&make_job("j-a2", Some("proj-A"), "completed"))
        .expect("put j-a2");
    reg.put_job(&make_job("j-b1", Some("proj-B"), "running"))
        .expect("put j-b1");

    let a_jobs = reg.list_jobs(Some("proj-A")).expect("list filtered");
    assert_eq!(
        a_jobs.len(),
        2,
        "proj-A must have 2 jobs; got {}",
        a_jobs.len()
    );

    let b_jobs = reg.list_jobs(Some("proj-B")).expect("list filtered");
    assert_eq!(b_jobs.len(), 1, "proj-B must have 1 job");
}

/// delete_job must remove the job; subsequent get_job returns None.
#[test]
fn registry_delete_job_removes_record() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_job(&make_job("job-del", Some("proj"), "running"))
        .expect("put");
    assert!(
        reg.get_job("job-del").expect("get").is_some(),
        "job must exist before delete"
    );

    reg.delete_job("job-del").expect("delete_job must succeed");

    let after = reg.get_job("job-del").expect("get after delete");
    assert!(after.is_none(), "job must be gone after delete");
}

// ── cleanup_orphaned_jobs ─────────────────────────────────────────────────────

/// cleanup_orphaned_jobs must transition all "running" jobs to "aborted"
/// and return the count of updated jobs.
#[test]
fn registry_cleanup_orphaned_jobs_aborts_running_jobs() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_job(&make_job("j-run-1", None, "running"))
        .expect("put running 1");
    reg.put_job(&make_job("j-run-2", None, "running"))
        .expect("put running 2");
    reg.put_job(&make_job("j-done", None, "completed"))
        .expect("put completed");

    let count = reg
        .cleanup_orphaned_jobs()
        .expect("cleanup_orphaned_jobs must succeed");
    assert_eq!(count, 2, "must abort exactly 2 running jobs; got {count}");

    // running jobs must now be "aborted"
    let j1 = reg.get_job("j-run-1").expect("get").unwrap();
    let j2 = reg.get_job("j-run-2").expect("get").unwrap();
    assert_eq!(j1.status, "aborted", "j-run-1 must be 'aborted'");
    assert_eq!(j2.status, "aborted", "j-run-2 must be 'aborted'");

    // completed job must be unchanged
    let jd = reg.get_job("j-done").expect("get").unwrap();
    assert_eq!(
        jd.status, "completed",
        "completed job must not be changed by cleanup"
    );
}

/// cleanup_orphaned_jobs with no running jobs must return 0 and not error.
#[test]
fn registry_cleanup_orphaned_jobs_zero_running_returns_zero() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_job(&make_job("j1", None, "completed"))
        .expect("put");
    reg.put_job(&make_job("j2", None, "failed")).expect("put");

    let count = reg.cleanup_orphaned_jobs().expect("cleanup must not error");
    assert_eq!(count, 0, "no running jobs → cleanup count must be 0");
}

// ── project isolation ─────────────────────────────────────────────────────────

/// Memory sections from different projects must not be visible to each other.
#[test]
fn registry_memory_section_project_isolation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_memory_section("proj-X", &make_memory_section("sec-x", "X's section"))
        .expect("put X");
    reg.put_memory_section("proj-Y", &make_memory_section("sec-y", "Y's section"))
        .expect("put Y");

    // proj-X must not see proj-Y's section
    let cross = reg
        .get_memory_section("proj-X", "sec-y")
        .expect("cross-project get");
    assert!(
        cross.is_none(),
        "proj-X must not see proj-Y's section 'sec-y' — project isolation failure"
    );
}

// ── S09-001: composite key delimiter adversarial inputs ───────────────────────

/// ENG-AUD-2026-S09-001: put_memory_section must reject a section_id that
/// contains the NUL byte (\0), which is the composite key delimiter.
///
/// If \0 is allowed, a section_id like "sec\0\proj-B" could encode a
/// second project_id component and read/write across project boundaries.
#[test]
fn put_memory_section_rejects_nul_in_section_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let mut sec = make_memory_section("legitimate-section", "Notes");
    sec.section_id = "sec\0injection".to_string(); // NUL byte injection

    let result = reg.put_memory_section("proj-adv", &sec);
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: put_memory_section must reject section_id containing \\0; \
         got Ok (security: NUL byte allows composite key manipulation)"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("ENG-AUD-2026-S09-001"),
        "error must cite ENG-AUD-2026-S09-001; got: {msg}"
    );
}

/// ENG-AUD-2026-S09-001: put_memory_section must reject an empty section_id.
///
/// An empty section_id would produce a key of "project_id\0" without a
/// section component, making it indistinguishable from other empty-id writes
/// and potentially overwriting unrelated records.
#[test]
fn put_memory_section_rejects_empty_section_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let mut sec = make_memory_section("", "Notes"); // empty section_id
    sec.section_id = String::new();

    let result = reg.put_memory_section("proj-adv-empty", &sec);
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: put_memory_section must reject empty section_id"
    );
}

/// ENG-AUD-2026-S09-001: delete_memory_section must reject a section_id
/// containing the NUL delimiter byte.
#[test]
fn delete_memory_section_rejects_nul_in_section_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    // First create a valid section so delete has something to act on.
    reg.put_memory_section("proj-del-adv", &make_memory_section("real-sec", "Title"))
        .expect("pre-condition: put must succeed for a valid section_id");

    // Now attempt to delete with an adversarial section_id.
    let result = reg.delete_memory_section("proj-del-adv", "real\0sec");
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: delete_memory_section must reject section_id with \\0"
    );
}

/// ENG-AUD-2026-S09-001: put_repo_rule must reject a rule_id containing \0.
#[test]
fn put_repo_rule_rejects_nul_in_rule_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let mut rule = make_repo_rule("rule\0evil", "*.cs");
    rule.rule_id = "rule\0evil".to_string();

    let result = reg.put_repo_rule("proj-rule-adv", &rule);
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: put_repo_rule must reject rule_id containing \\0"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("ENG-AUD-2026-S09-001"),
        "error must cite audit tag; got: {msg}"
    );
}

/// ENG-AUD-2026-S09-001: put_repo_rule must reject an empty rule_id.
#[test]
fn put_repo_rule_rejects_empty_rule_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let mut rule = make_repo_rule("", "*.cs");
    rule.rule_id = String::new();

    let result = reg.put_repo_rule("proj-rule-empty", &rule);
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: put_repo_rule must reject empty rule_id"
    );
}

/// ENG-AUD-2026-S09-001: delete_repo_rule must reject a rule_id with \0.
#[test]
fn delete_repo_rule_rejects_nul_in_rule_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    reg.put_repo_rule("proj-drule-adv", &make_repo_rule("real-rule", "*.rs"))
        .expect("pre-condition");

    let result = reg.delete_repo_rule("proj-drule-adv", "real\0rule");
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: delete_repo_rule must reject rule_id with \\0"
    );
}

/// ENG-AUD-2026-S09-001: put_watch must reject a watch_id containing \0.
#[test]
fn put_watch_rejects_nul_in_watch_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let mut watch = make_watch("watch\0evil", "/code/src");
    watch.watch_id = "watch\0evil".to_string();

    let result = reg.put_watch("proj-watch-adv", &watch);
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: put_watch must reject watch_id containing \\0"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("ENG-AUD-2026-S09-001"),
        "error must cite audit tag; got: {msg}"
    );
}

/// ENG-AUD-2026-S09-001: put_watch must reject an empty watch_id.
#[test]
fn put_watch_rejects_empty_watch_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let mut watch = make_watch("", "/code/src");
    watch.watch_id = String::new();

    let result = reg.put_watch("proj-watch-empty", &watch);
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: put_watch must reject empty watch_id"
    );
}

/// ENG-AUD-2026-S09-001: set_meta must reject a key containing \0.
#[test]
fn set_meta_rejects_nul_in_key() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let result = reg.set_meta("proj-meta-adv", "key\0evil", "value");
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: set_meta must reject key containing \\0"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("ENG-AUD-2026-S09-001"),
        "error must cite audit tag; got: {msg}"
    );
}

/// ENG-AUD-2026-S09-001: set_meta must reject an empty key.
#[test]
fn set_meta_rejects_empty_key() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let result = reg.set_meta("proj-meta-empty", "", "value");
    assert!(
        result.is_err(),
        "ENG-AUD-2026-S09-001: set_meta must reject empty meta key"
    );
}

// ── REG1: key-space aliasing prevention — read methods must also validate ──

#[test]
fn get_memory_section_rejects_null_byte_in_project_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = Registry::open(&tmp.path().join("r.redb")).unwrap();
    // Adversarial project_id with embedded NUL delimiter
    let result = reg.get_memory_section("proj\0other", "sec");
    assert!(
        result.is_err(),
        "get_memory_section must reject NUL in project_id"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("invalid")
            || msg.contains("null")
            || msg.contains("\\0")
            || msg.contains("NUL")
            || msg.contains("ENG-AUD"),
        "error should mention invalid key component; got: {msg}"
    );
}

#[test]
fn get_meta_rejects_null_byte_in_key() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = Registry::open(&tmp.path().join("r.redb")).unwrap();
    let result = reg.get_meta("proj", "key\0injected");
    assert!(result.is_err(), "get_meta must reject NUL in key");
}

#[test]
fn list_memory_sections_rejects_null_byte_in_project_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = Registry::open(&tmp.path().join("r.redb")).unwrap();
    let result = reg.list_memory_sections("proj\0other");
    assert!(
        result.is_err(),
        "list_memory_sections must reject NUL in project_id"
    );
}

#[test]
fn get_project_rejects_null_byte_in_project_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = Registry::open(&tmp.path().join("r.redb")).unwrap();
    let result = reg.get_project("proj\0bad");
    assert!(result.is_err(), "get_project must reject NUL in project_id");
}

/// ENG-AUD-2026-S09-001: valid IDs containing hyphens and underscores must
/// still be accepted (regression check — validation must not be too broad).
#[test]
fn composite_key_validation_accepts_normal_ids() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    // These should all succeed — they are normal IDs without NUL bytes.
    reg.put_memory_section(
        "proj-normal",
        &make_memory_section("section-001_v2", "Title"),
    )
    .expect("put_memory_section must accept id with hyphens and underscores");

    reg.put_repo_rule("proj-normal", &make_repo_rule("rule_001-alpha", "*.cs"))
        .expect("put_repo_rule must accept id with hyphens and underscores");

    reg.put_watch("proj-normal", &make_watch("watch-alpha_01", "/code"))
        .expect("put_watch must accept id with hyphens and underscores");

    reg.set_meta("proj-normal", "active_generation", "5")
        .expect("set_meta must accept key with underscores");
}

/// REG1-h3u7: NUL byte in project_id must be rejected at the write boundary.
///
/// Registry composite keys use NUL as a separator (e.g. "{project_id}\0{section_id}").
/// A NUL byte embedded in project_id would corrupt the composite key, allowing a
/// project to masquerade as a different project or corrupt memory_bank/repo_rule entries.
#[test]
fn put_project_rejects_nul_byte_in_project_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let bad = make_project("proj\0evil", "NUL injection attempt");
    let result = reg.put_project(&bad);
    assert!(
        result.is_err(),
        "REG1-h3u7: put_project must reject project_id containing NUL byte; \
         NUL is the registry composite-key separator and would corrupt key lookups"
    );
}

/// REG1-h3u7: Newline byte in project_id must be rejected at the write boundary.
///
/// Several storage backends use newline as a record separator. A newline in
/// project_id would allow an attacker to inject extra records or pollute composite
/// key ranges used by memory_bank and repo_rule lookups.
#[test]
fn put_project_rejects_newline_in_project_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let bad = make_project("proj\nevil", "newline injection attempt");
    let result = reg.put_project(&bad);
    assert!(
        result.is_err(),
        "REG1-h3u7: put_project must reject project_id containing newline byte; \
         newline is a record separator in several backends and would corrupt key ranges"
    );
}

/// REG1-h3u7: get_project with NUL in project_id must also be rejected.
///
/// Ensures that read operations cannot bypass the validation gate by probing
/// composite keys that contain the separator character.
#[test]
fn get_project_rejects_nul_byte_in_project_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = open_registry(&tmp);

    let result = reg.get_project("proj\0evil");
    assert!(
        result.is_err(),
        "REG1-h3u7: get_project must reject project_id containing NUL byte"
    );
}
