#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

async fn call(engram: &Engram, name: &str, value: Value) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    match name {
        "immune" => engram.handle_immune_check(serde_json::from_value(value).unwrap()).await,
        "anti" => engram.handle_anti_pattern_guard(serde_json::from_value(value).unwrap()).await,
        "validate" => engram.handle_validate_generated_code(serde_json::from_value(value).unwrap()).await,
        _ => unreachable!(),
    }
}

fn semantic(text: &str, json_output: bool) -> Value {
    if json_output {
        let mut value: Value = serde_json::from_str(text).unwrap();
        value.as_object_mut().unwrap().remove("input_evidence");
        value
    } else {
        json!(text.split("\n\nInput evidence: ").next().unwrap())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_handlers_bind_exact_file_bytes_and_preserve_inline_verdicts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let code = "Public Class Invoice\n Public Function Total() As Integer\n  Return 1\n End Function\nEnd Class\n";
    std::fs::write(root.join("Invoice.vb"), code).unwrap();
    let config = Config { allowed_roots: vec![root.clone()], data_dir: temp.path().join("data"), embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default() };
    let (state, _) = AppState::new(config).unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(serde_json::from_value(json!({"directory":root,"project_name":"input-fixture","project_type":"general","wait":true})).unwrap())).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    let digest = blake3::hash(code.as_bytes()).to_hex().to_string();
    for populated in [false, true] {
        if populated {
            let runtime = engram_server::services::project_service::ensure_project_runtime(&state, &pid).await.unwrap();
            let content_hash = engram_core::ContentHash::compute(code.as_bytes());
            runtime.search.index_docs(&pid, &[engram_index::IndexDoc {
                generation:0, chunk_id:9001, doc_id:engram_core::DocIdStr::compute("pattern",0,0,&content_hash).0,
                content_hash:content_hash.0, path:engram_core::RelPath::new("pattern"), language:"vb".into(),
                content:code.into(), namespace:"antipattern".into(), author:None,timestamp:None,start_line:0,end_line:0,
            }], &tokio_util::sync::CancellationToken::new()).await.unwrap();
        }
        for name in ["immune", "anti", "validate"] {
            let mut inline = json!({"project_id":pid,"code":code});
            if name=="immune" { inline["file_path"]=json!("Invoice.vb"); inline["use_vector"]=json!(false); }
            if name=="anti" { inline["use_vector"]=json!(false); }
            if name=="validate" { inline["target_file"]=json!("Invoice.vb");inline["language"]=json!("vb");inline["output_json"]=json!(true); }
            let mut file = inline.clone();file.as_object_mut().unwrap().remove("code");
            file["code_file"]=json!("Invoice.vb");file["code_file_blake3"]=json!(digest);
            let legacy=call(&engram,name,inline).await.unwrap();
            let result=call(&engram,name,file.clone()).await.unwrap();
            let text=&result.content[0].as_text().unwrap().text;
            assert_eq!(semantic(text,name=="validate"),semantic(&legacy.content[0].as_text().unwrap().text,name=="validate"));
            let evidence: Value = if name=="validate" { serde_json::from_str::<Value>(text).unwrap()["input_evidence"].clone() }
                else { serde_json::from_str(text.split("\n\nInput evidence: ").nth(1).unwrap()).unwrap() };
            assert_eq!(evidence["raw_blake3"],digest);assert_eq!(evidence["byte_length"],code.len());
            assert_eq!(evidence["project_relative_path"],"Invoice.vb");assert_eq!(evidence["project_id"],pid);
            if !populated && name=="anti" {assert!(text.starts_with("verdict: INSUFFICIENT"));}
            if name!="anti" {
                let mut derived=file.clone();derived.as_object_mut().unwrap().remove(if name=="immune" {"file_path"} else {"target_file"});
                let derived_result=call(&engram,name,derived).await.unwrap();
                assert_eq!(semantic(&derived_result.content[0].as_text().unwrap().text,name=="validate"),semantic(text,name=="validate"));
            }
            file["code_file_blake3"]=json!(blake3::hash(code.trim_end().as_bytes()).to_hex().to_string());
            assert!(call(&engram,name,file).await.is_err(),"{name}: missing LF hash must fail before corpus verdict");
        }
    }
    for name in ["immune","anti","validate"] {
        for value in [json!({"project_id":pid}),json!({"project_id":pid,"code_file":"Invoice.vb"}),
            json!({"project_id":pid,"code_file_blake3":digest}),
            json!({"project_id":pid,"code":"x","code_file":"Invoice.vb","code_file_blake3":digest}),
            json!({"project_id":pid,"code":"","code_file":"Invoice.vb","code_file_blake3":digest})] {
            assert!(call(&engram,name,value).await.is_err());
        }
        let mut mismatch=json!({"project_id":pid,"code_file":"Invoice.vb","code_file_blake3":digest});
        if name!="anti" {
            mismatch[if name=="immune" {"file_path"} else {"target_file"}]=json!("Other.vb");
            assert!(call(&engram,name,mismatch).await.is_err());
        }
    }
}

#[test]
fn optional_inputs_remain_nonnull_strings_and_old_inline_requests_deserialize() {
    macro_rules! check {
        ($ty:ty) => {{
            let old: $ty = serde_json::from_value(json!({"project_id":"p","code":""})).unwrap();
            assert_eq!(old.code.as_deref(), Some(""));
            for key in ["code","code_file","code_file_blake3"] {
                let mut value=json!({"project_id":"p"});value[key]=Value::Null;
                assert!(serde_json::from_value::<$ty>(value).is_err());
            }
            let schema=serde_json::to_value(schemars::schema_for!($ty)).unwrap();
            assert!(!schema["required"].as_array().unwrap().contains(&json!("code")));
            for key in ["code","code_file","code_file_blake3"] {assert_eq!(schema["properties"][key]["type"],"string");}
        }};
    }
    check!(engram_server::models::ImmuneCheckRequest);
    check!(engram_server::models::AntiPatternGuardRequest);
    check!(engram_server::models::ValidateGeneratedCodeRequest);
}
