#![allow(clippy::unwrap_used)]
//! Disconnected source windows share one bounded read, without implying a method.
use engram_core::{Config, ProjectRecord};
use engram_server::{state::AppState, tools::Engram};
use serde_json::{Value, json};
use std::{future::Future, path::PathBuf};

const PID: &str = "source-ranges";

// The runner owns TEMP/TMP. Runtime/DB handles drop before the TempDir on
// success and unwinding; this fixture launches no application jobs or sidecar.
fn owned_case<F, Fut>(run: F)
where
    F: FnOnce(Engram, PathBuf) -> Fut,
    Fut: Future<Output = ()>,
{
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            ..Default::default()
        }).unwrap();
        state.registry.put_project(&ProjectRecord {
            project_id: PID.into(), project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(), project_type: "general".into(),
            created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
        }).unwrap();
        run(Engram::new(state), root).await;
    });
    drop(runtime);
    temporary.close().unwrap();
}

fn request(file: &str, ranges: Value) -> Value {
    json!({"project_id":PID,"file_path":file,"source_ranges":ranges,
        "context_lines":0,"include_caller_bodies":false,"output_json":true})
}

async fn read(engram: &Engram, arguments: Value) -> (String, Value) {
    let result = engram.handle_get_full_method_body(serde_json::from_value(arguments).unwrap()).await.unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text = result.content[0].as_text().unwrap().text.clone();
    assert!(text.len() <= 32 * 1024);
    let value = serde_json::from_str(&text).unwrap();
    (text, value)
}

#[test]
fn discontiguous_windows_skip_unrelated_bodies_and_share_exact_raw_hash() {
    owned_case(|engram, root| async move {
        let mut lines = vec!["unrelated intervening body".to_string(); 200];
        lines[0] = "\u{feff}function first() {".into();
        lines[1] = "".into();
        lines[2] = "}".into();
        lines[99] = "function second() {".into();
        lines[100] = "return \"日本\";".into();
        lines[101] = "}".into();
        lines[198] = "tail".into();
        lines[199] = "".into();
        let source = format!("{}\r\n", lines.join("\r\n"));
        std::fs::write(root.join("route.ts"), &source).unwrap();
        let args = request("route.ts", json!([
            {"line_start":1,"line_end":3},
            {"line_start":100,"line_end":102},
            {"line_start":199,"line_end":200}
        ]));
        let (text, value) = read(&engram, args.clone()).await;
        assert_eq!(value["retrieval_scope"], "current_source_ranges");
        assert_eq!(value["source_verification"], "explicit_ranges_current_file_snapshot");
        assert_eq!(value["source_file_hash_algorithm"], "blake3");
        assert_eq!(value["source_file_hash"], blake3::hash(source.as_bytes()).to_hex().to_string());
        assert_eq!(value["total_returned_lines"], 8);
        assert_eq!(value["total_lines"], 200);
        assert_eq!(value["windows"].as_array().unwrap().len(), 3);
        assert_eq!(value["windows"][0]["source_code"], "\u{feff}function first() {\n\n}");
        assert_eq!(value["windows"][1]["source_code"], "function second() {\nreturn \"日本\";\n}");
        assert_eq!(value["windows"][2]["source_code"], "tail\n");
        assert!(!text.contains("unrelated intervening body"));
        assert!(value.get("line_start").is_none() && value.get("source_code").is_none());
        assert!(value["boundary_guidance"].as_str().unwrap().contains("Gaps are intentionally omitted"));
        assert!(value["freshness"]["interpretation"].as_str().unwrap().contains("no indexed-locator"));
        // Hash the complete file, including omitted gaps, on each new request.
        let changed = source.replacen("unrelated intervening body", "changed intervening body", 1);
        std::fs::write(root.join("route.ts"), &changed).unwrap();
        let (_, fresh) = read(&engram, args.clone()).await;
        assert_eq!(fresh["windows"], value["windows"]);
        assert_ne!(fresh["source_file_hash"], value["source_file_hash"]);
        assert_eq!(fresh["source_file_hash"], blake3::hash(changed.as_bytes()).to_hex().to_string());
        let mut markdown = args;
        markdown["output_json"] = json!(false);
        let result = engram.handle_get_full_method_body(serde_json::from_value(markdown).unwrap()).await.unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains("Lines 1–3") && text.contains("Lines 100–102") && text.contains("Lines 199–200"));
        assert!(!text.contains("changed intervening body"));
    });
}

#[test]
fn source_ranges_reject_conflicts_invalid_order_and_total_line_overflow() {
    owned_case(|engram, root| async move {
        std::fs::write(root.join("route.ts"), "line\n".repeat(200)).unwrap();
        let base = request("route.ts", json!([{"line_start":1,"line_end":1}]));
        let mut cases = vec![
            (request("route.ts", json!([])), "one to three"),
            (request("route.ts", json!([{"line_start":0,"line_end":1}])), "positive inclusive"),
            (request("route.ts", json!([{"line_start":3,"line_end":2}])), "positive inclusive"),
            (request("route.ts", json!([{"line_start":2,"line_end":3},{"line_start":1,"line_end":1}])), "ordered and nonoverlapping"),
            (request("route.ts", json!([{"line_start":1,"line_end":3},{"line_start":3,"line_end":4}])), "ordered and nonoverlapping"),
            (request("route.ts", json!([{"line_start":1,"line_end":1},{"line_start":2,"line_end":2},{"line_start":3,"line_end":3},{"line_start":4,"line_end":4}])), "one to three"),
            (request("route.ts", json!([{"line_start":1,"line_end":60},{"line_start":100,"line_end":160}])), "120 total-line"),
            (request("route.ts", json!([{"line_start":1,"line_end":1},{"line_start":201,"line_end":201}])), "no partial windows"),
        ];
        for (key, value) in [("fqn", json!("Route.run")), ("line_start", json!(1)), ("line_end", json!(1))] {
            let mut args = base.clone(); args[key] = value;
            cases.push((args, "mutually exclusive"));
        }
        for (key, value) in [("context_lines", json!(1)), ("include_caller_bodies", json!(true))] {
            let mut args = base.clone(); args[key] = value;
            cases.push((args, "requires context_lines=0"));
        }
        let mut default_context = base.clone(); default_context.as_object_mut().unwrap().remove("context_lines");
        cases.push((default_context, "requires context_lines=0"));
        let mut no_file = base; no_file.as_object_mut().unwrap().remove("file_path");
        cases.push((no_file, "requires file_path"));
        for (args, message) in cases {
            let result = engram.handle_get_full_method_body(serde_json::from_value(args.clone()).unwrap()).await;
            let error = result.expect_err(&format!("Invalid ranges unexpectedly succeeded: {args}"));
            assert!(error.message.contains(message), "{args}: {error:?}");
        }
        // Exactly120 requested lines is valid, including separate adjacent ranges.
        let (_, value) = read(&engram, request("route.ts", json!([
            {"line_start":1,"line_end":60},{"line_start":61,"line_end":120}
        ]))).await;
        assert_eq!(value["total_returned_lines"], 120);
        assert_eq!(value["windows"].as_array().unwrap().len(), 2);
    });
}

#[test]
fn source_ranges_fail_atomically_for_escape_large_files_and_escaped_output() {
    owned_case(|engram, root| async move {
        std::fs::write(root.parent().unwrap().join("outside.ts"), "private outside text").unwrap();
        std::fs::write(root.join("huge.ts"), vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
        std::fs::write(root.join("long.ts"), format!("small\n{}\n", "x".repeat(33 * 1024))).unwrap();
        // Raw text fits32KiB, but JSON escaping expands it beyond the response cap.
        std::fs::write(root.join("escaped.ts"), format!("small\n{}\n", "\"".repeat(17000))).unwrap();
        for (name, ranges, expected) in [
            ("../outside.ts", json!([{"line_start":1,"line_end":1}]), "Path validation"),
            ("huge.ts", json!([{"line_start":1,"line_end":1}]), "4MiB"),
            ("long.ts", json!([{"line_start":1,"line_end":1},{"line_start":2,"line_end":2}]), "no partial windows"),
            ("escaped.ts", json!([{"line_start":1,"line_end":1},{"line_start":2,"line_end":2}]), "rendered response exceeds")
        ] {
            let error = engram.handle_get_full_method_body(serde_json::from_value(request(name, ranges)).unwrap()).await.expect_err(name);
            assert!(error.message.contains(expected), "{name}: {error:?}");
            assert!(!error.message.contains("private outside text"));
        }
    });
}

#[test]
fn omitted_source_ranges_preserve_existing_single_range_and_excerpt_modes() {
    owned_case(|engram, root| async move {
        std::fs::write(root.join("route.ts"), "first\n\nthird\n").unwrap();
        let (_, explicit) = read(&engram, json!({"project_id":PID,"file_path":"route.ts",
            "line_start":2,"line_end":3,"context_lines":0,"output_json":true})).await;
        assert_eq!(explicit["retrieval_scope"], "explicit_source_range");
        assert_eq!(explicit["source_code"], "\nthird");
        let (_, excerpt) = read(&engram, json!({"project_id":PID,"file_path":"route.ts",
            "source_ranges":null,"context_lines":0,"output_json":true})).await;
        assert_eq!(excerpt["retrieval_scope"], "current_file_excerpt");
        assert_eq!(excerpt["source_code"], "first\n\nthird");
    });
}
