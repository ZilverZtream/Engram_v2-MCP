//! Request entry navigation composes existing compiler-backed value evidence.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
const VB: &str = r###"Public Class Orders
 Protected Sub Page_Load(sender As Object, e As System.EventArgs)
  Dim first = Normalize("first")
  Dim second = Normalize("second")
  Dim result = first & second
 End Sub
 Private Function Normalize(value As String) As String
  Return value.Trim()
 End Function
End Class
"###;
const WEB_CONFIG: &str = r###"<configuration>
<system.web><compilation targetFramework="4.8" defaultLanguage="vb" debug="true"><assemblies>
<add assembly="System.Data.Linq, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089"/>
</assemblies></compilation><pages><namespaces><clear/>
<add namespace="System"/><add namespace="System.Collections.Generic"/><add namespace="System.Linq"/>
</namespaces></pages></system.web>
<system.codedom><compilers><compiler language="vb;vbs;visualbasic;vbscript" extension=".vb"
type="Microsoft.CodeDom.Providers.DotNetCompilerPlatform.VBCodeProvider, Microsoft.CodeDom.Providers.DotNetCompilerPlatform, Version=4.1.0.0, Culture=neutral, PublicKeyToken=31bf3856ad364e35"
compilerOptions="/optioninfer+"><providerOption name="UseAspNetSettings" value="true"/>
</compiler></compilers></system.codedom></configuration>"###;
struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_entries_preserve_helper_value_contexts_and_original_recovery() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let root = owned.path().join("source");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("Orders.aspx.vb"), VB)?;
    std::fs::write(root.join("web.config"), WEB_CONFIG)?;
    std::fs::write(
        root.join("Orders.aspx"),
        "<%@ Page Language=\"VB\" AutoEventWireup=\"true\" CodeFile=\"Orders.aspx.vb\" Inherits=\"Orders\" %>\n",
    )?;
    std::fs::write(
        root.join("AGENTS.md"),
        "Preserve separate helper inputs; static lineage is not runtime execution.\n",
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: true,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        owned,
    };
    let indexed=fixture.server.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned request values","project_type":"dotnet_webforms_vb","wait":true}))?).await?;
    let project = text(&indexed)
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_owned();
    let direct=fixture.server.handle_trace_data_flow(serde_json::from_value(json!({"project_id":project,"file_path":"Orders.aspx.vb","entry_point":"Page_Load","output_json":true}))?).await?;
    let direct: Value = serde_json::from_str(text(&direct))?;
    let args = json!({"project_id":project,"entry_point":"/Orders.aspx","include_db":false,"include_triggers":false,"max_tokens":100000});
    let original = fixture
        .server
        .handle_trace_user_request(serde_json::from_value(args.clone())?)
        .await?;
    let original_text = text(&original).to_owned();
    let report: Value = serde_json::from_str(&original_text)?;
    let lineage = &report["pages"][0]["value_lineage"][0];
    ensure!(lineage["flow"] == direct["value_lineage"]);
    ensure!(lineage["source_verification"]["status"] == "returned_points_selected_source_checked");
    ensure!(
        lineage["source_verification"]["files"][0]["source_blake3"]
            == blake3::hash(VB.as_bytes()).to_hex().to_string()
    );
    let contexts = lineage["flow"]["contexts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["owner_id"].as_str().unwrap_or("").contains("Normalize"))
        .collect::<Vec<_>>();
    ensure!(contexts.len() == 2 && contexts[0]["call_site"] != contexts[1]["call_site"]);
    ensure!(
        lineage["flow"]["runtime_value_proven"] == false
            && report["coverage"]["runtime_execution_proven"] == false
    );
    let bounded = fixture
        .server
        .handle_trace_user_request(serde_json::from_value({
            let mut bounded_args = args.clone();
            bounded_args["max_tokens"] = json!(3072);
            bounded_args
        })?)
        .await?;
    let bounded_text = text(&bounded);
    ensure!(bounded_text.len() <= 12288);
    let manifest: Value = serde_json::from_str(bounded_text)?;
    ensure!(manifest["response_delivery"]["status"] == "DEFERRED_TRACE_EVIDENCE");
    ensure!(manifest["pages"][0]["value_lineage"][0]["flow"]["content_deferred"] == true);
    ensure!(
        manifest["pages"][0]["value_lineage"][0]["flow"]["status"] == lineage["flow"]["status"]
    );
    std::fs::write(
        root.join("Orders.aspx.vb"),
        VB.replace("\"first\"", "\"changed\""),
    )?;
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut recovered = String::new();
    for _ in 0..64 {
        let result = fixture
            .server
            .handle_trace_user_request(serde_json::from_value(action["arguments"].clone())?)
            .await?;
        ensure!(text(&result).len() <= 12288);
        let page: Value = serde_json::from_str(text(&result))?;
        ensure!(page["start_byte"] == recovered.len());
        recovered.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        action = page["next_call"].clone();
    }
    ensure!(recovered == original_text);
    ensure!(
        blake3::hash(recovered.as_bytes()).to_hex().to_string()
            == manifest["response_delivery"]["response_id"]
    );
    let changed = fixture
        .server
        .handle_trace_user_request(serde_json::from_value(args)?)
        .await?;
    let changed: Value = serde_json::from_str(text(&changed))?;
    ensure!(
        changed["pages"][0]["value_lineage"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["flow"]["nodes"].as_array().is_none_or(Vec::is_empty))
    );
    let Fixture { server, owned } = fixture;
    drop(server);
    owned.close()?;
    Ok(())
}
