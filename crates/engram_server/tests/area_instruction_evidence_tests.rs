//! Public area drafting gathers complete evidence without turning routing or
//! quoted implementation into accepted policy or writing authored guidance.
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{state::AppState, tools::Engram};
use serde_json::{Value, json};
use std::{collections::BTreeSet, future::Future, path::{Path, PathBuf}};

const PID: &str = "area-evidence";
const ENTRY: &str = "src/area/entry.rs";
const CALLER: &str = "src/client.rs";
const POLICY: &str = ".github/instructions/area.instructions.md";
const OUTPUT: &str = ".github/instructions/new-area.instructions.md";
const PROVENANCE: &str = ".github/instructions/new-area.provenance.json";
const ENTRY_BODY: &str = "pub fn authorized_save(allowed: bool) -> bool {\n    allowed\n}\n";
const CALLER_BODY: &str = "pub fn save_request() -> bool {\n    crate::area::authorized_save(true)\n}\n";
const SHARED_BODY: &str = "Retain authorization at the action boundary.\nDo not infer new requirements from examples.\nA [private example](src/area/excluded/private.rs) is optional evidence.\n";
const POLICY_BODY: &str = "---\napplyTo: 'src/area/**'\n---\nSave only after authorization.\nRetain the [shared rule](../../AGENTS.md).\nException: read-only probes do not save data.\n";

struct Fixture {
    engram: Engram,
    root: PathBuf,
    request: Value,
}

fn commit(root: &Path) -> String {
    let repo = git2::Repository::open(root).unwrap();
    let mut index = repo.index().unwrap();
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Area behavior fixture", "fixture@example.invalid").unwrap();
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    repo.commit(Some("HEAD"), &signature, &signature, "fixture", &tree,
        &parent.iter().collect::<Vec<_>>()).unwrap().to_string()
}

async fn fixture(owned: &Path) -> Fixture {
    let root = owned.join("repo");
    std::fs::create_dir_all(root.join("src/area/excluded")).unwrap();
    std::fs::create_dir_all(root.join(".github/instructions")).unwrap();
    std::fs::create_dir_all(root.join("decisions")).unwrap();
    drop(git2::Repository::init(&root).unwrap());
    for (path, body) in [
        (ENTRY, ENTRY_BODY), (CALLER, CALLER_BODY),
        ("src/area/example.rs", "pub fn authorized_example() -> bool { true }\n"),
        ("src/area/excluded/private.rs", "pub fn excluded_example() -> bool { false }\n"),
        ("AGENTS.md", SHARED_BODY), (POLICY, POLICY_BODY),
        ("decisions/closed.md", "Finding: src/area/entry.rs\nStatus: closed\nNo ruling or accepted exception was recorded.\n"),
    ] {
        std::fs::write(root.join(path), body).unwrap();
    }
    let base = commit(&root);
    let config = Config {
        data_dir: owned.join("data"), allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(), ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _) = AppState::new(config).unwrap();
    state.registry.register_source_project(&ProjectRecord {
        project_id: PID.into(), project_name: "Area drafting behavior".into(),
        project_type: "general".into(), directory: root.to_str().unwrap().into(),
        created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    }).unwrap();
    let engram = Engram::new(state);
    let repaired = engram.handle_repair_project(serde_json::from_value(json!({
        "project_id":PID, "scope":"full", "max_commits":0,
        "index_antipatterns":false
    })).unwrap()).await.unwrap();
    assert!(!repaired.is_error.unwrap_or(false), "The fixture needs a real published source snapshot");
    let generation = engram_server::services::project_service::get_active_generation(&engram.state, PID).await.unwrap();
    assert!(generation > 0);
    // Exact-generation routing evidence: actual indexed complete source remains
    // mandatory. The edge only makes external caller discovery deterministic.
    let nodes = [("fixture-entry", ENTRY, "authorized_save"),
        ("fixture-caller", CALLER, "save_request")].map(|(id, path, name)| Node {
        node_id: id.into(), node_type: "function".into(), name: name.into(),
        namespace: "memory".into(), language: "rust".into(), file_path: RelPath::new(path),
        start_line: 1, end_line: 3, generation, metadata: None,
    });
    engram.state.graph.upsert_nodes(PID, &nodes).unwrap();
    engram.state.graph.upsert_edges(PID, &[Edge {
        source_id: "fixture-caller".into(), target_id: "fixture-entry".into(),
        namespace: "memory".into(), language: "rust".into(), edge_kind: EdgeKind::Calls,
        weight: 1, generation, metadata: None, updated_at_ms: 1,
    }]).unwrap();
    Fixture { engram, root, request: json!({"project_id":PID, "instruction_maintenance":{
        "base_commit":base, "area_context":{"scope":"src/area", "max_files":4},
        "new_instruction_targets":[{"instruction_path":OUTPUT,"provenance_path":PROVENANCE,
            "area_scope":["src/area/**"]}]
    }}) }
}

// Runtime ownership encloses AppState, source workers and all blocking work.
// On success or panic it drops before the owned TempDir/database tree.
fn owned_case<F, Fut>(run: F)
where F: FnOnce(Fixture) -> Fut, Fut: Future<Output = ()> {
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2)
        .enable_all().build().unwrap();
    runtime.block_on(async { run(fixture(temporary.path()).await).await; });
    drop(runtime);
    temporary.close().unwrap();
}

async fn call(engram: &Engram, request: Value) -> Result<Value, rmcp::ErrorData> {
    let response = engram.handle_pre_push_audit(serde_json::from_value(request).unwrap()).await?;
    assert!(!response.is_error.unwrap_or(false));
    Ok(serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap())
}

fn evidence<'a>(report: &'a Value, path: &str, version: &str) -> &'a Value {
    &report["packet"]["evidence"].as_array().unwrap().iter()
        .find(|entry| entry["path"] == path).unwrap()[version]
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
    for path in [OUTPUT, PROVENANCE] { assert!(!fixture.root.join(path).exists()); }
    assert_eq!(std::fs::read_to_string(fixture.root.join(POLICY)).unwrap(), POLICY_BODY);
    assert_eq!(std::fs::read_to_string(fixture.root.join("AGENTS.md")).unwrap(), SHARED_BODY);
}

#[test]
fn scope_only_area_request_collects_complete_guidance_and_external_consumer_without_widening_scope() {
    owned_case(|fixture| async move {
        let report = call(&fixture.engram, fixture.request.clone()).await.unwrap();
        assert_eq!(report["packet"]["version"], 5);
        for (path, body) in [(ENTRY, ENTRY_BODY), (CALLER, CALLER_BODY),
            ("AGENTS.md", SHARED_BODY), (POLICY, POLICY_BODY)] {
            let source = evidence(&report, path, "after");
            assert_eq!(source["content"], body);
            assert_eq!(source["blake3"], blake3::hash(body.as_bytes()).to_hex().to_string());
        }
        let area = &report["packet"]["area_evidence"];
        let external = area["files"].as_array().unwrap().iter().find(|file| file["path"] == CALLER).unwrap();
        assert!(external["roles"].as_array().unwrap().iter().any(|role| role == "consumer"));
        assert!(area["symbols_and_relations"].as_array().unwrap().iter()
            .any(|symbol| symbol["node_id"] == "fixture-caller" && symbol["related_to"] == "fixture-entry"));
        assert_eq!(report["packet"]["new_instruction_targets"][0]["target"]["area_scope"], json!(["src/area/**"]));
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
        bounded["instruction_maintenance"]["area_context"]["exclude_paths"] = json!(["src/area/excluded"]);
        let report = call(&fixture.engram, bounded.clone()).await.unwrap();
        let area = &report["packet"]["area_evidence"];
        assert!(area["gaps"].as_array().unwrap().iter().any(|gap| gap.as_str().unwrap().contains("cap")));
        assert!(report["packet"]["evidence"].as_array().unwrap().iter()
            .all(|file| !file["path"].as_str().unwrap().starts_with("src/area/excluded/")));
        let manifest = &report["packet"]["instruction_manifest"];
        assert!(manifest["edges"].as_array().unwrap().iter()
            .any(|edge| edge["status"] == "excluded_area_boundary"));
        assert!(manifest["dependencies"].as_array().unwrap().iter()
            .all(|file| !file["path"].as_str().unwrap().starts_with("src/area/excluded/")),
            "A shared-guidance link must not reopen excluded complete bodies");
        bounded["instruction_maintenance"]["evidence_paths"] = json!(["src/area/excluded/private.rs"]);
        assert!(call(&fixture.engram, bounded).await.unwrap_err().message.contains("excluded"));

        let original = call(&fixture.engram, fixture.request.clone()).await.unwrap();
        let mut old_draft = fixture.request.clone();
        old_draft["instruction_maintenance"]["draft"] = draft(&original);
        let file_id = format!("file:{ENTRY}");
        let file = fixture.engram.state.graph.get_node(PID, &file_id).unwrap().unwrap();
        let mut future = file.clone();
        future.generation += 1;
        fixture.engram.state.graph.upsert_nodes(PID, &[future]).unwrap();
        let held = call(&fixture.engram, fixture.request.clone()).await.unwrap();
        assert_eq!(evidence(&held, ENTRY, "after")["content"], ENTRY_BODY);
        assert!(held["packet"]["area_evidence"]["symbols_and_relations"].as_array().unwrap()
            .iter().all(|symbol| symbol["path"] != ENTRY && symbol["related_to"] != "fixture-entry"),
            "A future file node cannot validate selected older declarations or their callers, even with matching bytes");
        fixture.engram.state.graph.upsert_nodes(PID, &[file]).unwrap();
        let changed = "pub fn authorized_save(allowed: bool) -> bool {\n    allowed && current_check()\n}\n";
        std::fs::write(fixture.root.join(ENTRY), changed).unwrap();
        let current = call(&fixture.engram, fixture.request.clone()).await.unwrap();
        assert_eq!(evidence(&current, ENTRY, "after")["content"], changed);
        assert!(current["packet"]["area_evidence"]["gaps"].as_array().unwrap().iter()
            .any(|gap| gap.as_str().unwrap().contains("changed since indexing")));
        assert!(current["packet"]["area_evidence"]["symbols_and_relations"].as_array().unwrap()
            .iter().all(|symbol| symbol["path"] != ENTRY && symbol["related_to"] != "fixture-entry"));
        assert!(call(&fixture.engram, old_draft).await.unwrap_err().message.contains("Stale maintenance proposal"));
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
        let provenance: Value = serde_json::from_str(area["provenance"]["proposed_content"].as_str().unwrap()).unwrap();
        assert_eq!(provenance["policy_acceptance"], "not_established");
        assert_eq!(provenance["policy_correctness"], "not_established");
        assert_eq!(provenance["area_evidence"], packet["packet"]["area_evidence"]);
        let mut keys = BTreeSet::new();
        for source in provenance["evidence_sources"].as_array().unwrap() {
            let path = source["path"].as_str().unwrap();
            let version = source["version"].as_str().unwrap();
            assert!(keys.insert((path, version)), "Repeated claim/shared/ruling citations must not duplicate complete bodies");
            assert_eq!(source["content"], evidence(&packet,path,version)["content"]);
            assert_eq!(source["blake3"], evidence(&packet,path,version)["blake3"]);
            assert_eq!(source["blake3"], blake3::hash(source["content"].as_str().unwrap().as_bytes()).to_hex().to_string());
        }
        assert!(keys.contains(&(ENTRY, "before")) && keys.contains(&(ENTRY, "after")));
        assert!(keys.contains(&(POLICY, "after")) && keys.contains(&("AGENTS.md", "after")));
        assert!(area["gaps"].as_array().unwrap().iter().all(|gap|
            !gap.as_str().unwrap().contains("consumer: observation is outside")));
        let consumer = provenance["claims"].as_array().unwrap().iter()
            .find(|claim| claim["claim"]["id"] == "consumer").unwrap();
        assert!(consumer["scope_relationship"].as_str().unwrap().contains("does not broaden"));
        let content = area["instruction"]["proposed_content"].as_str().unwrap();
        assert!(content.contains("applyTo: \"src/area/**\""));
        assert!(!content.contains(SHARED_BODY), "Compact guidance links shared rules; provenance preserves their complete bodies");
        assert_eq!(first["authored_files_written"], 0);

        let claims = request["instruction_maintenance"]["draft"]["new_area_files"][0]["claims"].as_array_mut().unwrap();
        let exception = claims.iter_mut().find(|claim| claim["id"] == "probe-exception").unwrap();
        exception.as_object_mut().unwrap().remove("explicit_ruling");
        exception["evidence"] = json!([citation(&packet,"decisions/closed.md","after","Status: closed")]);
        let unresolved = call(&fixture.engram, request).await.unwrap();
        assert_eq!(unresolved["new_area_proposals"][0]["status"], "needs_evidence");
        assert!(unresolved["new_area_proposals"][0]["gaps"].as_array().unwrap().iter()
            .any(|gap| gap.as_str().unwrap().contains("workflow status does not establish an accepted exception")));
        assert_no_authored_outputs(&fixture);
    });
}
