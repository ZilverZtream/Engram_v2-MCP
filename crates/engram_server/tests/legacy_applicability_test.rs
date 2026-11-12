//! Direct public-tool applicability and independently authored neutral behavior.
use anyhow::{Context, Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::handler::server::tool::Parameters;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::path::Path;

/// Own the async runtime too: on assertion failure it still drains index jobs,
/// closes database owners and waits for blocking workers before temp teardown.
struct Fixture {
    server: Option<Engram>,
    runtime: Option<tokio::runtime::Runtime>,
    temporary: Option<tempfile::TempDir>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Some(server), Some(runtime)) = (&self.server, &self.runtime) {
            runtime.block_on(async {
                loop {
                    let jobs: Vec<_> = server
                        .state
                        .active_jobs
                        .write()
                        .await
                        .drain()
                        .map(|(_, task)| task)
                        .collect();
                    if jobs.is_empty() {
                        break;
                    }
                    for job in jobs {
                        let _finished = job.await;
                    }
                }
            });
        }
        drop(self.server.take());
        drop(self.runtime.take());
        if let Some(temporary) = self.temporary.take() {
            let path = temporary.path().to_owned();
            if let Err(error) = temporary.close() {
                if std::thread::panicking() {
                    eprintln!(
                        "Owned language fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                } else {
                    panic!(
                        "Owned language fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
}

fn text(result: CallToolResult) -> Result<String> {
    ensure!(
        result.is_error != Some(true),
        "Tool returned an error: {result:?}"
    );
    Ok(result
        .content
        .iter()
        .filter_map(|item| item.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn git_commit(root: &Path) -> Result<()> {
    let repository = git2::Repository::init(root)?;
    let mut index = repository.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repository.find_tree(tree_id)?;
    let identity = git2::Signature::now("Owned Fixture", "fixture@example.invalid")?;
    repository.commit(
        Some("HEAD"),
        &identity,
        &identity,
        "Authored language corpus",
        &tree,
        &[],
    )?;
    Ok(())
}

impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::Builder::new().prefix("ea-").tempdir()?;
        let owned = temporary.path().to_owned();
        let mut fixture = Self {
            server: None,
            runtime: None,
            temporary: Some(temporary),
        };
        fixture.runtime = Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
        );
        fixture.server = Some(
            fixture
                .runtime
                .as_ref()
                .context("runtime")?
                .block_on(async {
                    let (state, _) = AppState::new(Config {
                        data_dir: owned.join("data"),
                        allowed_roots: vec![owned],
                        embedding_backend: "fts_only".into(),
                        llm_backend: "none".into(),
                        max_project_files: Some(32),
                        max_project_bytes: Some(64 * 1024),
                        ..Default::default()
                    })?;
                    Ok::<_, anyhow::Error>(Engram::new(state))
                })?,
        );
        Ok(fixture)
    }

    async fn index(&self, name: &str, files: &[(&str, &str)]) -> Result<String> {
        let root = self.temporary.as_ref().context("owner")?.path().join(name);
        std::fs::create_dir(&root)?;
        for (file, content) in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().context("source parent")?)?;
            std::fs::write(path, content)?;
        }
        git_commit(&root)?;
        let output = text(
            self.server
                .as_ref()
                .context("server")?
                .handle_index_project(serde_json::from_value(json!({
                    "directory":root.to_string_lossy(),"project_name":name,"wait":true
                }))?)
                .await?,
        )?;
        Ok(output
            .split_once("project_id: ")
            .and_then(|(_, tail)| tail.lines().next())
            .with_context(|| format!("Missing project identity: {output}"))?
            .trim()
            .to_owned())
    }
}

const TS: &str = "export function createOrder(): number {\n    return 17;\n}\nexport function findOrder(): number {\n    return 23;\n}\nexport function closeOrder(): number {\n    return 29;\n}\n";
const PAGE: &str = "<%@ Page Language=\"C#\" %>\n<asp:TextBox ID=\"OrderName\" runat=\"server\" />\n<asp:RequiredFieldValidator ID=\"RequiredOrderName\" runat=\"server\" ControlToValidate=\"OrderName\" ErrorMessage=\"Order name required\" />\n";

fn not_applicable(output: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(output)?;
    ensure!(
        value["status"] == "not_applicable" && value["analysis_performed"] == false,
        "Expected a truthful no-op, got {output}"
    );
    ensure!(
        value["message"]
            .as_str()
            .is_some_and(|message| message.contains("Not applicable")),
        "Missing explanation"
    );
    Ok(())
}

#[test]
fn direct_legacy_calls_use_the_requested_project_and_do_not_write_progress() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .runtime
        .as_ref()
        .context("runtime")?
        .block_on(async {
            let server = fixture.server.as_ref().context("server")?;
            let node = fixture
                .index(
                    "node",
                    &[
                        ("package.json", "{\"name\":\"owned-node\"}"),
                        ("app.ts", TS),
                    ],
                )
                .await?;
            // Registering a separate WebForms project must not authorize Node analysis.
            let _legacy = fixture.index("legacy", &[("Default.aspx", PAGE)]).await?;
            let before = text(
                server
                    .handle_project_info(serde_json::from_value(json!({"project_id":node}))?)
                    .await?,
            )?;
            not_applicable(&text(
                server
                    .generate_migration_plan(Parameters(serde_json::from_value(
                        json!({"project_id":node}),
                    )?))
                    .await?,
            )?)?;
            not_applicable(&text(
                server
                    .suggest_migration_order(Parameters(serde_json::from_value(
                        json!({"project_id":node}),
                    )?))
                    .await?,
            )?)?;
            not_applicable(&text(
                server
                    .update_migration_status(Parameters(serde_json::from_value(
                        json!({"project_id":node,"file_path":"app.ts","status":"migrated"}),
                    )?))
                    .await?,
            )?)?;
            not_applicable(&text(
                server
                    .generate_instrumentation_code(Parameters(serde_json::from_value(
                        json!({"project_id":node,"target_files":["app.ts"],"language":"csharp"}),
                    )?))
                    .await?,
            )?)?;
            let progress = text(
                server
                    .handle_get_migration_progress(serde_json::from_value(
                        json!({"project_id":node}),
                    )?)
                    .await?,
            )?;
            ensure!(
                progress.contains("**Total files**: 0") && !progress.contains("app.ts"),
                "Unsupported call wrote progress: {progress}"
            );
            let after = text(
                server
                    .handle_project_info(serde_json::from_value(json!({"project_id":node}))?)
                    .await?,
            )?;
            ensure!(
                before == after,
                "Unsupported calls changed source/project information"
            );
            Ok(())
        })
}

#[test]
fn mixed_project_rejects_typescript_page_but_keeps_authored_webforms_validation() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .runtime
        .as_ref()
        .context("runtime")?
        .block_on(async {
            let server = fixture.server.as_ref().context("server")?;
            let mixed = fixture
                .index(
                    "mixed",
                    &[
                        ("package.json", "{}"),
                        ("app.ts", TS),
                        ("Default.aspx", PAGE),
                    ],
                )
                .await?;
            not_applicable(&text(
                server
                    .get_page_context(Parameters(serde_json::from_value(
                        json!({"project_id":mixed,"aspx_file":"app.ts"}),
                    )?))
                    .await?,
            )?)?;
            not_applicable(&text(
                server
                    .generate_characterization_tests(Parameters(serde_json::from_value(
                        json!({"project_id":mixed,"file_path":"app.ts"}),
                    )?))
                    .await?,
            )?)?;
            let validators = text(
                server
                    .map_validation_controls(Parameters(serde_json::from_value(
                        json!({"project_id":mixed,"file_path":"Default.aspx"}),
                    )?))
                    .await?,
            )?;
            ensure!(
                validators.contains("RequiredOrderName") && validators.contains("OrderName"),
                "Lost authored validator: {validators}"
            );
            let traversal = server
                .map_validation_controls(Parameters(serde_json::from_value(
                    json!({"project_id":mixed,"file_path":"../outside.aspx"}),
                )?))
                .await;
            ensure!(traversal.is_err(), "Applicability bypassed path validation");
            Ok(())
        })
}

#[test]
fn node_guidance_and_style_use_authored_source_without_inventing_a_test_script() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .runtime
        .as_ref()
        .context("runtime")?
        .block_on(async {
            let server = fixture.server.as_ref().context("server")?;
            let node = fixture
                .index(
                    "node",
                    &[
                        ("package.json", "{\"name\":\"owned-node\",\"private\":true}"),
                        ("app.ts", TS),
                    ],
                )
                .await?;
            let guide = text(
                server
                    .produce_claude_md(Parameters(serde_json::from_value(
                        json!({"project_id":node,"write_to_disk":false}),
                    )?))
                    .await?,
            )?;
            ensure!(
                guide.contains("TypeScript")
                    && !guide.contains("npm test")
                    && !guide.contains("Page_Load"),
                "Invented non-Node guidance: {guide}"
            );
            let style = text(
                server
                    .analyze_file_coding_style(Parameters(serde_json::from_value(
                        json!({"project_id":node,"file_path":"app.ts"}),
                    )?))
                    .await?,
            )?;
            ensure!(
                style.contains("camelCase")
                    && style.contains("createOrder")
                    && !style.contains("VB.NET")
                    && !style.contains("ViewState"),
                "Wrong source style: {style}"
            );
            Ok(())
        })
}

#[test]
fn non_dotnet_retrieval_preserves_source_and_uses_authored_benchmark_queries() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .runtime
        .as_ref()
        .context("runtime")?
        .block_on(async {
            let server = fixture.server.as_ref().context("server")?;
            let node = fixture
                .index("node", &[("package.json", "{}"), ("app.ts", TS)])
                .await?;
            let defaults = text(
                server
                    .benchmark_retrieval(Parameters(serde_json::from_value(
                        json!({"project_id":node}),
                    )?))
                    .await?,
            )?;
            not_applicable(&defaults)?;
            ensure!(
                defaults.contains("custom_queries"),
                "No usable next step: {defaults}"
            );
            let custom = text(
                server
                    .benchmark_retrieval(Parameters(serde_json::from_value(json!({
                        "project_id":node,"output_json":true,
                        "custom_queries":[{"query":"createOrder","relevant_paths":["app.ts"]}]
                    }))?))
                    .await?,
            )?;
            let result: serde_json::Value = serde_json::from_str(&custom)?;
            ensure!(
                result["query_count"] == 1
                    && result["per_query_results"][0]["actual_top_paths"] == json!(["app.ts"]),
                "Wrong authored benchmark result: {custom}"
            );
            let hits = text(server.handle_search_memory(serde_json::from_value(json!({
            "project_id":node,"query":"createOrder","semantic":false,"use_mmr":false,"max_results":5
        }))?).await?)?;
            let recovery = hits
                .lines()
                .find_map(|line| {
                    line.strip_prefix("full_chunk: get_chunk(")
                        .and_then(|rest| rest.strip_suffix(')'))
                })
                .with_context(|| format!("Missing public recovery call: {hits}"))?;
            let mut request: serde_json::Value = serde_json::from_str(recovery)?;
            request["inject_rules"] = json!(false);
            request["logical_slice"] = json!("state_access");
            let chunk = text(
                server
                    .handle_get_chunk(serde_json::from_value(request)?)
                    .await?,
            )?;
            ensure!(
                chunk.contains("logical_slice_status: not_applicable")
                    && chunk.contains("createOrder")
                    && chunk.contains("return 17"),
                "Unsupported slice lost authored source: {chunk}"
            );
            Ok(())
        })
}

#[test]
fn neutral_guidance_style_and_review_work_for_four_authored_source_languages() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.runtime.as_ref().context("runtime")?.block_on(async {
        let server = fixture.server.as_ref().context("server")?;
        let python = "def create_order():\n    return 17\ndef find_order():\n    return 23\ndef close_order():\n    return 29\n";
        let rust = "pub fn create_order() -> i32 {\n    17\n}\npub fn find_order() -> i32 {\n    23\n}\npub fn close_order() -> i32 {\n    29\n}\n";
        let go = "package orders\nfunc CreateOrder() int {\n    return 17\n}\nfunc FindOrder() int {\n    return 23\n}\nfunc CloseOrder() int {\n    return 29\n}\n";
        for (language, path, source, manifest, manifest_body, convention, comment) in [
            ("TypeScript", "app.ts", TS, "package.json", "{\"name\":\"owned\",\"scripts\":{\"test\":\"node --test\"}}", "camelCase", "//"),
            ("Python", "app.py", python, "pyproject.toml", "[project]\nname='owned'\nversion='0.1.0'\n[tool.pytest.ini_options]\ntestpaths=['tests']\n", "snake_case", "#"),
            ("Rust", "src/lib.rs", rust, "Cargo.toml", "[package]\nname='owned'\nversion='0.1.0'\nedition='2021'\n", "snake_case", "//"),
            ("Go", "app.go", go, "go.mod", "module example.com/owned\ngo 1.23\n", "PascalCase", "//"),
        ] {
            let pid = fixture.index(language, &[(manifest, manifest_body), (path, source)]).await?;
            let guide = text(server.produce_claude_md(Parameters(serde_json::from_value(json!({"project_id":pid,"write_to_disk":false}))?)).await?)?;
            ensure!(guide.contains(language) && !guide.contains("Page_Load") && !guide.contains("ViewState"), "{language} guidance invented WebForms requirements: {guide}");
            let style = text(server.analyze_file_coding_style(Parameters(serde_json::from_value(json!({"project_id":pid,"file_path":path}))?)).await?)?;
            ensure!(style.contains(convention) && !style.contains("VB.NET"), "{language} authored naming was not recovered: {style}");
            for arguments in [json!({"project_id":pid,"file_path":path}), json!({"project_id":pid})] {
                let business = text(server.analyze_business_logic(Parameters(serde_json::from_value(arguments)?)).await?)?;
                not_applicable(&business)?;
                ensure!(business.contains("No model call or persistence performed"), "Unsupported business analysis was not explicit: {business}");
            }
            // Deliberately synthetic token shape, constructed only inside this owned fixture.
            let added = format!("{comment} owned fixture: sk-{}", "OwnedNotARealCredential".repeat(2));
            let directory = fixture.temporary.as_ref().context("owner")?.path().join(language);
            std::fs::write(directory.join(path), format!("{source}{added}\n"))?;
            let line = source.lines().count();
            let last = source.lines().last().context("last authored line")?;
            let diff = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -{line},1 +{line},2 @@\n {last}\n+{added}\n");
            let reviewed = text(server.pre_commit_review(Parameters(serde_json::from_value(json!({"project_id":pid,"diff":diff,"output_json":true}))?)).await?)?;
            let review: serde_json::Value = serde_json::from_str(&reviewed)?;
            ensure!(review["findings"].as_array().context("findings")?.iter().any(|finding| finding["gate"]=="secret_leakage" && finding["file_path"]==path && finding["lines"]==json!([line+1])), "{language} review missed the authored changed line: {reviewed}");
            for gate in ["guard_parity", "state", "ui_house_style"] {
                let outcome = review["gate_status"].as_array().context("gate status")?.iter().find(|outcome| outcome["name"]==gate).context("specialized gate outcome")?;
                ensure!(outcome["status"]["kind"]=="not_applicable" && outcome["source_scope"]["not_applicable_files"]==json!([path]), "{language}: {gate} claimed unsupported analysis: {outcome}");
            }
            ensure!(review["summary"]["gates_not_applicable"]==3, "Wrong gate coverage: {reviewed}");
        }
        Ok(())
    })
}

#[test]
fn legacy_unknown_coverage_is_explicit_and_mixed_review_reports_each_file_scope() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.runtime.as_ref().context("runtime")?.block_on(async {
        let server = fixture.server.as_ref().context("server")?;
        let unknown_dir = fixture.temporary.as_ref().context("owner")?.path().join("unknown");
        std::fs::create_dir(&unknown_dir)?;
        server.state.registry.put_project(&engram_core::ProjectRecord {
            project_id: "unknown-owned".into(), project_name: "Unknown source coverage".into(),
            directory: unknown_dir.to_string_lossy().into_owned(), project_type: "general".into(),
            created_at_ms: 0, updated_at_ms: 0, reindex_required_since_ms: None,
        })?;
        server.state.registry.set_meta("unknown-owned", "active_generation", "1")?;
        let error = server.generate_migration_plan(Parameters(serde_json::from_value(json!({"project_id":"unknown-owned"}))?)).await.err().context("Unproven storage must not permit analysis")?;
        ensure!(error.message.contains("source snapshot unavailable") && error.message.contains("repair required"), "Unproven source coverage was not explicit: {error}");
        ensure!(server.state.active_jobs.read().await.is_empty(), "Unproven source request started a worker");

        let mixed = fixture.index("mixed-review", &[("package.json", "{}"), ("app.ts", TS), ("Default.aspx", PAGE)]).await?;
        let root = fixture.temporary.as_ref().context("owner")?.path().join("mixed-review");
        let mut diff = String::new();
        for (path, source, addition) in [("app.ts", TS, "// owned change"), ("Default.aspx", PAGE, "<!-- owned change -->")] {
            std::fs::write(root.join(path),format!("{source}{addition}\n"))?;
            let line=source.lines().count();
            let last=source.lines().last().context("last source line")?;
            diff.push_str(&format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -{line},1 +{line},2 @@\n {last}\n+{addition}\n"));
        }
        let output = text(server.pre_commit_review(Parameters(serde_json::from_value(json!({"project_id":mixed,"diff":diff,"output_json":true}))?)).await?)?;
        let review: serde_json::Value = serde_json::from_str(&output)?;
        for name in ["state", "ui_house_style"] {
            let scope = review["gate_status"].as_array().context("outcomes")?.iter().find(|outcome| outcome["name"]==name).context("scoped gate")?;
            ensure!(scope["source_scope"]["applicable_files"]==json!(["Default.aspx"]) && scope["source_scope"]["not_applicable_files"]==json!(["app.ts"]), "Mixed review scope was conflated: {scope}");
            ensure!(scope["status"]["kind"]!="not_applicable", "Supported page gate did not run: {scope}");
        }
        Ok(())
    })
}

#[test]
fn dotnet_service_migration_does_not_require_webforms_markup() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.runtime.as_ref().context("runtime")?.block_on(async {
        let server = fixture.server.as_ref().context("server")?;
        let pid = fixture.index("dotnet", &[
            ("App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"),
            ("App.cs", "public class OwnedService { public static int Value() { return 17; } }"),
        ]).await?;
        let plan = text(server.generate_migration_plan(Parameters(serde_json::from_value(json!({"project_id":pid}))?)).await?)?;
        ensure!(plan.contains("App.cs") && !plan.contains("not_applicable"), "Supported .NET service was treated as a non-.NET project: {plan}");
        not_applicable(&text(server.get_page_context(Parameters(serde_json::from_value(json!({"project_id":pid,"aspx_file":"App.cs"}))?)).await?)?)?;
        Ok(())
    })
}
