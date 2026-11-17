//! Imported review conditions survive promotion, identity and source injection.
use anyhow::{Result, ensure};
use engram_core::{Config, RelPath, registry::RepoRule};
use engram_server::{AppState, Engram, services::project_service};
use futures::FutureExt;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use std::{collections::BTreeSet, panic::AssertUnwindSafe};

const FILE: &str = "Site/reviewed.ts";
const CODE: &str = "export function reviewedFinding(): string { return \"owned source\"; }\n";
const PREFIX: &str = "Validate organization authorization before accepting a customer-supplied account identifier. Resolve the account within the current organization and preserve existing audit metadata. Review both the selected account and its parent organization, keep the caller-visible error behavior stable, and avoid broadening write access while reusing the existing Ångström validation helper. This explanation concerns the submitted write flow rather than every read in the application.\n";

fn messages() -> (String, String) {
    (
        format!(
            "{PREFIX}Exception: read-only preview requests retain their existing authorization path; do not submit or revoke sessions."
        ),
        format!(
            "{PREFIX}Exception: explicitly delegated service accounts retain the documented opt-in delegation condition."
        ),
    )
}

fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

async fn import(server: &Engram, project: &str, arguments: Value) -> Result<CallToolResult> {
    let mut arguments = arguments;
    arguments["project_id"] = json!(project);
    Ok(server
        .handle_ingest_review_findings(serde_json::from_value(arguments)?)
        .await?)
}

async fn run(mode: &str) -> Result<()> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("rq-").tempdir_in(parent)?;
    let root = owned.path().join("root");
    std::fs::create_dir_all(root.join("Site"))?;
    std::fs::write(root.join(FILE), CODE)?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        watch_debounce_secs: 3600,
        ..Default::default()
    })?;
    let server = Engram::new(state);
    let result = AssertUnwindSafe(async {
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned review qualifications","project_type":"general","wait":true,
        }))?).await?;
        let project = body(&indexed).split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let (a, b) = messages();
        ensure!(a.chars().count() > 300 && a.chars().take(300).eq(b.chars().take(300)));
        if mode == "manual" {
            import(&server, project, json!({"findings":[{"file":FILE,"severity":"critical","message":a}]})).await?;
            let rules = server.state.registry.list_repo_rules(project)?;
            ensure!(rules.len() == 1 && rules[0].rule_text == a && rules[0].file_pattern == FILE);
            let injected = project_service::inject_repo_rules(&server.state, project, &RelPath::new(FILE), CODE).await;
            ensure!(injected.contains(&a) && injected.ends_with(CODE));
            let unrelated = project_service::inject_repo_rules(&server.state, project, &RelPath::new("Site/unrelated.ts"), CODE).await;
            ensure!(!unrelated.contains(&a));
            ensure!(std::fs::read_to_string(root.join(FILE))? == CODE);
        } else if mode == "identities" {
            let legacy = RepoRule {
                rule_id: "retained_legacy_review".into(), file_pattern: FILE.into(),
                rule_text: a.chars().take(300).collect(), priority: 2, updated_at_ms: 12345,
                introduced_at: None, provenance: Some("Existing legacy finding; ownership unresolved".into()),
            };
            server.state.registry.put_repo_rule(project, &legacy)?;
            let request = json!({"findings":[{"file":FILE,"severity":"blocker","message":a},
                {"file":FILE,"severity":"HIGH","message":b}]});
            import(&server, project, request.clone()).await?;
            let first = server.state.registry.list_repo_rules(project)?;
            ensure!(first.len() == 3 && first.iter().any(|r| *r == legacy));
            let full: Vec<_> = first.iter().filter(|r| r.rule_id != legacy.rule_id).collect();
            ensure!(full.iter().any(|r| r.rule_text == a) && full.iter().any(|r| r.rule_text == b));
            let identities: BTreeSet<_> = full.iter().map(|r| r.rule_id.clone()).collect();
            ensure!(identities.len() == 2);
            import(&server, project, request).await?;
            let repeated = server.state.registry.list_repo_rules(project)?;
            ensure!(repeated.len() == 3 && repeated.iter().any(|r| *r == legacy));
            ensure!(repeated.iter().filter(|r| r.rule_id != legacy.rule_id).map(|r| r.rule_id.clone()).collect::<BTreeSet<_>>() == identities);
        } else {
            let sonar = json!({"issues":[
                {"component":format!("owned:{FILE}"),"rule":"typescript:owned","severity":"CRITICAL","message":a},
                {"component":format!("owned:{FILE}"),"severity":"MINOR","message":b},
                {"severity":"CRITICAL","message":"Project-wide imported observation without a file scope"},
            ]}).to_string();
            import(&server, project, json!({"sonarqube_json":sonar,"promote_rules":false})).await?;
            ensure!(server.state.registry.list_repo_rules(project)?.is_empty());
            import(&server, project, json!({"sonarqube_json":sonar,"promote_rules":true})).await?;
            let rules = server.state.registry.list_repo_rules(project)?;
            ensure!(rules.len() == 1 && rules[0].rule_text == a && rules[0].file_pattern == FILE);
            let injected = project_service::inject_repo_rules(&server.state, project, &RelPath::new(FILE), CODE).await;
            ensure!(injected.contains(&a) && !injected.contains(&b));
        }
        Ok::<(), anyhow::Error>(())
    }).catch_unwind().await;
    drop(server);
    owned.close()?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_manual_finding_and_exception_survive_rule_storage_and_scoped_source_injection()
-> Result<()> {
    run("manual").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_prefix_findings_keep_distinct_stable_full_identities_without_rewriting_legacy_rules()
-> Result<()> {
    run("identities").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sonar_qualifications_survive_with_existing_opt_out_and_severity_scope_eligibility()
-> Result<()> {
    run("sonar").await
}
