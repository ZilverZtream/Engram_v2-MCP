#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

const SOURCE: &str = "class Flow {\n public int Run(int first, int second) {\n  int alias = first;\n  int a = Identity(alias);\n  int b = Identity(second);\n  alias = 42;\n  return a;\n }\n public int Identity(int value) { return value; }\n public string Label(int input) { return nameof(input); }\n}";

fn text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|c| c.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn return_inputs(trace: &Value) -> Vec<String> {
    let points = trace["nodes"].as_array().unwrap();
    let returned = points
        .iter()
        .find(|p| p["context"] == "c0" && p["role"] == "return")
        .expect("source return point");
    let mut seen = std::collections::BTreeSet::from([returned["id"].as_str().unwrap().to_string()]);
    loop {
        let before = seen.len();
        for edge in trace["edges"].as_array().unwrap() {
            if seen.contains(edge["to"].as_str().unwrap()) {
                seen.insert(edge["from"].as_str().unwrap().into());
            }
        }
        if seen.len() == before {
            break;
        }
    }
    points
        .iter()
        .filter(|p| {
            p["context"] == "c0"
                && p["role"] == "parameter"
                && seen.contains(p["id"].as_str().unwrap())
        })
        .map(|p| p["name"].as_str().unwrap().into())
        .collect()
}

#[test]
fn indexed_csharp_public_trace_keeps_invocations_distinct_and_refreshes_changed_source() {
    // Runtime and database owners unwind before the owned directory, including
    // failed assertions during a public indexing job.
    let temp = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let root=temp.path().join("repo");std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("Flow.cs"),SOURCE).unwrap();
        let repo=git2::Repository::init(&root).unwrap();
        let tree_id=repo.index().unwrap().write_tree().unwrap();
        let tree=repo.find_tree(tree_id).unwrap();
        let author=git2::Signature::now("Fixture","fixture@example.invalid").unwrap();
        repo.commit(Some("HEAD"),&author,&author,"fixture",&tree,&[]).unwrap();
        let (state,_)=AppState::new(Config {allowed_roots:vec![root.clone()],data_dir:temp.path().join("data"),embedding_backend:"fts_only".into(),..Default::default()}).unwrap();
        let engram=Engram::new(state.clone());
        let indexed=engram.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"value-lineage","project_type":"general","wait":true})).unwrap()).await.unwrap();
        assert_ne!(indexed.is_error,Some(true),"{}",text(&indexed));
        let pid=state.registry.list_projects().unwrap()[0].project_id.clone();
        let request=|entry:&str,output_json| serde_json::from_value(json!({"project_id":pid,"file_path":"Flow.cs","entry_point":entry,"output_json":output_json})).unwrap();
        let result=engram.handle_trace_data_flow(request("Run",true)).await.unwrap();
        let trace:Value=serde_json::from_str(&text(&result)).unwrap();
        let lineage=&trace["value_lineage"];
        assert_eq!(lineage["contexts"].as_array().unwrap().len(),3,"{trace}");
        assert_eq!(return_inputs(lineage),vec!["first"],"{trace}");
        assert!(lineage["nodes"].as_array().unwrap().iter().any(|p|p["line"]==4 && p["role"]=="call_result"));
        let readable=text(&engram.handle_trace_data_flow(request("Run",false)).await.unwrap());
        assert!(readable.contains("Value lineage") && readable.contains("Flow.cs:4:") && readable.contains("argument_parameter"),"{readable}");
        let label:Value=serde_json::from_str(&text(&engram.handle_trace_data_flow(request("Label",true)).await.unwrap())).unwrap();
        assert!(return_inputs(&label["value_lineage"]).is_empty(),"nameof is a constant, not an input read: {label}");
        std::fs::write(root.join("Flow.cs"),SOURCE.replace("return a;","return b;")).unwrap();
        let stale:Value=serde_json::from_str(&text(&engram.handle_trace_data_flow(request("Run",true)).await.unwrap())).unwrap();
        assert_eq!(stale["value_lineage"]["status"],"unavailable");
        let updated=engram.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1})).unwrap()).await.unwrap();
        assert_ne!(updated.is_error,Some(true),"{}",text(&updated));
        let refreshed:Value=serde_json::from_str(&text(&engram.handle_trace_data_flow(request("Run",true)).await.unwrap())).unwrap();
        assert_eq!(return_inputs(&refreshed["value_lineage"]),vec!["second"],"{refreshed}");
    });
    drop(runtime);
    temp.close().unwrap();
}

struct ParserOwner;
impl Drop for ParserOwner {
    fn drop(&mut self) {
        let _ = engram_index::vb_extractor::shutdown_idle_parser();
    }
}

#[test]
#[ignore = "requires an explicitly selected built VB companion"]
fn indexed_vb_public_trace_uses_admitted_named_arguments_and_reports_unsupported_effects() {
    assert!(std::env::var_os("ENGRAM_VB_SIDECAR_PATH").is_some());
    let temp = tempfile::tempdir().unwrap();
    let parser = ParserOwner;
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let root=temp.path().join("repo"); std::fs::create_dir(&root).unwrap();
        let source = "Public Class Flow\n Public Function Run(first As Integer, second As Integer) As Integer\n  Dim a = Pick(second:=second, first:=first)\n  Dim b = Pick(first:=second, second:=first)\n  Return a\n End Function\n Private Function Pick(first As Integer, second As Integer) As Integer\n  Return first\n End Function\n Public Function Unsupported(input As Integer) As Integer\n  While input > 0\n   input -= 1\n  End While\n  Return input\n End Function\n Public Function Label(input As Integer) As String\n  Return NameOf(input)\n End Function\n Public Function Persistent(input As Integer) As Integer\n  Static prior As Integer = input\n  Return prior\n End Function\nEnd Class\n";
        std::fs::write(root.join("Flow.vb"),source).unwrap();
        std::fs::write(root.join("web.config"),r#"<configuration><system.web><compilation targetFramework="4.8" defaultLanguage="vb"><assemblies><add assembly="System.Data.Linq, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089"/></assemblies></compilation></system.web><system.codedom><compilers><compiler language="vb;vbs;visualbasic;vbscript" extension=".vb" type="Microsoft.CodeDom.Providers.DotNetCompilerPlatform.VBCodeProvider, Microsoft.CodeDom.Providers.DotNetCompilerPlatform, Version=4.1.0.0, Culture=neutral, PublicKeyToken=31bf3856ad364e35" compilerOptions="/optioninfer+"><providerOption name="UseAspNetSettings" value="true"/></compiler></compilers></system.codedom></configuration>"#).unwrap();
        let (state,_) = AppState::new(Config {allowed_roots:vec![root.clone()],data_dir:temp.path().join("data"),embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()}).unwrap();
        let app=Engram::new(state);
        let indexed=app.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"VB lineage","project_type":"general","wait":true})).unwrap()).await.unwrap();
        assert_ne!(indexed.is_error,Some(true),"{}",text(&indexed));
        let pid=app.state.registry.list_projects().unwrap()[0].project_id.clone();
        for entry in ["Run","Unsupported","Label","Persistent"] {
            let result=app.handle_trace_data_flow(serde_json::from_value(json!({"project_id":pid,"file_path":"Flow.vb","entry_point":entry,"output_json":true})).unwrap()).await.unwrap();
            let trace:Value=serde_json::from_str(&text(&result)).unwrap();let lineage=&trace["value_lineage"];
            if entry=="Run" {
                assert_eq!(lineage["contexts"].as_array().unwrap().len(),3,"{trace}");
                assert_eq!(return_inputs(lineage),vec!["first"],"{trace}");
                assert!(lineage["edges"].as_array().unwrap().iter().any(|edge|edge["binding"]=="admitted_compiler_selected_source_call"),"{trace}");
            } else {
                assert!(return_inputs(lineage).is_empty(),"Unsupported effects/compile-time constants must not invent input lineage: {trace}");
                if entry!="Label" {assert_eq!(lineage["status"],"partial","{trace}");}
            }
        }
        let jobs=app.state.active_jobs.write().await.drain().map(|(_,job)|job).collect::<Vec<_>>();
        for job in jobs {job.await.unwrap();}
    });
    drop(runtime);
    drop(parser);
    temp.close().unwrap();
}
