#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P0-4), owner decision 2026-08-30
//! ("build the callee-expansion arm now"): live r37 left two golden rows
//! wrong because the answer lives ONE CALL away from the cited entry point
//! — "how does a bulk update get authorized" is answered by the
//! CanUserBulkUpdate callee, not by the API file that calls it. The engine
//! follows one bounded call-graph hop from the cited files and keeps callees
//! whose name matches the question's cues. A named FILE entity's definition
//! evidence also survives the evidence cap (P0-4b).

use engram_core::config::Config;
use engram_server::models::AskCodebaseRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    for d in [
        "Site/App_Code/api-json",
        "Site/App_Code/dal",
        "Site/App_Code/noise",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    // The API entry point: authorization happens in a callee defined elsewhere.
    std::fs::write(
        root.join("Site/App_Code/api-json/api-installationsobjektprojekt.vb"),
        "Public Class api_installationsobjektprojekt\n    Public Function ioUpdateBaseTypeInBulk(qry As Object) As String\n        Dim pr_id As Integer = GetDictionaryIntegerValue(qry, \"pr_id\")\n        If Not installationsobjektprojekt.CanUserBulkUpdate(pr_id) Then Return \"denied\"\n        Return installationsobjektprojekt.UpdateBaseType(qry)\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/dal/installationsobjektprojekt.vb"),
        "Public Class installationsobjektprojekt\n    Public Shared Function CanUserBulkUpdate(pr_id As Integer) As Boolean\n        ' the permission check: project membership and the admin role\n        Return Check_pr_id(pr_id) AndAlso IsAdminOrArbetsledare()\n    End Function\n    Public Shared Function UpdateBaseType(qry As Object) As String\n        Return \"ok\"\n    End Function\n    Private Shared Function Check_pr_id(pr_id As Integer) As Boolean\n        Return pr_id > 0\n    End Function\n    Private Shared Function IsAdminOrArbetsledare() As Boolean\n        Return True\n    End Function\nEnd Class\n",
    )
    .unwrap();
    // Enough code chunks about "bulk", "update" and "installation objects" to
    // fill the evidence cap on their own — without the callee hop the
    // authorization function is never cited.
    for i in 0..30 {
        std::fs::write(
            root.join(format!("Site/App_Code/noise/bulk_installation_update{i:02}.vb")),
            format!(
                "Public Class bulk_installation_update{i:02}\n    ' bulk update of installation objects base type via the API entry point\n    Public Function ApplyBulkUpdate{i}(qry As Object) As String\n        Return \"bulk update installation objects api entry point\"\n    End Function\nEnd Class\n"
            ),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(200),
        max_project_bytes: Some(4 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "CalleeHop".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid)
}

async fn ask(engram: &Engram, pid: &str, question: &str) -> Value {
    let req: AskCodebaseRequest = serde_json::from_value(json!({
        "project_id": pid,
        "question": question,
        "output_format": "json",
        "depth": "standard"
    }))
    .unwrap();
    let res = engram.handle_ask_codebase(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let start = t.find('{').unwrap_or(0);
    serde_json::from_str(&t[start..]).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"))
}

fn blob(v: &Value) -> String {
    v["evidence"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    format!(
                        "{} {}",
                        e["path"].as_str().unwrap_or(""),
                        e["content"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
        .to_lowercase()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_authorization_callee_one_hop_from_the_entry_point_is_cited() {
    let (_tmp, engram, pid) = build().await;
    let v = ask(
        &engram,
        &pid,
        "How does a bulk base-type update on installation objects get authorized, from the API entry point to the permission check?",
    )
    .await;
    let b = blob(&v);
    assert!(
        b.contains("canuserbulkupdate"),
        "the callee that authorizes the update must be cited (one hop from the entry point):\n{b}"
    );
    assert!(
        b.contains("dal/installationsobjektprojekt.vb"),
        "the callee's defining file is cited:\n{b}"
    );
    let providers = v["providers"].to_string().to_lowercase();
    assert!(
        providers.contains("callee"),
        "the callee arm reports itself: {providers}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_file_entity_survives_the_evidence_cap() {
    let (_tmp, engram, pid) = build().await;
    let v = ask(
        &engram,
        &pid,
        "How are permission checks done in api-installationsobjektprojekt, and which endpoints read a client-supplied project id?",
    )
    .await;
    let b = blob(&v);
    assert!(
        b.contains("api-json/api-installationsobjektprojekt.vb"),
        "the file the question names is cited even when 30 look-alike chunks fill the cap:\n{b}"
    );
}
