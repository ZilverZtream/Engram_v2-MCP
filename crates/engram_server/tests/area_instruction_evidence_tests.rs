//! Public area drafting gathers complete evidence without turning routing or
//! quoted implementation into accepted policy or writing authored guidance.
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{state::AppState, tools::Engram};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    future::Future,
    path::{Path, PathBuf},
};

const PID: &str = "area-evidence";
const ENTRY: &str = "src/area/entry.rs";
const CALLER: &str = "src/client.rs";
const POLICY: &str = ".github/instructions/area.instructions.md";
const OUTPUT: &str = ".github/instructions/new-area.instructions.md";
const PROVENANCE: &str = ".github/instructions/new-area.provenance.json";
const ENTRY_BODY: &str = "pub fn authorized_save(allowed: bool) -> bool {\n    allowed\n}\n";
const CALLER_BODY: &str =
    "pub fn save_request() -> bool {\n    crate::area::authorized_save(true)\n}\n";
const SHARED_BODY: &str = "Retain authorization at the action boundary.\nDo not infer new requirements from examples.\nA [private example](src/area/excluded/private.rs) is optional evidence.\n";
const POLICY_BODY: &str = "---\napplyTo: 'src/area/**'\n---\nSave only after authorization.\nRetain the [shared rule](../../AGENTS.md).\nException: read-only probes do not save data.\n";
const NOISY: &str = "src/area/noisy.cs";
const EXTERNAL: &str = "src/api-client.cs";
const LARGE: &str = "src/large-client.cs";
const HISTORY: &str = "src/history-client.cs";
const BAD_EDGE: &str = "src/unverified-client.cs";
const FUTURE: &str = "src/future-client.cs";
const BLOCKED: &str = "src/area/excluded/secret-client.cs";
const EXTERNAL_BODY: &str = "namespace Consumers { public static class Client { public static bool SaveRequest() { return Area.Service.AuthorizedSave(true); } } }\n";
const EXCLUDED_SENTINEL: &str = "EXCLUDED_CONSUMER_BODY_MUST_NOT_BE_RETURNED";

struct Fixture {
    engram: Engram,
    root: PathBuf,
    request: Value,
    entry_node: String,
    caller_node: String,
}

fn indexed_function(engram: &Engram, path: &str, name: &str) -> Node {
    engram
        .state
        .graph
        .query_nodes_by_symbol_name(PID, name, Some(path), 16)
        .unwrap()
        .into_iter()
        .find(|node| {
            matches!(
                node.node_type.as_str(),
                "function" | "method" | "sub" | "procedure"
            )
        })
        .unwrap_or_else(|| panic!("Actual source extraction did not produce {name} in {path}"))
}

fn commit(root: &Path) -> String {
    let repo = git2::Repository::open(root).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature =
        git2::Signature::now("Area behavior fixture", "fixture@example.invalid").unwrap();
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "fixture",
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
    .to_string()
}

async fn fixture(owned: &Path) -> Fixture {
    let root = owned.join("repo");
    std::fs::create_dir_all(root.join("src/area/excluded")).unwrap();
    std::fs::create_dir_all(root.join(".github/instructions")).unwrap();
    std::fs::create_dir_all(root.join("decisions")).unwrap();
    drop(git2::Repository::init(&root).unwrap());
    for (path, body) in [
        (ENTRY, ENTRY_BODY),
        (CALLER, CALLER_BODY),
        (
            "src/area/example.rs",
            "pub fn authorized_example() -> bool { true }\n",
        ),
        (
            "src/area/excluded/private.rs",
            "pub fn excluded_example() -> bool { false }\n",
        ),
        ("AGENTS.md", SHARED_BODY),
        (POLICY, POLICY_BODY),
        (
            "decisions/closed.md",
            "Finding: src/area/entry.rs\nStatus: closed\nNo ruling or accepted exception was recorded.\n",
        ),
    ] {
        std::fs::write(root.join(path), body).unwrap();
    }
    let base = commit(&root);
    let config = Config {
        data_dir: owned.join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _) = AppState::new(config).unwrap();
    state
        .registry
        .register_source_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: "Area drafting behavior".into(),
            project_type: "general".into(),
            directory: root.to_str().unwrap().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    let engram = Engram::new(state);
    let repaired = engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PID, "scope":"full", "max_commits":0,
                "index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !repaired.is_error.unwrap_or(false),
        "The fixture needs a real published source snapshot"
    );
    let generation =
        engram_server::services::project_service::get_active_generation(&engram.state, PID)
            .await
            .unwrap();
    assert!(generation > 0);
    // Bind deterministic routing to the actual canonical/Contains declarations
    // produced by the full source repair, not supplemental shadow identities.
    let entry = indexed_function(&engram, ENTRY, "authorized_save");
    let caller = indexed_function(&engram, CALLER, "save_request");
    engram
        .state
        .graph
        .upsert_edges(
            PID,
            &[Edge {
                source_id: caller.node_id.clone(),
                target_id: entry.node_id.clone(),
                namespace: "memory".into(),
                language: "rust".into(),
                edge_kind: EdgeKind::Calls,
                weight: 1,
                generation: caller.generation,
                metadata: None,
                updated_at_ms: 1,
            }],
        )
        .unwrap();
    Fixture {
        engram,
        root,
        entry_node: entry.node_id,
        caller_node: caller.node_id,
        request: json!({"project_id":PID, "instruction_maintenance":{
            "base_commit":base, "area_context":{"scope":"src/area", "max_files":4},
            "new_instruction_targets":[{"instruction_path":OUTPUT,"provenance_path":PROVENANCE,
                "area_scope":["src/area/**"]}]
        }}),
    }
}

// Runtime ownership encloses AppState, source workers and all blocking work.
// On success or panic it drops before the owned TempDir/database tree.
fn owned_case<F, Fut>(run: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        run(fixture(temporary.path()).await).await;
    });
    drop(runtime);
    temporary.close().unwrap();
}

async fn call(engram: &Engram, request: Value) -> Result<Value, rmcp::ErrorData> {
    let response = engram
        .handle_pre_push_audit(serde_json::from_value(request).unwrap())
        .await?;
    assert!(!response.is_error.unwrap_or(false));
    Ok(serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap())
}

struct NoisyArea {
    body: String,
    large_body: String,
    caller: Node,
    large: Node,
}

async fn prepare_noisy_area(fixture: &mut Fixture, padding: usize) -> NoisyArea {
    let mut body = "namespace Area {\n".to_owned();
    // Genuine extracted classes/fields sort before callable declarations.
    // A consumer query must not mistake this prefix for the area's entrypoints.
    for number in 0..20 {
        body.push_str(&format!(
            " public sealed class Noise{number:02} {{ public bool Flag{number:02}; }}\n"
        ));
    }
    body.push_str(" public static class Service {\n public static bool AuthorizedSave(bool allowed) { return allowed; }\n public static bool ForwardSave() { return AuthorizedSave(true); }\n }\n}\n");
    let large_body = format!(
        "namespace Consumers {{\n public static class LargeClient {{\n public static bool LargeSave() {{ return Area.Service.AuthorizedSave(true); }}\n }}\n}}\n/* LARGE_CONSUMER_COMPLETE_BODY {} */\n",
        "x".repeat(padding)
    );
    let blocked = format!(
        "namespace Consumers {{ public static class SecretClient {{ public const string Marker = \"{EXCLUDED_SENTINEL}\"; public static bool SecretSave() {{ return Area.Service.AuthorizedSave(true); }} }} }}\n"
    );
    for (path, text) in [
        (NOISY, body.as_str()),
        (EXTERNAL, EXTERNAL_BODY),
        (LARGE, large_body.as_str()),
        (
            HISTORY,
            "namespace Consumers { public static class History { public static bool HistoricPeer() { return true; } } }\n",
        ),
        (
            BAD_EDGE,
            "namespace Consumers { public static class Unverified { public static bool UnverifiedSave() { return true; } } }\n",
        ),
        (
            FUTURE,
            "namespace Consumers { public static class Future { public static bool FutureSave() { return Area.Service.AuthorizedSave(true); } } }\n",
        ),
        (BLOCKED, blocked.as_str()),
    ] {
        std::fs::write(fixture.root.join(path), text).unwrap();
    }
    fixture.request["instruction_maintenance"]["base_commit"] = json!(commit(&fixture.root));
    let repaired = fixture
        .engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PID,"scope":"full","max_commits":0,"index_antipatterns":false,
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(!repaired.is_error.unwrap_or(false));
    let target = indexed_function(&fixture.engram, NOISY, "AuthorizedSave");
    let internal = indexed_function(&fixture.engram, NOISY, "ForwardSave");
    let caller = indexed_function(&fixture.engram, EXTERNAL, "SaveRequest");
    let large = indexed_function(&fixture.engram, LARGE, "LargeSave");
    let historical = indexed_function(&fixture.engram, HISTORY, "HistoricPeer");
    let bad = indexed_function(&fixture.engram, BAD_EDGE, "UnverifiedSave");
    let mut future = indexed_function(&fixture.engram, FUTURE, "FutureSave");
    future.generation += 1;
    fixture
        .engram
        .state
        .graph
        .upsert_nodes(PID, &[future.clone()])
        .unwrap();
    let blocked = indexed_function(&fixture.engram, BLOCKED, "SecretSave");
    let edges: Vec<_> = [
        (&caller, EdgeKind::Calls, caller.generation),
        (&large, EdgeKind::Calls, large.generation),
        (&internal, EdgeKind::Calls, internal.generation),
        (
            &historical,
            EdgeKind::TemporalCoupling,
            historical.generation,
        ),
        (&bad, EdgeKind::Calls, bad.generation + 10),
        (&future, EdgeKind::Calls, future.generation),
        (&blocked, EdgeKind::Calls, blocked.generation),
    ]
    .into_iter()
    .map(|(source, kind, generation)| Edge {
        source_id: source.node_id.clone(),
        target_id: target.node_id.clone(),
        edge_kind: kind,
        namespace: "memory".into(),
        language: "csharp".into(),
        generation,
        weight: 1,
        metadata: None,
        updated_at_ms: 1,
    })
    .collect();
    fixture
        .engram
        .state
        .graph
        .upsert_edges(PID, &edges)
        .unwrap();
    fixture.request["instruction_maintenance"]["area_context"] = json!({
        "scope":NOISY,"max_files":1,"exclude_paths":["src/area/excluded"]});
    fixture.request["instruction_maintenance"]["new_instruction_targets"][0]["area_scope"] =
        json!([NOISY]);
    assert!(
        fixture.request["instruction_maintenance"]
            .get("evidence_paths")
            .is_none(),
        "The real regression is automatic discovery, not an oracle-provided evidence list"
    );
    NoisyArea {
        body,
        large_body,
        caller,
        large,
    }
}

fn area_file<'a>(report: &'a Value, path: &str) -> &'a Value {
    report["packet"]["area_evidence"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == path)
        .unwrap_or_else(|| panic!("Missing area file/recovery lead: {path}"))
}

fn has_role(file: &Value, role: &str) -> bool {
    file["roles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == role)
}

fn evidence<'a>(report: &'a Value, path: &str, version: &str) -> &'a Value {
    &report["packet"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap()[version]
}

fn citation(report: &Value, path: &str, version: &str, quote: &str) -> Value {
    json!({"path":path,"version":version,"blake3":evidence(report,path,version)["blake3"],"quote":quote})
}

fn draft(report: &Value) -> Value {
    let entry = citation(report, ENTRY, "after", ENTRY_BODY);
    let caller = citation(report, CALLER, "after", CALLER_BODY);
    let shared = citation(report, "AGENTS.md", "after", SHARED_BODY);
    let policy = citation(report, POLICY, "after", POLICY_BODY);
    let before = citation(report, ENTRY, "before", ENTRY_BODY);
    let claims = [
        ("purpose","purpose","authored_intent","Authorized area saves.",vec![policy.clone()]),
        ("authorization","invariant","authored_intent","Retain action authorization.",vec![shared.clone(),policy.clone()]),
        ("example","house_example","observed_implementation",ENTRY_BODY,vec![entry.clone(),before]),
        ("companion","companion_file","observed_implementation","The selected source has a caller.",vec![entry.clone(),caller.clone()]),
        ("entry","entry_point","observed_implementation","The area declares authorized_save.",vec![entry.clone()]),
        ("consumer","consumer","observed_implementation","The external client calls the area.",vec![caller]),
        ("validation","validation_path","proposed_policy","Review allowed and denied action paths; execution is pending.",vec![entry]),
    ].into_iter().map(|(id,section,kind,text,evidence)| json!({"id":id,"section":section,
        "kind":kind,"text":text,"rationale":"Review the complete captured evidence and its limits.","evidence":evidence})).collect::<Vec<_>>();
    let mut proposal = json!({"expected_packet_blake3":report["packet_blake3"],"new_area_files":[{
        "instruction_path":OUTPUT,"claims":claims,"shared_rules":[shared,policy],"unresolved_decisions":[]
    }]});
    proposal["new_area_files"][0]["claims"].as_array_mut().unwrap().push(json!({
        "id":"probe-exception","section":"accepted_exception","kind":"authored_intent",
        "text":"Read-only probes do not save data.","rationale":"Preserve the authored condition without asserting its acceptance.",
        "evidence":[citation(report,POLICY,"after",POLICY_BODY)],
        "explicit_ruling":citation(report,POLICY,"after","Exception: read-only probes do not save data.")
    }));
    proposal
}

fn assert_no_authored_outputs(fixture: &Fixture) {
    for path in [OUTPUT, PROVENANCE] {
        assert!(!fixture.root.join(path).exists());
    }
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(POLICY)).unwrap(),
        POLICY_BODY
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("AGENTS.md")).unwrap(),
        SHARED_BODY
    );
}

#[test]
fn scope_only_area_request_collects_complete_guidance_and_external_consumer_without_widening_scope()
{
    owned_case(|fixture| async move {
        let report = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        assert_eq!(report["packet"]["version"], 5);
        for (path, body) in [
            (ENTRY, ENTRY_BODY),
            (CALLER, CALLER_BODY),
            ("AGENTS.md", SHARED_BODY),
            (POLICY, POLICY_BODY),
        ] {
            let source = evidence(&report, path, "after");
            assert_eq!(source["content"], body);
            assert_eq!(
                source["blake3"],
                blake3::hash(body.as_bytes()).to_hex().to_string()
            );
        }
        let area = &report["packet"]["area_evidence"];
        let external = area["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == CALLER)
            .unwrap();
        assert!(
            external["roles"]
                .as_array()
                .unwrap()
                .iter()
                .any(|role| role == "consumer")
        );
        assert!(
            area["symbols_and_relations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["node_id"] == fixture.caller_node.as_str()
                    && symbol["related_to"] == fixture.entry_node.as_str())
        );
        assert_eq!(
            report["packet"]["new_instruction_targets"][0]["target"]["area_scope"],
            json!(["src/area/**"])
        );
        assert!(report["new_area_proposals"].as_array().unwrap().is_empty());
        assert_eq!(report["authored_files_written"], 0);
        assert_no_authored_outputs(&fixture);
    });
}

#[test]
fn bounded_excluded_and_stale_area_evidence_exposes_gaps_and_refuses_stale_drafts() {
    owned_case(|fixture| async move {
        let mut bounded = fixture.request.clone();
        bounded["instruction_maintenance"]["area_context"]["max_files"] = json!(1);
        bounded["instruction_maintenance"]["area_context"]["exclude_paths"] =
            json!(["src/area/excluded"]);
        let report = call(&fixture.engram, bounded.clone()).await.unwrap();
        let area = &report["packet"]["area_evidence"];
        assert!(
            area["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap.as_str().unwrap().contains("cap"))
        );
        assert!(
            report["packet"]["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| !file["path"]
                    .as_str()
                    .unwrap()
                    .starts_with("src/area/excluded/"))
        );
        let manifest = &report["packet"]["instruction_manifest"];
        assert!(
            manifest["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["status"] == "excluded_area_boundary")
        );
        assert!(
            manifest["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| !file["path"]
                    .as_str()
                    .unwrap()
                    .starts_with("src/area/excluded/")),
            "A shared-guidance link must not reopen excluded complete bodies"
        );
        bounded["instruction_maintenance"]["evidence_paths"] =
            json!(["src/area/excluded/private.rs"]);
        assert!(
            call(&fixture.engram, bounded)
                .await
                .unwrap_err()
                .message
                .contains("excluded")
        );

        let original = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        let mut old_draft = fixture.request.clone();
        old_draft["instruction_maintenance"]["draft"] = draft(&original);
        let file_id = format!("file:{ENTRY}");
        let file = fixture
            .engram
            .state
            .graph
            .get_node(PID, &file_id)
            .unwrap()
            .unwrap();
        let mut future = file.clone();
        future.generation += 1;
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(PID, &[future])
            .unwrap();
        let held = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        assert_eq!(evidence(&held, ENTRY, "after")["content"], ENTRY_BODY);
        assert!(
            held["packet"]["area_evidence"]["symbols_and_relations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|symbol| symbol["path"] != ENTRY
                    && symbol["related_to"] != fixture.entry_node.as_str()),
            "A future file node cannot validate selected older declarations or their callers, even with matching bytes"
        );
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(PID, &[file])
            .unwrap();
        let changed =
            "pub fn authorized_save(allowed: bool) -> bool {\n    allowed && current_check()\n}\n";
        std::fs::write(fixture.root.join(ENTRY), changed).unwrap();
        let current = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        assert_eq!(evidence(&current, ENTRY, "after")["content"], changed);
        assert!(
            current["packet"]["area_evidence"]["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap.as_str().unwrap().contains("changed since indexing"))
        );
        assert!(
            current["packet"]["area_evidence"]["symbols_and_relations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|symbol| symbol["path"] != ENTRY
                    && symbol["related_to"] != fixture.entry_node.as_str())
        );
        assert!(
            call(&fixture.engram, old_draft)
                .await
                .unwrap_err()
                .message
                .contains("Stale maintenance proposal")
        );
        assert_no_authored_outputs(&fixture);
    });
}

#[test]
fn area_proposals_are_stable_self_contained_and_closed_status_does_not_accept_an_exception() {
    owned_case(|fixture| async move {
        let mut request = fixture.request.clone();
        request["instruction_maintenance"]["evidence_paths"] = json!(["decisions/closed.md"]);
        let packet = call(&fixture.engram, request.clone()).await.unwrap();
        request["instruction_maintenance"]["draft"] = draft(&packet);
        let first = call(&fixture.engram, request.clone()).await.unwrap();
        let second = call(&fixture.engram, request.clone()).await.unwrap();
        assert_eq!(first["new_area_proposals"], second["new_area_proposals"]);
        let area = &first["new_area_proposals"][0];
        let provenance: Value =
            serde_json::from_str(area["provenance"]["proposed_content"].as_str().unwrap()).unwrap();
        assert_eq!(provenance["policy_acceptance"], "not_established");
        assert_eq!(provenance["policy_correctness"], "not_established");
        assert_eq!(
            provenance["area_evidence"],
            packet["packet"]["area_evidence"]
        );
        let mut keys = BTreeSet::new();
        for source in provenance["evidence_sources"].as_array().unwrap() {
            let path = source["path"].as_str().unwrap();
            let version = source["version"].as_str().unwrap();
            assert!(
                keys.insert((path, version)),
                "Repeated claim/shared/ruling citations must not duplicate complete bodies"
            );
            assert_eq!(
                source["content"],
                evidence(&packet, path, version)["content"]
            );
            assert_eq!(source["blake3"], evidence(&packet, path, version)["blake3"]);
            assert_eq!(
                source["blake3"],
                blake3::hash(source["content"].as_str().unwrap().as_bytes())
                    .to_hex()
                    .to_string()
            );
        }
        assert!(keys.contains(&(ENTRY, "before")) && keys.contains(&(ENTRY, "after")));
        assert!(keys.contains(&(POLICY, "after")) && keys.contains(&("AGENTS.md", "after")));
        assert!(area["gaps"].as_array().unwrap().iter().all(|gap| {
            !gap.as_str()
                .unwrap()
                .contains("consumer: observation is outside")
        }));
        let consumer = provenance["claims"]
            .as_array()
            .unwrap()
            .iter()
            .find(|claim| claim["claim"]["id"] == "consumer")
            .unwrap();
        assert!(
            consumer["scope_relationship"]
                .as_str()
                .unwrap()
                .contains("does not broaden")
        );
        let content = area["instruction"]["proposed_content"].as_str().unwrap();
        assert!(content.contains("applyTo: \"src/area/**\""));
        assert!(
            !content.contains(SHARED_BODY),
            "Compact guidance links shared rules; provenance preserves their complete bodies"
        );
        assert_eq!(first["authored_files_written"], 0);

        let claims = request["instruction_maintenance"]["draft"]["new_area_files"][0]["claims"]
            .as_array_mut()
            .unwrap();
        let exception = claims
            .iter_mut()
            .find(|claim| claim["id"] == "probe-exception")
            .unwrap();
        exception.as_object_mut().unwrap().remove("explicit_ruling");
        exception["evidence"] = json!([citation(
            &packet,
            "decisions/closed.md",
            "after",
            "Status: closed"
        )]);
        let unresolved = call(&fixture.engram, request).await.unwrap();
        assert_eq!(
            unresolved["new_area_proposals"][0]["status"],
            "needs_evidence"
        );
        assert!(
            unresolved["new_area_proposals"][0]["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap
                    .as_str()
                    .unwrap()
                    .contains("workflow status does not establish an accepted exception"))
        );
        assert_no_authored_outputs(&fixture);
    });
}

#[test]
fn noisy_noncallable_declarations_do_not_hide_complete_external_consumers_or_widen_instruction_scope()
 {
    owned_case(|mut fixture| async move {
        let source = prepare_noisy_area(&mut fixture, 150 * 1024).await;
        let report = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        for (path, body) in [
            (EXTERNAL, EXTERNAL_BODY),
            (LARGE, source.large_body.as_str()),
        ] {
            assert_eq!(evidence(&report, path, "after")["content"], body);
            assert_eq!(
                area_file(&report, path)["captured_blake3"],
                blake3::hash(body.as_bytes()).to_hex().to_string()
            );
            assert!(has_role(area_file(&report, path), "consumer"));
            assert!(
                area_file(&report, path)["capture_limit_bytes"]
                    .as_u64()
                    .unwrap()
                    >= body.len() as u64
            );
        }
        let area = &report["packet"]["area_evidence"];
        let symbols = area["symbols_and_relations"].as_array().unwrap();
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol["node_id"] == source.caller.node_id
                    && symbol["role"] == "consumer"),
            "Real callable consumers must survive a source file's class/field prefix"
        );
        assert!(
            !has_role(area_file(&report, NOISY), "consumer"),
            "Internal forwarding calls are area context, not external consumers"
        );
        for path in [HISTORY, BAD_EDGE, FUTURE, BLOCKED] {
            assert!(
                area["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|file| file["path"] == path)
                    .all(|file| !has_role(file, "consumer")),
                "Historical, unverified, future or excluded evidence is not a current consumer: {path}"
            );
        }
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains(EXCLUDED_SENTINEL)
        );
        assert_eq!(
            report["packet"]["new_instruction_targets"][0]["target"]["area_scope"],
            json!([NOISY])
        );
        assert_eq!(evidence(&report, POLICY, "after")["content"], POLICY_BODY);
        assert_eq!(report["authored_files_written"], 0);

        // The scoped automatic-code allowance must not silently enlarge the
        // legacy manual-evidence or authored-instruction limits.
        let mut manual = fixture.request.clone();
        manual["instruction_maintenance"]
            .as_object_mut()
            .unwrap()
            .remove("area_context");
        manual["instruction_maintenance"]["evidence_paths"] = json!([LARGE]);
        assert!(call(&fixture.engram, manual).await.is_err());
        let oversized_policy = format!(
            "---\napplyTo: 'src/area/**'\n---\n{}",
            "p".repeat(150 * 1024)
        );
        std::fs::write(fixture.root.join(POLICY), oversized_policy).unwrap();
        let mut authored = fixture.request.clone();
        authored["instruction_maintenance"]["instruction_paths"] = json!([POLICY]);
        let refusal = call(&fixture.engram, authored).await;
        std::fs::write(fixture.root.join(POLICY), POLICY_BODY).unwrap();
        assert!(
            refusal.is_err(),
            "Authored guidance must remain complete and bounded independently of code"
        );
        assert_no_authored_outputs(&fixture);
    });
}

#[test]
fn oversized_known_consumers_have_executable_range_recovery_and_real_callee_remaps_preserve_only_current_calls()
 {
    owned_case(|mut fixture| async move {
        let source = prepare_noisy_area(&mut fixture, 280 * 1024).await;
        let report = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        let lead = area_file(&report, LARGE);
        assert!(has_role(lead, "consumer_recovery_candidate"));
        assert!(!has_role(lead, "consumer"));
        assert!(lead["captured_blake3"].is_null());
        assert!(
            report["packet"]["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| file["path"] != LARGE),
            "A deferred body is not a complete file citation"
        );
        let recovery = &lead["recovery"];
        assert_eq!(recovery["tool"], "get_full_method_body");
        assert_eq!(recovery["arguments"]["file_path"], LARGE);
        assert_eq!(recovery["arguments"]["line_start"], source.large.start_line);
        assert_eq!(recovery["arguments"]["line_end"], source.large.end_line);
        assert_eq!(
            recovery["expected_indexed_file_blake3"],
            blake3::hash(source.large_body.as_bytes())
                .to_hex()
                .to_string()
        );
        let recovered = fixture
            .engram
            .handle_get_full_method_body(
                serde_json::from_value(recovery["arguments"].clone()).unwrap(),
            )
            .await
            .unwrap();
        assert!(!recovered.is_error.unwrap_or(false));
        let recovered: Value =
            serde_json::from_str(&recovered.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(recovered["retrieval_scope"], "explicit_source_range");
        assert_eq!(
            recovered["source_file_hash"],
            recovery["expected_indexed_file_blake3"]
        );
        assert!(
            recovered["source_code"]
                .as_str()
                .unwrap()
                .contains("AuthorizedSave")
        );
        assert!(
            report["packet"]["area_evidence"]["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap.as_str().unwrap().contains(LARGE)),
            "The known omitted consumer must remain a named evidence gap"
        );

        // Exercise the real source-update producer: moving only the callee
        // remaps unchanged caller edges and preserves their source revision.
        std::fs::write(
            fixture.root.join(NOISY),
            format!("// moved area declarations\n\n{}", source.body),
        )
        .unwrap();
        let update = fixture
            .engram
            .handle_update_project(
                serde_json::from_value(json!({
                    "project_id":PID,"wait":true,"max_commits":0,"index_antipatterns":false,
                    "reindex_paths":[NOISY],
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!update.is_error.unwrap_or(false));
        let current_caller = indexed_function(&fixture.engram, EXTERNAL, "SaveRequest");
        assert_eq!(
            current_caller.generation, source.caller.generation,
            "The unchanged consumer must remain an inherited physical source revision"
        );
        let moved = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        assert!(has_role(area_file(&moved, EXTERNAL), "consumer"));
        assert_eq!(
            evidence(&moved, EXTERNAL, "after")["content"],
            EXTERNAL_BODY
        );
        let source_generation =
            moved["packet"]["area_evidence"]["discovery_coverage"]["source_generation"]
                .as_u64()
                .unwrap();
        assert!(source_generation > current_caller.generation);

        // Refreshing the stable caller identity after removing its invocation
        // must retire the remapped edge instead of reviving its origin proof.
        std::fs::write(fixture.root.join(EXTERNAL),
            "namespace Consumers { public static class Client { public static bool SaveRequest() { return true; } } }\n").unwrap();
        let update = fixture
            .engram
            .handle_update_project(
                serde_json::from_value(json!({
                    "project_id":PID,"wait":true,"max_commits":0,"index_antipatterns":false,
                    "reindex_paths":[EXTERNAL],
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!update.is_error.unwrap_or(false));
        let removed = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        assert!(
            removed["packet"]["area_evidence"]["files"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|file| file["path"] == EXTERNAL)
                .all(|file| !has_role(file, "consumer"))
        );
        assert_eq!(
            removed["packet"]["new_instruction_targets"][0]["target"]["area_scope"],
            json!([NOISY])
        );
        assert_no_authored_outputs(&fixture);
    });
}

#[test]
fn managed_source_with_legacy_only_function_ids_still_discovers_its_external_consumer() {
    owned_case(|fixture| async move {
        let entry_file = fixture
            .engram
            .state
            .graph
            .get_node(PID, &format!("file:{ENTRY}"))
            .unwrap()
            .unwrap();
        let caller_file = fixture
            .engram
            .state
            .graph
            .get_node(PID, &format!("file:{CALLER}"))
            .unwrap()
            .unwrap();
        // This owned graph intentionally represents the older producer shape:
        // valid file/revision facts, custom function IDs and no Contains index.
        fixture.engram.state.graph.delete_project_data(PID).unwrap();
        let entry = Node {
            node_id: "legacy-function:authorized-save".into(),
            node_type: "function".into(),
            name: "authorized_save".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: RelPath::new(ENTRY),
            start_line: 1,
            end_line: 3,
            generation: entry_file.generation,
            metadata: None,
        };
        let caller = Node {
            node_id: "legacy-function:save-request".into(),
            node_type: "function".into(),
            name: "save_request".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: RelPath::new(CALLER),
            start_line: 1,
            end_line: 3,
            generation: caller_file.generation,
            metadata: None,
        };
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(
                PID,
                &[entry_file, caller_file, entry.clone(), caller.clone()],
            )
            .unwrap();
        fixture
            .engram
            .state
            .graph
            .upsert_edges(
                PID,
                &[Edge {
                    source_id: caller.node_id.clone(),
                    target_id: entry.node_id.clone(),
                    edge_kind: EdgeKind::Calls,
                    namespace: "memory".into(),
                    language: "rust".into(),
                    generation: caller.generation,
                    weight: 1,
                    metadata: None,
                    updated_at_ms: 1,
                }],
            )
            .unwrap();
        let mut request = fixture.request.clone();
        request["instruction_maintenance"]["area_context"]["scope"] = json!(ENTRY);
        request["instruction_maintenance"]["new_instruction_targets"][0]["area_scope"] =
            json!([ENTRY]);
        let report = call(&fixture.engram, request).await.unwrap();
        assert!(has_role(area_file(&report, CALLER), "consumer"));
        assert_eq!(evidence(&report, CALLER, "after")["content"], CALLER_BODY);
        assert!(
            report["packet"]["area_evidence"]["symbols_and_relations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["node_id"] == caller.node_id
                    && symbol["related_to"] == entry.node_id)
        );
        assert_no_authored_outputs(&fixture);
    });
}

#[test]
fn type_only_area_preserves_external_dependencies_without_labelling_types_as_entrypoints() {
    owned_case(|mut fixture| async move {
        const TYPES: &str = "src/area/settings.cs";
        const TYPE_CLIENT: &str = "src/type-client.cs";
        const TYPES_BODY: &str =
            "namespace AreaTypes { public class SharedSettings { public static bool Enabled; } }\n";
        const CLIENT_BODY: &str =
            "namespace Consumers { public class UsesSettings : AreaTypes.SharedSettings { } }\n";
        for (path, body) in [(TYPES, TYPES_BODY), (TYPE_CLIENT, CLIENT_BODY)] {
            std::fs::write(fixture.root.join(path), body).unwrap();
        }
        fixture.request["instruction_maintenance"]["base_commit"] = json!(commit(&fixture.root));
        let repaired = fixture
            .engram
            .handle_repair_project(
                serde_json::from_value(json!({
                    "project_id":PID,"scope":"full","max_commits":0,"index_antipatterns":false,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!repaired.is_error.unwrap_or(false));
        // There are no explicit methods in this area. Its type contract still
        // has a real outside consumer and must survive callable prioritization.
        let indexed_class = |path: &str, name: &str| {
            fixture
                .engram
                .state
                .graph
                .query_nodes_by_symbol_name(PID, name, Some(path), 16)
                .unwrap()
                .into_iter()
                .find(|node| node.node_type == "class")
                .unwrap_or_else(|| panic!("Actual source extraction did not produce class {name}"))
        };
        let settings = indexed_class(TYPES, "SharedSettings");
        let consumer = indexed_class(TYPE_CLIENT, "UsesSettings");
        fixture
            .engram
            .state
            .graph
            .upsert_edges(
                PID,
                &[Edge {
                    source_id: consumer.node_id.clone(),
                    target_id: settings.node_id.clone(),
                    edge_kind: EdgeKind::InheritsFrom,
                    namespace: "memory".into(),
                    language: "csharp".into(),
                    generation: consumer.generation,
                    weight: 1,
                    metadata: None,
                    updated_at_ms: 1,
                }],
            )
            .unwrap();
        fixture.request["instruction_maintenance"]["area_context"] =
            json!({"scope":TYPES,"max_files":1});
        fixture.request["instruction_maintenance"]["new_instruction_targets"][0]["area_scope"] =
            json!([TYPES]);
        assert!(
            fixture.request["instruction_maintenance"]
                .get("evidence_paths")
                .is_none()
        );
        let report = call(&fixture.engram, fixture.request.clone())
            .await
            .unwrap();
        assert!(has_role(area_file(&report, TYPE_CLIENT), "consumer"));
        assert_eq!(
            evidence(&report, TYPE_CLIENT, "after")["content"],
            CLIENT_BODY
        );
        assert_eq!(
            area_file(&report, TYPE_CLIENT)["captured_blake3"],
            blake3::hash(CLIENT_BODY.as_bytes()).to_hex().to_string()
        );
        let symbols = report["packet"]["area_evidence"]["symbols_and_relations"]
            .as_array()
            .unwrap();
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol["node_id"] == consumer.node_id
                    && symbol["related_to"] == settings.node_id
                    && symbol["role"] == "consumer")
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol["node_id"] == settings.node_id
                    && symbol["role"] == "indexed_declaration")
        );
        assert!(
            symbols
                .iter()
                .filter(|symbol| symbol["path"] == TYPES)
                .all(|symbol| symbol["role"] != "callable_declaration_candidate"
                    && symbol["role"] != "entry_point_candidate"),
            "Types and fields establish an indexed contract, not callable runtime entrypoints"
        );
        assert!(!has_role(area_file(&report, TYPES), "consumer"));
        assert_eq!(
            report["packet"]["new_instruction_targets"][0]["target"]["area_scope"],
            json!([TYPES])
        );
        assert_no_authored_outputs(&fixture);
    });
}
