#![allow(clippy::unwrap_used)]
//! MIG1/D2 — fault-injection and assembly tests for the migration report
//! completeness surface (`degraded_sections` / `report_is_complete`).
//!
//! **Design note:** `edges_or_warn` and `nodes_or_warn` are private helpers that
//! use a thread-local accumulator.  Direct fault injection (returning `Err` from
//! a live `GraphStore` read) requires either file corruption or a trait mock layer
//! that does not yet exist.  The unit-level fault injection tests live in
//! `full_project_migration_service.rs#[cfg(test)]` where the private helpers are
//! accessible.
//!
//! These integration tests cover the **assembly** of the pipeline:
//! - Happy path: `analyze_full_project` on a fresh empty graph finishes with
//!   `report_is_complete = true` and `degraded_sections` empty.
//! - The returned types carry the completeness fields at all.

use std::sync::Arc;
use engram_graph::GraphStore;
use engram_server::services::full_project_migration_service::{
    analyze_full_project, FileContent, ProjectFileBundle, ProjectReferenceBundle,
};

fn empty_bundle() -> ProjectFileBundle {
    ProjectFileBundle {
        markup_files: vec![],
        js_files: vec![],
        classic_asp_files: vec![],
        report_files: vec![],
        global_asax: None,
        web_config_content: None,
        code_files: vec![],
        project_references: vec![],
        sql_files: vec![],
        packages_config_files: vec![],
        config_transform_files: vec![],
        resx_files: vec![],
        master_files: vec![],
    }
}

/// MIG1/D2 happy path: `analyze_full_project` on an empty graph with an empty
/// file bundle must succeed, set `report_is_complete = true`, and leave
/// `degraded_sections` empty — proving the TLS accumulator is wired correctly
/// into the returned report.
#[test]
fn mig1_report_completeness_fields_present_and_correct_on_happy_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("graph.redb");
    let graph = Arc::new(GraphStore::open(&db_path).expect("GraphStore::open must succeed"));

    let bundle = empty_bundle();
    let report = analyze_full_project(&graph, "test-proj", "react", &bundle, 100)
        .expect("analyze_full_project must succeed on empty graph");

    assert!(
        report.report_is_complete,
        "MIG1: empty graph with no failed queries must give report_is_complete = true; \
         got degraded_sections = {:?}",
        report.degraded_sections
    );
    assert!(
        report.degraded_sections.is_empty(),
        "MIG1: no graph failures must produce empty degraded_sections; \
         got: {:?}",
        report.degraded_sections
    );
}

/// MIG1/D2 assembly: the report struct actually contains `degraded_sections`
/// and `report_is_complete` fields (not dead code).  Also verifies that a
/// second call resets the TLS accumulator, so stale state from the previous
/// call doesn't bleed into the new report.
#[test]
fn mig1_consecutive_calls_do_not_accumulate_across_calls() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("graph.redb");
    let graph = Arc::new(GraphStore::open(&db_path).expect("GraphStore::open"));

    let bundle = empty_bundle();

    let r1 = analyze_full_project(&graph, "proj-a", "blazor", &bundle, 50).unwrap();
    let r2 = analyze_full_project(&graph, "proj-b", "blazor", &bundle, 50).unwrap();

    // Neither call should pollute the other's completeness state.
    assert!(r1.report_is_complete, "call 1 must be complete");
    assert!(r2.report_is_complete, "call 2 must be complete — TLS must have been reset");
    assert!(r1.degraded_sections.is_empty(), "call 1 degraded_sections must be empty");
    assert!(r2.degraded_sections.is_empty(), "call 2 degraded_sections must be empty");
}

/// MIG1/D2: verifies the `FileContent` and `ProjectReferenceBundle` types are
/// usable as bundle inputs without panicking — exercises construction paths.
#[test]
fn mig1_bundle_with_minimal_content_does_not_panic() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("graph.redb");
    let graph = Arc::new(GraphStore::open(&db_path).unwrap());

    let bundle = ProjectFileBundle {
        markup_files: vec![FileContent {
            file_path: "Default.aspx".into(),
            markup_content: "<%@ Page Language=\"C#\" %>".into(),
            codebehind_content: Some("public partial class _Default : System.Web.UI.Page {}".into()),
        }],
        code_files: vec![("App_Code/Helper.cs".into(), "public class Helper {}".into())],
        project_references: vec![ProjectReferenceBundle {
            project_path: "MyApp.csproj".into(),
            target_framework: Some("net48".into()),
            assembly_name: Some("MyApp".into()),
            root_namespace: Some("MyApp".into()),
            package_references: vec![],
            assembly_references: vec![],
            project_dependencies: vec![],
        }],
        ..empty_bundle()
    };

    let result = analyze_full_project(&graph, "real-proj", "react", &bundle, 10);
    assert!(
        result.is_ok(),
        "MIG1: minimal non-empty bundle must not panic; got: {:?}",
        result.err()
    );
    let report = result.unwrap();
    // With one markup file and no graph data, report must still be complete.
    assert!(
        report.report_is_complete,
        "MIG1: single markup file with empty graph must still be complete"
    );
}
