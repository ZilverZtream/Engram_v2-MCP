#![allow(clippy::unwrap_used)]
//! End-to-end tests for the planning trio: get_concept_footprint,
//! find_similar_changes, find_implementation_pattern.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

fn test_config(root: &std::path::Path, data_dir_name: &str) -> Config {
    Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join(data_dir_name),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    }
}

async fn index(engram: &engram_server::Engram, dir: &std::path::Path, name: &str) {
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: dir.to_string_lossy().to_string(),
            project_name: name.into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn concept_footprint_groups_touchpoints_by_role() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    std::fs::write(
        root.join("PhotoUpload.aspx"),
        r#"<%@ Page Inherits="App.PhotoUpload" CodeBehind="PhotoUpload.aspx.cs" %>
<asp:Button ID="btnUploadPhoto" runat="server" OnClick="btnUploadPhoto_Click" />"#,
    )
    .unwrap();
    std::fs::write(
        root.join("PhotoUpload.aspx.cs"),
        r#"
namespace App {
    public partial class PhotoUpload {
        protected void btnUploadPhoto_Click(object sender, System.EventArgs e) {
            var cmd = new SqlCommand("SELECT * FROM Photos");
        }
    }
}"#,
    )
    .unwrap();
    // Mentions the concept only in text — name and symbols don't match.
    std::fs::write(
        root.join("Misc.cs"),
        r#"
namespace App {
    // helper used by the photo gallery batch job
    public class Misc { public void Run() {} }
}"#,
    )
    .unwrap();

    let cfg = test_config(root, "engram_data_fp");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    index(&engram, root, "FootprintTest").await;
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    let res = engram
        .get_concept_footprint(Parameters(
            engram_server::models::GetConceptFootprintRequest {
                project_id: project_id.clone(),
                concept: "photo".into(),
                max_per_group: 15,
            },
        ))
        .await
        .unwrap();
    let text = &res.content[0].as_text().unwrap().text;
    println!("FOOTPRINT OUTPUT:\n{text}");

    assert!(
        text.contains("btnUploadPhoto"),
        "UI group should list the upload control/handler"
    );
    assert!(
        text.contains("PhotoUpload"),
        "logic/UI groups should list the page class or page"
    );
    assert!(
        text.contains("Misc.cs"),
        "lexical-only section should surface the file that merely mentions the concept"
    );
    assert!(
        text.contains("get_index_freshness"),
        "footer must be present"
    );
}

#[tokio::test]
async fn find_similar_changes_reports_missing_companions() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let repo = git2::Repository::init(root).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();

    let commit_files = |files: &[(&str, &str)], msg: &str, parent: Option<git2::Oid>| {
        for (path, content) in files {
            let full = root.join(path);
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }
        let mut index = repo.index().unwrap();
        for (path, _) in files {
            index.add_path(std::path::Path::new(path)).unwrap();
        }
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parents: Vec<git2::Commit> = parent
            .map(|p| vec![repo.find_commit(p).unwrap()])
            .unwrap_or_default();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
            .unwrap()
    };

    let c1 = commit_files(
        &[
            ("OrderSettings.aspx", "<%@ Page %>"),
            ("OrderSettings.aspx.cs", "class OrderSettings {}"),
            ("Admin/menu.xml", "<menu><item>orders</item></menu>"),
        ],
        "feat: order settings page",
        None,
    );
    let c2 = commit_files(
        &[
            ("UserSettings.aspx", "<%@ Page %>"),
            ("UserSettings.aspx.cs", "class UserSettings {}"),
            (
                "Admin/menu.xml",
                "<menu><item>orders</item><item>users</item></menu>",
            ),
        ],
        "feat: user settings page",
        Some(c1),
    );
    commit_files(
        &[
            ("ReportSettings.aspx", "<%@ Page %>"),
            ("ReportSettings.aspx.cs", "class ReportSettings {}"),
            (
                "Admin/menu.xml",
                "<menu><item>orders</item><item>users</item><item>reports</item></menu>",
            ),
        ],
        "feat: report settings page",
        Some(c2),
    );

    let cfg = test_config(root, "engram_data_sim");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    index(&engram, root, "SimilarTest").await;
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    let res = engram
        .find_similar_changes(Parameters(
            engram_server::models::FindSimilarChangesRequest {
                project_id: project_id.clone(),
                files: vec!["PhotoSettings.aspx".into(), "PhotoSettings.aspx.cs".into()],
                max_commits: 100,
                top: 3,
            },
        ))
        .await
        .unwrap();
    let text = &res.content[0].as_text().unwrap().text;
    println!("SIMILAR OUTPUT:\n{text}");

    assert!(
        text.contains("settings page"),
        "should surface the similar historical feature commits"
    );
    assert!(
        text.contains("MISSING from your set"),
        "companion section must be present"
    );
    assert!(
        text.contains("Admin/menu.xml"),
        "the recurring menu registration the plan forgot must be reported"
    );
}

#[tokio::test]
async fn find_implementation_pattern_returns_exemplars_with_common_ingredients() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    for name in ["OrdersAdmin", "UsersAdmin"] {
        std::fs::write(
            root.join(format!("{name}.aspx.cs")),
            format!(
                r#"
namespace App {{
    public partial class {name} {{
        protected void SaveSettings(object sender, System.EventArgs e) {{
            var cmd = new SqlCommand("sp_SaveSettings");
        }}
    }}
}}"#
            ),
        )
        .unwrap();
    }

    let cfg = test_config(root, "engram_data_pat");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    index(&engram, root, "PatternTest").await;
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    let res = engram
        .find_implementation_pattern(Parameters(
            engram_server::models::FindImplementationPatternRequest {
                project_id: project_id.clone(),
                pattern_query: "SaveSettings".into(),
                max_examples: 3,
                output_json: false,
            },
        ))
        .await
        .unwrap();
    let text = &res.content[0].as_text().unwrap().text;
    println!("PATTERN OUTPUT:\n{text}");

    assert!(
        text.contains("OrdersAdmin.aspx.cs") && text.contains("UsersAdmin.aspx.cs"),
        "both exemplar files should be listed"
    );
    assert!(
        text.contains("sp_SaveSettings"),
        "the shared stored proc should appear as a data edge / common ingredient"
    );
    assert!(
        text.contains("Exemplar #1"),
        "exemplar cards should be rendered"
    );
}

#[tokio::test]
async fn map_guards_and_settings_reports_parity_and_setting_reads() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    std::fs::write(
        root.join("web.config"),
        r#"<?xml version="1.0"?>
<configuration>
  <appSettings>
    <add key="MinPhotosRequired" value="3" />
  </appSettings>
</configuration>"#,
    )
    .unwrap();
    std::fs::write(
        root.join("AdminApi.aspx.cs"),
        r#"
namespace App {
    public partial class AdminApi {
        protected void DeleteUser(object sender, System.EventArgs e) {
            if (!User.IsInRole("Admin")) { return; }
        }
        protected void ListUsers(object sender, System.EventArgs e) {
            var min = ConfigurationManager.AppSettings["MinPhotosRequired"];
        }
    }
}"#,
    )
    .unwrap();

    let cfg = test_config(root, "engram_data_guards");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    index(&engram, root, "GuardsTest").await;
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    let res = engram
        .map_guards_and_settings(Parameters(
            engram_server::models::MapGuardsAndSettingsRequest {
                project_id: project_id.clone(),
                scope: Some("AdminApi".into()),
                output_json: false,
            },
        ))
        .await
        .unwrap();
    let text = &res.content[0].as_text().unwrap().text;
    println!("GUARDS OUTPUT:\n{text}");

    assert!(text.contains("Guard parity"), "parity section required");
    assert!(
        text.contains("UNGUARDED: ListUsers"),
        "the unguarded sibling must be called out"
    );
    assert!(
        text.contains("DeleteUser") && text.contains("isinrole"),
        "guarded function with its check must be listed"
    );
    assert!(
        text.contains("MinPhotosRequired"),
        "the setting read in scope must be reported"
    );
    assert!(
        text.contains("roles referenced: Admin"),
        "role literal must surface in house patterns"
    );
}

#[tokio::test]
async fn plan_user_story_produces_brief_with_checklist() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    std::fs::write(
        root.join("PhotoUpload.aspx.cs"),
        r#"
namespace App {
    public partial class PhotoUpload {
        protected void btnUploadPhoto_Click(object sender, System.EventArgs e) {
            var min = ConfigurationManager.AppSettings["MinPhotosRequired"];
        }
    }
}"#,
    )
    .unwrap();

    let cfg = test_config(root, "engram_data_story");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    index(&engram, root, "StoryTest").await;
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    let res = engram
        .plan_user_story(Parameters(engram_server::models::PlanUserStoryRequest {
            project_id: project_id.clone(),
            story: "As an admin I would like to set minimum number of photos required".into(),
            concepts: None,
        }))
        .await
        .unwrap();
    let text = &res.content[0].as_text().unwrap().text;
    println!("STORY BRIEF:\n{text}");

    assert!(text.contains("concepts: photos"), "concept extraction");
    assert!(
        text.contains("Concept footprint: 'photos'"),
        "per-concept footprint section"
    );
    assert!(text.contains("## Checklist"), "checklist section");
    assert!(
        text.contains("detect_incomplete_changes") && text.contains("pre_commit_review"),
        "checklist must chain into the completeness (fast, precomputed) and review tools"
    );
}
