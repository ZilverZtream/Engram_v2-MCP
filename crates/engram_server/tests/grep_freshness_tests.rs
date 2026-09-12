#![allow(clippy::unwrap_used)]
//! `grep_project`'s freshness guard has to actually detect staleness.
//!
//! The parameter defaults to `"strict"` and its whole job is to stop an
//! agent reading grep output as current when the file on disk has moved on.
//! It worked by comparing indexed fingerprints from the DocStore against
//! disk — but nothing has ever written a fingerprint to the DocStore, so it
//! compared against an empty set and could never report anything. Every
//! `docs.redb` on disk is redb's empty size to the byte.
//!
//! The fingerprints do exist: ingest records (mtime, size, file_hash) into
//! the graph's file nodes, which is what the incremental change scan already
//! trusts to decide what to re-index. The guard now reads the same source.
//!
//! Note the indexer stores mtime in SECONDS, so these tests move mtime by
//! whole seconds — a sub-second edit is genuinely below the recorded
//! resolution and is caught by the size comparison instead.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use std::path::Path;
use tempfile::tempdir;

#[tokio::test]
async fn markdown_budget_recovery_delivers_matches_and_usable_chunk_arguments() {
    let source = format!(
        "pub fn markers() {{\n{}}}\n",
        (0..40)
            .map(|i| format!("    // recovery_marker_{i:02} {}\n", "x".repeat(120)))
            .collect::<String>()
    );
    let (_tmp, engram, pid, _root) = setup_with_source(source.as_bytes()).await;
    let request = grep_request(&pid, "recovery_marker", "strict");
    let markdown = grep(&engram, request.clone()).await;
    assert!(
        markdown.contains("Markdown output budget reached"),
        "{markdown}"
    );
    assert!(markdown.contains("output_json: true"), "{markdown}");
    assert!(!markdown.contains("raise `max_results` for an exhaustive list"));
    let shown = markdown
        .lines()
        .filter(|line| line.starts_with("**src/"))
        .count();
    assert!(shown > 0 && shown < 40);
    for line in markdown.lines().filter_map(|line| {
        line.strip_prefix("get_chunk arguments: `")
            .and_then(|s| s.strip_suffix('`'))
    }) {
        let arguments: engram_server::GetChunkRequest = serde_json::from_str(line).unwrap();
        let chunk = engram.handle_get_chunk(arguments).await.unwrap();
        assert!(
            chunk.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("recovery_marker")
        );
    }
    assert!(markdown.contains("get_chunk arguments: `"));
    let mut recovery = request;
    recovery.output_json = true;
    let json: serde_json::Value = serde_json::from_str(&grep(&engram, recovery).await).unwrap();
    assert_eq!(json["result_limit"], 200);
    assert_eq!(json["result_limit_reached"], false);
    let matches = json["matches"].as_array().unwrap();
    let actual: std::collections::BTreeSet<_> = matches
        .iter()
        .map(|hit| hit["line"].as_u64().unwrap())
        .collect();
    assert_eq!(actual, (2..42).collect());
    for hit in matches {
        assert_eq!(
            hit["line_text"],
            source
                .lines()
                .nth(hit["line"].as_u64().unwrap() as usize - 1)
                .unwrap()
        );
        let args = serde_json::from_value(hit["recovery"]["arguments"].clone()).unwrap();
        let chunk = engram.handle_get_chunk(args).await.unwrap();
        assert!(
            chunk.content[0]
                .as_text()
                .unwrap()
                .text
                .contains(hit["line_text"].as_str().unwrap())
        );
    }
    let mut bounded = grep_request(&pid, "recovery_marker", "strict");
    bounded.max_results = 3;
    let capped_markdown = grep(&engram, bounded.clone()).await;
    assert!(capped_markdown.contains("Result limit reached"));
    bounded.output_json = true;
    let capped: serde_json::Value = serde_json::from_str(&grep(&engram, bounded).await).unwrap();
    assert_eq!(capped["matches"].as_array().unwrap().len(), 3);
    assert_eq!(capped["result_limit"], 3);
    assert_eq!(capped["result_limit_reached"], true);
    assert!(
        capped["coverage"]
            .as_str()
            .unwrap()
            .contains("not_exhaustive")
    );
}

#[tokio::test]
async fn grep_results_round_trip_to_get_chunk_across_search_tiers() {
    let (_tmp, engram, pid, root) = setup().await;
    for (pattern, regex, tier) in [
        ("tracker_marker", false, "term_index"),
        ("tracker_marker.*", true, "term_narrowed"),
        (".+", true, "full_scan"),
    ] {
        let mut req = grep_request(&pid, pattern, "off");
        req.regex = regex;
        req.output_json = true;
        let result: serde_json::Value = serde_json::from_str(&grep(&engram, req).await).unwrap();
        assert_eq!(result["tier_used"], tier);
        let found = &result["matches"][0];
        let doc_id = found["doc_id"].as_str().unwrap();
        assert!(!doc_id.is_empty());
        assert_eq!(found["source"], "index");
        assert_eq!(found["recovery"]["tool"], "get_chunk");
        assert_eq!(found["recovery"]["arguments"]["doc_id"], doc_id);
        assert_eq!(found["recovery"]["arguments"]["namespace"], "memory");
        assert_ne!(found["chunk_id"].as_u64().unwrap(), 0);
        let chunk = engram
            .handle_get_chunk(
                serde_json::from_value(serde_json::json!({
                    "project_id": pid, "doc_id": doc_id
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            chunk.content[0]
                .as_text()
                .unwrap()
                .text
                .contains(found["file_path"].as_str().unwrap())
        );
    }
    let markdown = grep(&engram, grep_request(&pid, "tracker_marker", "off")).await;
    assert!(markdown.contains("doc_id: `"));

    std::fs::write(root.join("src/new.rs"), "pub fn disk_only_probe() {}\n").unwrap();
    let mut req = grep_request(&pid, "disk_only_probe", "warn");
    req.output_json = true;
    let result: serde_json::Value = serde_json::from_str(&grep(&engram, req).await).unwrap();
    assert!(!result["matches"].as_array().unwrap().is_empty());
    assert!(
        result["matches"][0].get("doc_id").is_none(),
        "disk-only hits must not invent indexed identifiers"
    );
    let hit = &result["matches"][0];
    assert_eq!(hit["source"], "working_tree");
    assert_eq!(hit["recovery"]["action"], "read_file");
    let file = std::path::PathBuf::from(hit["recovery"]["directory"].as_str().unwrap())
        .join(hit["recovery"]["file_path"].as_str().unwrap());
    let content = std::fs::read_to_string(file).unwrap();
    assert!(
        content
            .lines()
            .nth(hit["recovery"]["line"].as_u64().unwrap() as usize - 1)
            .unwrap()
            .contains("disk_only_probe")
    );
    assert!(hit["recovery"].get("tool").is_none());
}

fn grep_request(pid: &str, pattern: &str, freshness: &str) -> engram_server::GrepProjectRequest {
    engram_server::GrepProjectRequest {
        project_id: pid.to_string(),
        pattern: pattern.to_string(),
        regex: false,
        case_sensitive: None,
        multiline: false,
        path_prefix: None,
        language: None,
        context_before: 0,
        context_after: 0,
        max_results: 200,
        freshness: freshness.to_string(),
        namespace: "memory".into(),
        output_json: false,
    }
}

/// Push a file's mtime back so the recorded second-granularity stamp and the
/// on-disk one genuinely differ, regardless of how fast the test runs.
fn age_file(path: &Path, secs: u64) {
    let meta = std::fs::metadata(path).unwrap();
    let modified = meta.modified().unwrap();
    let older = modified - std::time::Duration::from_secs(secs);
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(older).unwrap();
}

async fn setup() -> (
    tempfile::TempDir,
    engram_server::Engram,
    String,
    std::path::PathBuf,
) {
    setup_with_source(b"pub fn submit_order(id: u64) -> bool {\n    tracker_marker(id)\n}\n").await
}

async fn setup_with_source(
    source: &[u8],
) -> (
    tempfile::TempDir,
    engram_server::Engram,
    String,
    std::path::PathBuf,
) {
    let tmp = tempdir().unwrap();
    // data_dir lives OUTSIDE the indexed tree. Nested inside it, engram
    // indexes its own Tantivy segment files, which keep being rewritten
    // after the index run and so read as permanently stale.
    let root = tmp.path().join("repo");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/orders.rs"), source).unwrap();
    std::fs::write(
        root.join("src/stable.rs"),
        "pub fn untouched_helper() -> u8 {\n    7\n}\n",
    )
    .unwrap();

    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: data_dir.clone(),
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "FreshTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid, root)
}

async fn grep(engram: &engram_server::Engram, req: engram_server::GrepProjectRequest) -> String {
    let res = engram
        .grep_project(Parameters(req))
        .await
        .expect("grep_project must succeed");
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A freshly indexed project must report no staleness — the guard must not
/// become a permanent false alarm.
#[tokio::test]
async fn freshly_indexed_project_reports_no_stale_files() {
    let (_tmp, engram, pid, _root) = setup().await;
    let out = grep(&engram, grep_request(&pid, "tracker_marker", "strict")).await;
    assert!(
        !out.contains("match the indexed fingerprint"),
        "a just-indexed project must be clean; got:\n{out}"
    );
}

/// The finding. Editing a tracked file behind the index must be reported.
#[tokio::test]
async fn editing_a_file_behind_the_index_is_reported_as_stale() {
    let (_tmp, engram, pid, root) = setup().await;
    let edited = root.join("src/orders.rs");

    std::fs::write(
        &edited,
        "pub fn submit_order(id: u64) -> bool {\n    tracker_marker(id) && audit(id)\n}\n\
         pub fn audit(_id: u64) -> bool { true }\n",
    )
    .unwrap();
    age_file(&edited, 5);

    let out = grep(&engram, grep_request(&pid, "tracker_marker", "strict")).await;

    assert!(
        out.contains("match the indexed fingerprint"),
        "an edit behind the index must be surfaced, or the guard is decorative; got:\n{out}"
    );
    // Check the WARNING names it, not merely that the string appears
    // somewhere — orders.rs is also a match line here, which would pass
    // vacuously.
    assert!(
        out.contains("> - `src/orders.rs`"),
        "the warning must name the file that drifted; got:\n{out}"
    );
    assert!(
        !out.contains("> - `src/stable.rs`"),
        "an untouched file must not be reported as stale; got:\n{out}"
    );
}

/// A deleted file is stale too — its indexed content can no longer be
/// verified against anything.
#[tokio::test]
async fn deleting_a_tracked_file_is_reported_as_stale() {
    let (_tmp, engram, pid, root) = setup().await;
    std::fs::remove_file(root.join("src/orders.rs")).unwrap();

    let out = grep(&engram, grep_request(&pid, "untouched_helper", "strict")).await;
    assert!(
        out.contains("match the indexed fingerprint") && out.contains("orders.rs"),
        "a deleted tracked file must be surfaced; got:\n{out}"
    );
}

/// `freshness=off` must skip the check — it exists for callers who know the
/// index is being rebuilt and do not want the stat sweep.
#[tokio::test]
async fn freshness_off_skips_the_check() {
    let (_tmp, engram, pid, root) = setup().await;
    let edited = root.join("src/orders.rs");
    std::fs::write(&edited, "pub fn submit_order() {}\n").unwrap();
    age_file(&edited, 5);

    let out = grep(&engram, grep_request(&pid, "submit_order", "off")).await;
    assert!(
        !out.contains("match the indexed fingerprint"),
        "freshness=off must not run the check; got:\n{out}"
    );
}

/// A check that has nothing to compare against must SAY so. An empty
/// fingerprint set produces an empty stale list, which is indistinguishable
/// from a clean bill of health — that silence is exactly what let this guard
/// pass as working while it validated nothing.
#[tokio::test]
async fn a_check_with_no_fingerprints_says_it_could_not_verify() {
    let (_tmp, engram, pid, _root) = setup().await;

    // Drop the graph, keeping the search index: the fingerprints are gone
    // but the corpus is still searchable.
    engram
        .state
        .graph
        .delete_project_data(&pid)
        .expect("purge graph");

    let out = grep(&engram, grep_request(&pid, "tracker_marker", "strict")).await;
    assert!(
        out.contains("could NOT be verified"),
        "with no fingerprints the guard must admit it proved nothing; got:\n{out}"
    );
    assert!(
        !out.contains("match the indexed fingerprint"),
        "it must not also claim files drifted; got:\n{out}"
    );
}

/// The full-scan tier must actually scan. It is reached by patterns the
/// trigram index cannot serve, and it used to read the same unwritten store
/// as the freshness guard — so it scanned zero chunks and reported zero
/// matches for text plainly present in the corpus.
#[tokio::test]
async fn full_scan_tier_finds_matches_the_term_index_cannot_serve() {
    let (_tmp, engram, pid, _root) = setup().await;

    // A two-character literal is below the trigram minimum, so this cannot
    // be served by the term index and falls through to the full scan.
    let out = grep(&engram, grep_request(&pid, "u8", "strict")).await;

    assert!(
        out.contains("full_scan"),
        "precondition: a 2-char literal must reach the full-scan tier; got:\n{out}"
    );
    assert!(
        out.contains("stable.rs"),
        "the full scan must find `u8` in stable.rs; got:\n{out}"
    );
    assert!(
        !out.contains("UNCONFIRMED coverage"),
        "a scan that covered the corpus must not warn about zero coverage; got:\n{out}"
    );
}

/// The J5 fix: grep_project must find a string in a file added since the last
/// index — by scanning the working tree, not just the index — instead of
/// returning "no matches" and letting the agent conclude the code is absent.
#[tokio::test]
async fn grep_finds_a_new_unindexed_file_via_disk_fallback() {
    let (_tmp, engram, pid, root) = setup().await;
    // A brand-new file the index has never seen.
    std::fs::write(
        root.join("src/newly_added.rs"),
        "pub fn frobnicate_widget() -> u8 {\n    42\n}\n",
    )
    .unwrap();
    let out = grep(&engram, grep_request(&pid, "frobnicate_widget", "warn")).await;
    assert!(
        out.contains("newly_added.rs"),
        "disk fallback must find the string in the new file:\n{out}"
    );
    assert!(
        out.to_lowercase().contains("not in the index"),
        "output must note the match came from the working tree:\n{out}"
    );
}

#[tokio::test]
async fn working_tree_overlay_merges_new_matches_and_removes_obsolete_hits() {
    let (_tmp, engram, pid, root) = setup().await;
    std::fs::write(
        root.join("src/new.rs"),
        "// before\npub fn tracker_marker_new() {}\n// after\n",
    )
    .unwrap();
    let mut req = grep_request(&pid, "tracker_marker", "strict");
    req.output_json = true;
    req.context_before = 1;
    req.context_after = 1;
    let result: serde_json::Value =
        serde_json::from_str(&grep(&engram, req.clone()).await).unwrap();
    let hits = result["matches"].as_array().unwrap();
    assert!(hits.iter().any(|m| m["file_path"] == "src/orders.rs"));
    let new = hits
        .iter()
        .find(|m| m["file_path"] == "src/new.rs")
        .unwrap();
    assert_eq!(new["context_before"][0], "// before");
    assert_eq!(new["context_after"][0], "// after");
    assert!(new.get("doc_id").is_none());
    // Same size AND timestamp: neither the stat guard nor its cache can see it.
    let path = root.join("src/orders.rs");
    let timestamp = std::fs::metadata(&path).unwrap().modified().unwrap();
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("tracker_marker", "removed_marker");
    std::fs::write(&path, text).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(timestamp)
        .unwrap();
    let result: serde_json::Value =
        serde_json::from_str(&grep(&engram, req.clone()).await).unwrap();
    assert!(
        result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["file_path"] != "src/orders.rs")
    );
    req.max_results = 1;
    let result: serde_json::Value = serde_json::from_str(&grep(&engram, req).await).unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn working_tree_overlay_honors_regex_multiline_scope_and_namespace() {
    let (_tmp, engram, pid, root) = setup().await;
    std::fs::write(root.join("src/new.rs"), "// disk_begin\n// disk_end\n").unwrap();
    let mut req = grep_request(&pid, "disk_begin.*disk_end", "strict");
    req.regex = true;
    req.multiline = true;
    req.path_prefix = Some("SRC/".into());
    req.language = Some("rust".into());
    req.output_json = true;
    let result: serde_json::Value =
        serde_json::from_str(&grep(&engram, req.clone()).await).unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    req.language = Some("python".into());
    let result: serde_json::Value =
        serde_json::from_str(&grep(&engram, req.clone()).await).unwrap();
    assert!(result["matches"].as_array().unwrap().is_empty());
    req.language = None;
    req.namespace = "business_logic".into();
    let result: serde_json::Value = serde_json::from_str(&grep(&engram, req).await).unwrap();
    assert!(result["matches"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_chunk_withholds_source_after_same_size_edit() {
    let (_tmp, engram, pid, root) = setup().await;
    let mut req = grep_request(&pid, "tracker_marker", "off");
    req.output_json = true;
    let result: serde_json::Value = serde_json::from_str(&grep(&engram, req).await).unwrap();
    let doc_id = result["matches"][0]["doc_id"].as_str().unwrap();
    let request = || {
        serde_json::from_value(serde_json::json!({"project_id": pid, "doc_id": doc_id})).unwrap()
    };
    assert!(engram.handle_get_chunk(request()).await.is_ok());
    let path = root.join("src/orders.rs");
    let content = std::fs::read_to_string(&path)
        .unwrap()
        .replace("tracker_marker", "removed_marker");
    std::fs::write(path, content).unwrap();
    let err = engram.handle_get_chunk(request()).await.unwrap_err();
    assert!(
        err.message.contains("Stale source chunk withheld"),
        "{err:?}"
    );
}

#[tokio::test]
async fn method_edit_context_rejects_shifted_indexed_spans() {
    let (_tmp, engram, pid, root) = setup().await;
    let request = || {
        serde_json::from_value(serde_json::json!({
            "project_id": pid, "file_path": "src/orders.rs", "method_name": "submit_order",
            "include_business_logic": false
        }))
        .unwrap()
    };
    assert!(
        engram
            .handle_get_method_edit_context(request())
            .await
            .is_ok()
    );
    let path = root.join("src/orders.rs");
    let content = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path, format!("// shifted\n{content}")).unwrap();
    let err = engram
        .handle_get_method_edit_context(request())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("Stale method spans withheld"),
        "{err:?}"
    );
}

#[tokio::test]
async fn disk_only_result_limit_reports_omitted_matches() {
    let (_tmp, engram, pid, root) = setup().await;
    std::fs::write(
        root.join("src/new.rs"),
        "// disk_only_limit\n// disk_only_limit\n",
    )
    .unwrap();
    let mut req = grep_request(&pid, "disk_only_limit", "strict");
    req.max_results = 1;
    req.output_json = true;
    let result: serde_json::Value = serde_json::from_str(&grep(&engram, req).await).unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert!(
        result["index_stale_warning"]
            .as_str()
            .unwrap()
            .contains("Results capped: true")
    );
}

#[tokio::test]
async fn multiline_zero_width_regex_handles_empty_new_files() {
    let (_tmp, engram, pid, root) = setup().await;
    std::fs::write(root.join("src/empty.rs"), "").unwrap();
    std::fs::write(root.join("src/new.rs"), "// marker\n").unwrap();
    let mut req = grep_request(&pid, "$", "strict");
    req.regex = true;
    req.multiline = true;
    req.output_json = true;
    let result: serde_json::Value = serde_json::from_str(&grep(&engram, req).await).unwrap();
    assert!(
        result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["file_path"] == "src/new.rs")
    );
}

#[tokio::test]
async fn unchanged_legacy_encoded_file_retains_verified_index_hits() {
    let (_tmp, engram, pid, _root) = setup_with_source(
        b"pub fn submit_order(id: u64) -> bool {\n    tracker_marker(id)\n}\n// legacy \xe9\n",
    )
    .await;
    let mut req = grep_request(&pid, "tracker_marker", "strict");
    req.output_json = true;
    let result: serde_json::Value = serde_json::from_str(&grep(&engram, req).await).unwrap();
    let hit = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["file_path"] == "src/orders.rs")
        .unwrap();
    assert!(hit["doc_id"].is_string());
    assert!(
        result
            .get("stale_paths")
            .and_then(|v| v.as_array())
            .is_none_or(|paths| !paths.iter().any(|p| p == "src/orders.rs"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cold_clients_share_one_project_runtime() {
    let (_tmp, engram, pid, _root) = setup().await;
    engram.state.projects.clear();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let state = engram.state.clone();
        let pid = pid.clone();
        tasks.push(tokio::spawn(async move {
            engram_server::services::project_service::ensure_project_runtime(&state, &pid)
                .await
                .unwrap()
                .search
        }));
    }
    let first = tasks.remove(0).await.unwrap();
    for task in tasks {
        assert!(std::sync::Arc::ptr_eq(&first, &task.await.unwrap()));
    }
}


#[tokio::test]
async fn grep_advertised_citations_roundtrip_and_disk_only_has_none() {
    let source = "pub fn citation_probe() {\r\n    // citation_marker \u{00c5}\r\n}";
    // Code extraction stores a reconstructed LF-terminated chunk. Citation
    // exactness describes those stored bytes, not the original file bytes.
    let stored = "pub fn citation_probe() {\n    // citation_marker \u{00c5}\n}\n";
    let (_tmp, engram, pid, root) = setup_with_source(source.as_bytes()).await;
    let mut request = grep_request(&pid, "citation_marker", "strict");
    request.output_json = true;
    let output: serde_json::Value = serde_json::from_str(&grep(&engram, request.clone()).await).unwrap();
    let hits = output["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit["citation_recovery"]["tool"], "get_chunk");
    assert!(hit["recovery"]["arguments"].get("citation").is_none());
    let json_args = hit["citation_recovery"]["arguments"].clone();
    request.output_json = false;
    let markdown = grep(&engram, request).await;
    let markdown_args: Vec<serde_json::Value> = markdown.lines().filter_map(|line| {
        line.strip_prefix("citation get_chunk arguments: `").and_then(|s| s.strip_suffix('`'))
    }).map(|s| serde_json::from_str(s).unwrap()).collect();
    assert_eq!(markdown_args, vec![json_args.clone()]);
    assert!(markdown.contains("\nget_chunk arguments: `"));
    for args in [json_args, markdown_args[0].clone()] {
        assert_eq!(args["citation"], serde_json::json!({}));
        let result = engram.handle_get_chunk(serde_json::from_value(args).unwrap()).await.unwrap();
        let raw = result.content.iter().filter_map(|c| c.as_text().map(|t| t.text.as_str())).collect::<Vec<_>>().join("\n");
        let page: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(page["identity"], serde_json::json!({"project_id":pid,"namespace":"memory","doc_id":hit["doc_id"]}));
        assert_eq!(page["content"], stored);
        assert_eq!(page["hash_scope"], "raw_stored_utf8_no_normalization");
        assert_eq!(page["raw_content_hash"], format!("blake3-raw-utf8:{}", blake3::hash(stored.as_bytes()).to_hex()));
        assert_ne!(page["raw_content_hash"], format!("blake3-raw-utf8:{}", blake3::hash(source.as_bytes()).to_hex()));
        assert_eq!(page["metadata"]["path"], "src/orders.rs");
        assert!(page.get("continuation").is_none_or(serde_json::Value::is_null));
    }
    std::fs::write(root.join("src/new.rs"), "pub fn only_disk_citation() {}\n").unwrap();
    let mut request = grep_request(&pid, "only_disk_citation", "warn");
    request.output_json = true;
    let json: serde_json::Value = serde_json::from_str(&grep(&engram, request.clone()).await).unwrap();
    let hits = json["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["source"], "working_tree");
    assert_eq!(hits[0]["recovery"]["action"], "read_file");
    assert!(hits[0].get("citation_recovery").is_none());
    request.output_json = false;
    let markdown = grep(&engram, request).await;
    assert!(!markdown.contains("citation get_chunk arguments:"));
    assert!(markdown.contains("no indexed doc_id"));
}
