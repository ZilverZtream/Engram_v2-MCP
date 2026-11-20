//! Browser storage evidence belongs in the ordinary source-bound question path.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{
    AppState, Engram,
    services::ask_engine::{browser_state, literal_state},
};
use futures::FutureExt;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use std::panic::AssertUnwindSafe;

const LOCAL: &str = "export function save() { localStorage.setItem('K', '1'); }\nexport function read() { return localStorage.getItem('K'); }\nexport function remove() { localStorage.removeItem('K'); }\n";
const CLEAR: &str = "function clearLocal() { localStorage.clear(); }\n";
const SESSION: &str = "// K is handed back from the callback.\nconst value = sessionStorage.getItem('K');\nexport function clearSession() { sessionStorage.clear(); }\n";
const GUIDE: &str = "Exception: read-only previews never submit.\n";

fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn facts(answer: &Value) -> Vec<Value> {
    answer["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["provider"] == "browser_state")
        .map(|row| {
            let mut content: Value =
                serde_json::from_str(row["content"].as_str().unwrap()).unwrap();
            content["path"] = row["path"].clone();
            content
        })
        .collect()
}

#[test]
fn browser_mentions_preserve_store_identity_and_key_case_without_activating_for_dynamic_or_legacy_keys()
 {
    let query = browser_state::queries(
        "Who writes localStorage key `K`, reads LOCALSTORAGE.getItem('K'), and removes sessionStorage.removeItem(\"K\")?",
    );
    assert_eq!(query.len(), 2);
    assert_eq!(query[0].state_type, "LocalStorage");
    assert_eq!(query[1].state_type, "SessionStorage");
    assert_eq!(
        browser_state::queries("localStorage key 'K' localStorage key 'k'").len(),
        2
    );
    assert!(
        browser_state::queries(
            "localStorage.getItem(variable) and Session(\"K\") and Application[\"K\"]"
        )
        .is_empty()
    );
    assert!(literal_state::queries("Where is localStorage key 'K' read?").is_empty());
    assert_eq!(browser_state::queries("localStorage key 'A' localStorage key 'B' localStorage key 'C' localStorage key 'D' localStorage key 'E'").len(), 4);
}

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
    root: std::path::PathBuf,
    project: String,
}
impl Fixture {
    async fn new(full: bool) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("bs-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        for dir in ["Site/Browser", "Site/Other", "Site/Outside"] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        for (path, content) in [
            ("Site/Browser/local.ts", LOCAL),
            ("Site/Browser/clear.js", CLEAR),
            ("Site/Other/session.ts", SESSION),
            (
                "Site/Outside/clear.js",
                "function outsideClear() { localStorage.clear(); }\n",
            ),
            (
                "Site/server.aspx",
                "<%@ Page Language='VB' CodeFile='server.vb' Inherits='ServerState' %>\n",
            ),
            (
                "Site/server.vb",
                "Public Class ServerState\nInherits System.Web.UI.Page\nPublic Function Read() As Object\nReturn Session(\"K\")\nEnd Function\nEnd Class\n",
            ),
            ("AGENTS.md", GUIDE),
        ] {
            std::fs::write(root.join(path), content)?;
        }
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: full,
            watch_debounce_secs: 3600,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned browser questions","project_type":"general","wait":true}))?).await?;
        let project = body(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            server,
            owned,
            root,
            project,
        })
    }
    async fn ask(&self, question: &str) -> Result<Value> {
        let result = self
            .server
            .handle_ask_codebase(serde_json::from_value(json!({"project_id":self.project,
            "question":question,"output_format":"json","max_tokens":65536,"deadline_ms":5000}))?)
            .await?;
        ensure!(result.is_error != Some(true));
        Ok(serde_json::from_str(body(&result))?)
    }
    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_browser_questions_keep_key_roles_and_separate_store_wide_clear_evidence()
-> Result<()> {
    let fixture = Fixture::new(true).await?;
    let result = AssertUnwindSafe(async {
        let answer = fixture
            .ask("Where is localStorage key 'K' read and written?")
            .await?;
        let rows = facts(&answer);
        ensure!(rows.iter().all(|row| row["state_type"] == "LocalStorage"));
        ensure!(
            rows.iter()
                .any(|row| row["roles"].as_array().unwrap().contains(&json!("writer")))
        );
        ensure!(
            rows.iter()
                .any(|row| row["roles"].as_array().unwrap().contains(&json!("reader")))
        );
        let clear = rows
            .iter()
            .find(|row| row["path"] == "Site/Browser/clear.js")
            .unwrap();
        ensure!(clear["evidence_kind"] == "browser_storage_clear_candidate");
        let span = &clear["candidate"]["byte_range"];
        let excerpt = clear["candidate"]["excerpt"].as_str().unwrap();
        ensure!(
            excerpt.contains("localStorage.clear()") && clear["candidate"]["queried_key"] == "K"
        );
        ensure!(
            CLEAR.get(span[0].as_u64().unwrap() as usize..span[1].as_u64().unwrap() as usize)
                == Some(excerpt)
        );
        ensure!(
            clear["candidate"]["source_blake3"]
                == blake3::hash(CLEAR.as_bytes()).to_hex().to_string()
        );
        ensure!(
            answer["status"] != "answered"
                && answer["project_policy_sources"]
                    .to_string()
                    .contains("read-only previews never submit")
        );
        let session = fixture
            .ask("Where is sessionStorage.getItem('K') read?")
            .await?;
        let session_rows = facts(&session);
        let read = session_rows
            .iter()
            .find(|row| row["roles"] == json!(["reader"]))
            .ok_or_else(|| anyhow::anyhow!("Missing top-level SessionStorage read"))?;
        let site = &read["access_sites"][0];
        let range = site["byte_range"].as_array().unwrap();
        let start = range[0].as_u64().unwrap() as usize;
        let end = range[1].as_u64().unwrap() as usize;
        ensure!(read["source_scope"] == "file" && read["declaration_lines"].is_null());
        ensure!(site["role"] == "reader" && site["line"] == 2);
        ensure!(SESSION.get(start..end) == Some("sessionStorage.getItem('K')"));
        ensure!(site["excerpt"] == "sessionStorage.getItem('K')");
        ensure!(read["source_recovery"]["arguments"]["line_start"] == 2);
        ensure!(
            session_rows.iter().all(|row| row["state_type"] == "SessionStorage")
        );
        let clear_only = fixture
            .ask("What can clear localStorage key 'NeverAccessedLiterally'?")
            .await?;
        ensure!(!facts(&clear_only).is_empty());
        ensure!(
            facts(&clear_only)
                .iter()
                .all(|row| row["evidence_kind"] == "browser_storage_clear_candidate")
        );
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    fixture.close()?;
    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_browser_sources_and_core_recovery_withhold_stale_members_and_keep_legacy_diagnostics()
-> Result<()> {
    let fixture = Fixture::new(false).await?;
    let result = AssertUnwindSafe(async {
        std::fs::remove_file(fixture.root.join("Site/Outside/clear.js"))?;
        let question = "Where is localStorage key 'K' read under Site/Browser?";
        let answer = fixture.ask(question).await?;
        let rows = facts(&answer);
        ensure!(
            !rows.is_empty()
                && rows
                    .iter()
                    .all(|row| row["path"].as_str().unwrap().starts_with("Site/Browser/"))
        );
        let provider = answer["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["provider"] == "browser_state")
            .unwrap();
        ensure!(!provider["note"].as_str().unwrap().contains("Site/Outside"));
        let read = rows
            .iter()
            .find(|row| row["path"] == "Site/Browser/local.ts")
            .unwrap()["source_recovery"]
            .clone();
        ensure!(read["tool"] == "get_full_method_body");
        let source = fixture
            .server
            .handle_get_full_method_body(serde_json::from_value(read["arguments"].clone())?)
            .await?;
        ensure!(
            body(&source).contains("localStorage")
                && body(&source).contains("read-only previews never submit")
        );
        std::fs::write(
            fixture.root.join("Site/Browser/local.ts"),
            format!("{LOCAL}\n// edited after indexing\n"),
        )?;
        let current = fixture.ask(question).await?;
        ensure!(
            !facts(&current)
                .iter()
                .any(|row| row["path"] == "Site/Browser/local.ts")
        );
        ensure!(
            facts(&current)
                .iter()
                .any(|row| row["path"] == "Site/Browser/clear.js")
        );
        ensure!(
            fixture
                .server
                .handle_get_full_method_body(serde_json::from_value(read["arguments"].clone())?)
                .await
                .is_err()
        );
        let mixed = fixture
            .ask("Compare localStorage key 'K' with Session(\"K\")")
            .await?;
        let providers = mixed["providers"].as_array().unwrap();
        ensure!(
            providers
                .iter()
                .any(|row| row["provider"] == "browser_state")
        );
        ensure!(
            providers
                .iter()
                .any(|row| row["provider"] == "legacy_state")
        );
        ensure!(
            mixed["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["provider"] == "legacy_state")
        );
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    fixture.close()?;
    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}
