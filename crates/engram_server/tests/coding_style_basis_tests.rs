#![allow(clippy::unwrap_used)]
//! Row-5 audit (docs/audits/07-house-pattern-and-ui-conformance.md)
//! slice 2 — A5/A6/A7 for `analyze_file_coding_style`: the printed
//! confidence is the ENGINE's computed value (not a constant 1.00), the
//! evidence basis is stated (commits · file read · VB analyser ran/skipped
//! · LLM), the VB static analyser runs whenever the file can be read (not
//! only below five commits), and git/file failures are reported lines
//! instead of silently "no history".

use engram_core::config::Config;
use engram_server::models::AnalyzeFileCodingStyleRequest;
use engram_server::services::cognitive_service::analyze_file_style;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;
use std::path::Path;

const FILE: &str = "Site/App_Code/rv/categories.vb";

fn vb_body(n: usize) -> String {
    let mut s = String::from(
        "Public Class categories\n\
         \x20   Public Function GetById(id As Integer, Optional db As iFaltDataContext = Nothing) As category\n\
         \x20       If db Is Nothing Then db = New iFaltDataContext()\n\
         \x20       Using cmd As New SqlCommand()\n\
         \x20           Try\n\
         \x20               Return db.rk_categories.FirstOrDefault(Function(c) c.id = id)\n\
         \x20           Catch ex As Exception\n\
         \x20               LogError(ex)\n\
         \x20           End Try\n\
         \x20       End Using\n\
         \x20       Return Nothing\n\
         \x20   End Function\n",
    );
    for i in 0..n {
        s.push_str(&format!(
            "    Public Sub Handler{i}(sender As Object, e As EventArgs) Handles btn{i}.Click\n\
             \x20       If sender Is Nothing Then Return\n\
             \x20       SafeRedirect(\"~/x{i}.aspx\")\n\
             \x20       Return\n\
             \x20   End Sub\n"
        ));
    }
    s.push_str("End Class\n");
    s
}

fn commit_file(repo: &git2::Repository, root: &Path, rel: &str, body: &str, msg: &str) {
    let abs = root.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(&abs, body).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(rel)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parents: Vec<git2::Commit> = match repo.head().ok().and_then(|h| h.target()) {
        Some(oid) => vec![repo.find_commit(oid).unwrap()],
        None => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
        .unwrap();
}

/// A project whose VB file has SIX commits — deep enough that the old
/// `< 5 commits` gate skipped the VB analyser.
async fn build_with_history() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    for i in 1..=6 {
        commit_file(&repo, &root, FILE, &vb_body(i), &format!("step {i}"));
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(50),
        max_project_bytes: Some(2 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "StyleFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_vb_analyser_runs_on_a_file_with_deep_history_and_the_basis_says_so() {
    let (_tmp, state, _engram, pid) = build_with_history().await;
    let r = analyze_file_style(&state, &pid, FILE, 10).await;
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.basis.commits, 6, "{:?}", r.basis);
    assert!(r.basis.file_read, "{:?}", r.basis);
    assert!(
        r.basis.vb_analyser_ran,
        "the VB static analyser must run whenever the file is readable — history depth is irrelevant to it: {:?}",
        r.basis
    );
    let guide = r.style_guide.clone().unwrap_or_default();
    assert!(
        guide.contains("Is Nothing") || guide.contains("SafeRedirect") || guide.contains("Handles"),
        "a VB house rule must be in the guide:\n{guide}"
    );
    assert!(
        r.basis.failures.is_empty(),
        "a healthy repo has no provider failures: {:?}",
        r.basis.failures
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_printed_confidence_is_the_engines_value_and_the_basis_line_is_printed() {
    let (_tmp, state, engram, pid) = build_with_history().await;
    let r = analyze_file_style(&state, &pid, FILE, 10).await;
    let req: AnalyzeFileCodingStyleRequest =
        serde_json::from_value(json!({"project_id": pid, "file_path": FILE, "diff_limit": 10}))
            .unwrap();
    let res = engram.handle_analyze_file_coding_style(req).await.unwrap();
    let out = res.content[0].as_text().unwrap().text.clone();
    let expected = format!("Confidence: {:.2}", r.confidence);
    assert!(
        out.contains(&expected),
        "the handler must print the engine's confidence ({expected}):\n{out}"
    );
    assert!(
        out.contains("Basis:") && out.contains("6 commit") && out.contains("VB analyser: ran"),
        "the evidence basis must be printed:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_git_history_is_a_reported_failure_not_silence() {
    // A registered project whose directory is NOT a git repository.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("plain");
    std::fs::create_dir_all(root.join("Site/App_Code/rv")).unwrap();
    std::fs::write(root.join(FILE), vb_body(2)).unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let pid = "plain-style";
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: pid.into(),
            project_name: pid.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    let r = analyze_file_style(&state, pid, FILE, 10).await;
    assert!(
        r.error.is_none(),
        "the static analyser still produces a guide: {:?}",
        r.error
    );
    assert_eq!(r.basis.commits, 0);
    assert!(
        r.basis
            .failures
            .iter()
            .any(|f| f.to_lowercase().contains("git")),
        "the git failure must be a reported line: {:?}",
        r.basis.failures
    );
    assert!(r.basis.vb_analyser_ran, "{:?}", r.basis);
}
