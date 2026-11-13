//! Production stdio MCP contracts on owned source/config/data; no live services.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TOOLS: [&str; 20] = [
    "index_project",
    "list_projects",
    "project_info",
    "get_index_freshness",
    "search_memory",
    "grep_project",
    "get_chunk",
    "get_full_method_body",
    "resolve_id",
    "get_method_edit_context",
    "get_page_context",
    "get_change_set",
    "pre_commit_review",
    "get_review_decisions",
    "read_memory_bank",
    "begin_edit_session",
    "complete_edit_session",
    "check_edit_safety",
    "find_symbol_references",
    "prepare_implementation_context",
];
const FILE: &str = "client/upload.ts";
const METHOD: &str = "validateImageUpload";

struct Server {
    child: Option<Child>,
    input: Option<ChildStdin>,
    reader: Option<JoinHandle<()>>,
    lines: Receiver<std::io::Result<String>>,
    sequence: u64,
    called: BTreeSet<String>,
    stderr: PathBuf,
}
impl Server {
    fn start(owned: &Path, roots: &[PathBuf]) -> Result<Self> {
        Self::start_with_backend(owned, roots, Some("fts_only"))
    }
    fn start_with_backend(owned: &Path, roots: &[PathBuf], backend: Option<&str>) -> Result<Self> {
        let config = owned.join("config.json");
        let mut settings = json!({
            "data_dir":owned.join("data"), "allowed_roots":roots,
            "multi_client":false, "advertise_all_tools":true,
            "max_project_files":32, "max_project_bytes":262144
        });
        if let Some(backend) = backend {
            settings["embedding_backend"] = json!(backend);
            settings["llm_backend"] = json!("none");
            settings["max_concurrent_jobs"] = json!(1);
        }
        std::fs::write(&config, serde_json::to_vec(&settings)?)?;
        let stderr = owned.join("server.stderr");
        let executable = if let Some(path) = std::env::var_os("ENGRAM_CONTRACT_SERVER") {
            let expected = std::env::var("ENGRAM_CONTRACT_SERVER_SHA256")
                .context("Explicit candidate needs its SHA256 pin")?;
            let mut file = std::fs::File::open(&path)?;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            ensure!(
                format!("{:x}", digest.finalize()) == expected.to_ascii_lowercase(),
                "Candidate server identity changed"
            );
            PathBuf::from(path)
        } else {
            PathBuf::from(env!("CARGO_BIN_EXE_engram_server"))
        };
        let mut command = Command::new(executable);
        command
            .arg("--no-multi-client")
            .arg("--no-daemon")
            .current_dir(owned)
            .env("ENGRAM_CONFIG_PATH", &config)
            .env("ENGRAM_MULTI_CLIENT", "0")
            .env("ENGRAM_MULTI_CLIENT_DAEMON", "0")
            // This portable contract tier must not depend on an installed sidecar.
            .env("ENGRAM_VB_SIDECAR_PATH", owned.join("no-optional-sidecar"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(std::fs::File::create(&stderr)?));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn()?;
        let input = child.stdin.take().context("Missing child stdin")?;
        let stdout = child.stdout.take().context("Missing child stdout")?;
        let (send, lines) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child: Some(child),
            input: Some(input),
            reader: Some(reader),
            lines,
            sequence: 0,
            called: BTreeSet::new(),
            stderr,
        })
    }
    fn send(&mut self, value: &Value) -> Result<()> {
        println!("MCP_REQUEST {value}");
        let input = self.input.as_mut().context("Client closed")?;
        serde_json::to_writer(&mut *input, value)?;
        input.write_all(b"\n")?;
        input.flush()?;
        Ok(())
    }
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("Waiting for this exact MCP request; no replay")??;
            println!("MCP_RESPONSE {line}");
            ensure!(
                line.len() <= 4 * 1024 * 1024,
                "Unexpectedly large fixture frame"
            );
            let value: Value = serde_json::from_str(&line)?;
            if value["id"] == id {
                return Ok(value);
            }
        }
    }
    fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        self.called.insert(name.to_owned());
        self.rpc("tools/call", json!({"name":name,"arguments":args}))
    }
    fn ok(&mut self, name: &str, args: Value) -> Result<String> {
        success_text(&self.call(name, args)?)
    }
    fn close(&mut self) -> Result<()> {
        drop(self.input.take());
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let (status, forced) = loop {
            if let Some(status) = child.try_wait()? {
                break (status, false);
            }
            if Instant::now() >= deadline {
                child.kill()?;
                break (child.wait()?, true);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("MCP reader panicked"))?;
        }
        if forced || !status.success() || std::thread::panicking() {
            eprintln!(
                "MCP_STDERR {}",
                std::fs::read_to_string(&self.stderr).unwrap_or_default()
            );
        }
        ensure!(
            !forced && status.success(),
            "Server did not exit naturally: {status}, forced={forced}"
        );
        Ok(())
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("Owned MCP teardown: {error:#}");
        }
    }
}

struct Fixture {
    server: Option<Server>,
    temporary: Option<tempfile::TempDir>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.server.take());
        if let Some(temp) = self.temporary.take() {
            let path = temp.path().to_owned();
            if let Err(error) = temp.close() {
                if std::thread::panicking() {
                    eprintln!(
                        "Owned fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                } else {
                    panic!(
                        "Owned fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
}

fn success_text(frame: &Value) -> Result<String> {
    ensure!(frame.get("error").is_none(), "RPC failure: {frame}");
    let result = frame.get("result").context("Missing MCP result")?;
    ensure!(result["isError"] != true, "Tool failure: {frame}");
    let content = result["content"]
        .as_array()
        .context("Missing MCP content array")?;
    ensure!(!content.is_empty());
    Ok(content
        .iter()
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n"))
}
fn require_error(frame: &Value) -> Result<()> {
    if let Some(error) = frame.get("error") {
        ensure!(error["code"].as_i64().is_some_and(|c| c < 0));
        ensure!(
            error["message"]
                .as_str()
                .is_some_and(|m| !m.trim().is_empty())
        );
        return Ok(());
    }
    ensure!(
        frame["result"]["isError"] == true,
        "Expected an explicit error, got {frame}"
    );
    ensure!(frame["result"]["content"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|i| i["text"].as_str().is_some_and(|s| !s.trim().is_empty()))
    }));
    Ok(())
}
fn identity(text: &str) -> Result<String> {
    text.split_once("project_id: ")
        .and_then(|(_, tail)| tail.lines().next())
        .map(str::trim)
        .map(str::to_owned)
        .with_context(|| format!("Missing indexed project ID: {text}"))
}
fn commit(root: &Path) -> Result<()> {
    let repo = git2::Repository::init(root)?;
    let mut index = repo.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let who = git2::Signature::now("Owned Fixture", "fixture@example.invalid")?;
    repo.commit(
        Some("HEAD"),
        &who,
        &who,
        "Authored wire fixture",
        &tree,
        &[],
    )?;
    Ok(())
}
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), target.join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[cfg(feature = "vector")]
#[test]
fn default_search_discloses_projection_ranking() -> Result<()> {
    let mut fixture = Fixture {
        server: None,
        temporary: Some(tempfile::tempdir()?),
    };
    let owned = fixture
        .temporary
        .as_ref()
        .context("Fixture owner missing")?
        .path()
        .to_owned();
    let root = owned.join("project");
    copy_tree(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/webforms_retrieval/source/client"),
        &root,
    )?;
    commit(&root)?;
    // Omit embedding/model, LLM, memory, job and watcher settings so the real
    // configuration loader supplies the defaults users actually receive.
    fixture.server = Some(Server::start_with_backend(
        &owned,
        std::slice::from_ref(&root),
        None,
    )?);
    let client = fixture.server.as_mut().context("Server owner missing")?;
    let initialized = client.rpc("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"owned-defaults-test","version":"1"}}))?;
    ensure!(initialized["result"]["capabilities"]["tools"].is_object());
    client.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
    let pid = identity(&client.ok("index_project", json!({"directory":root,"project_name":"Default source lookup","project_type":"general","wait":true}))?)?;
    let args = json!({"project_id":pid,"query":METHOD,"include_content":true,"output_json":true});
    let result: Value = serde_json::from_str(&client.ok("search_memory", args.clone())?)?;
    ensure!(result["semantic_quality"] == "degraded_trigram");
    // A projection is not semantic evidence. Unknown identifiers must not
    // acquire arbitrary matches merely because the default enables semantic.
    let absent = "zzqxNonexistentPhotoRequirement987654";
    let missing: Value = serde_json::from_str(&client.ok("search_memory", json!({
        "project_id":pid,"query":absent,"search_scope":"code","output_json":true
    }))?)?;
    ensure!(missing["results"].as_array().is_some_and(Vec::is_empty),
        "Default fallback introduced unrelated projection-only matches: {missing}");
    for query in [METHOD, "exportRows", "saveColumnVisibility", "return"] {
        for mmr in [false, true] {
            let mut query_args = json!({"project_id":pid,"query":query,
                "search_scope":"code","use_mmr":mmr,"max_results":2,
                "output_json":true});
            let default: Value = serde_json::from_str(&client.ok("search_memory", query_args.clone())?)?;
            query_args["semantic"] = json!(false);
            let lexical: Value = serde_json::from_str(&client.ok("search_memory", query_args)?)?;
            let identities = |value: &Value| -> Vec<Value> {
                value["results"].as_array().unwrap().iter().map(|hit|
                    json!([hit["path"],hit["doc_id"],hit["chunk_id"]])).collect()
            };
            ensure!(!identities(&lexical).is_empty(), "Authored query lost its answer: {query}");
            ensure!(identities(&default) == identities(&lexical),
                "Default ranking differs from lexical for {query}, MMR={mmr}");
        }
    }
    for offset in 0..4 {
        let mut page_args = json!({"project_id":pid,"query":"return",
            "search_scope":"code","use_mmr":true,"max_results":1,
            "offset":offset,"output_json":true});
        let default: Value = serde_json::from_str(&client.ok("search_memory", page_args.clone())?)?;
        page_args["semantic"] = json!(false);
        let lexical: Value = serde_json::from_str(&client.ok("search_memory", page_args)?)?;
        ensure!(default["results"] == lexical["results"],
            "Default fallback changed a lexical page at offset {offset}");
    }
    // Deliberate vector lookup remains available as an explicitly degraded
    // operation; the fallback must not disable or destroy its stored vectors.
    let vectors: Value = serde_json::from_str(&client.ok("vector_search", json!({
        "project_id":pid,"query":METHOD,"output_json":true
    }))?)?;
    ensure!(vectors["semantic_quality"] == "degraded_trigram");
    ensure!(vectors["results"].as_array().is_some_and(|hits| !hits.is_empty()));
    ensure!(
        result["results"]
            .as_array()
            .is_some_and(|hits| hits.iter().any(|hit| hit["path"] == "upload.ts"
                && hit["content"]
                    .as_str()
                    .is_some_and(|body| body.contains("file.size > maxBytes")))),
        "Default search must still retrieve the authored answer: {result}"
    );
    let warnings = result["warnings"].to_string();
    ensure!(
        !warnings.contains("Lexical quality is unaffected"),
        "Default warning must not guarantee unaffected ranking"
    );
    ensure!(
        warnings.contains("lexical-only retrieval") && warnings.contains("do not affect ranking"),
        "Default warning must explain the actual lexical fallback: {warnings}"
    );
    let mut markdown_args = args.clone();
    markdown_args["output_json"] = json!(false);
    let markdown = client.ok("search_memory", markdown_args)?;
    ensure!(markdown.contains("lexical-only retrieval") && markdown.contains("do not affect ranking"));
    ensure!(!markdown.contains("Lexical quality is unaffected"));
    let mut lexical_args = args;
    lexical_args["semantic"] = json!(false);
    let lexical: Value = serde_json::from_str(&client.ok("search_memory", lexical_args)?)?;
    ensure!(lexical["semantic_quality"] == "not_used");
    ensure!(
        lexical["results"]
            .as_array()
            .is_some_and(|hits| hits.iter().any(|hit| hit["path"] == "upload.ts"))
    );
    let metrics: Value =
        serde_json::from_str(&client.ok("get_metrics", json!({"output_json":true}))?)?;
    ensure!(metrics["memory"]["budget_bytes"] == 2147483648u64);
    client.close()?;
    drop(fixture);
    ensure!(!owned.exists());
    println!(
        "MCP_DEFAULTS_REPORT {}",
        json!({"status":"PASS","semantic_quality":"degraded_trigram","lexical_quality":"not_used","memory_budget_bytes":2147483648u64,"server_closed":true,"fixture_removed":true,"ranking_comparisons":8,"page_comparisons":4,"absent_identifier_empty":true,"explicit_vectors_retained":true,"scope":"Default preserves lexical source ranking with MMR on/off; broader relevance and productivity not evaluated"})
    );
    Ok(())
}

// Fixed before measurement; each query has its own gate so fast queries cannot
// hide a slower path. This small-fixture gate is not a production latency SLA.
const LATENCY_SAMPLES: usize = 40;
fn latency_gate(samples: &[Duration], budget: Duration) -> Result<Duration> {
    ensure!(
        samples.len() == LATENCY_SAMPLES,
        "Incomplete timing sample set"
    );
    let mut ordered = samples.to_vec();
    ordered.sort_unstable();
    let p95 = ordered[(95 * ordered.len()).div_ceil(100) - 1];
    ensure!(
        p95 <= budget,
        "Fixture p95 {p95:?} exceeds fixed {budget:?} budget"
    );
    Ok(p95)
}

#[test]
fn latency_gate_rejects_slow_tail_and_missing_measurements() {
    let fast = Duration::from_millis(10);
    let slow = Duration::from_millis(300);
    let budget = Duration::from_millis(200);
    let mut samples = vec![fast; LATENCY_SAMPLES];
    samples[0] = slow;
    samples[20] = slow;
    assert_eq!(latency_gate(&samples, budget).unwrap(), fast);
    samples[39] = slow;
    assert!(latency_gate(&samples, budget).is_err());
    assert!(latency_gate(&samples[..39], budget).is_err());
}

#[test]
fn hot_read_latency_through_production_mcp() -> Result<()> {
    let mut fixture = Fixture {
        server: None,
        temporary: Some(tempfile::tempdir()?),
    };
    let owned = fixture
        .temporary
        .as_ref()
        .context("Fixture owner missing")?
        .path()
        .to_owned();
    let root = owned.join("project");
    copy_tree(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/webforms_retrieval/source"),
        &root,
    )?;
    commit(&root)?;
    fixture.server = Some(Server::start(&owned, std::slice::from_ref(&root))?);
    let client = fixture.server.as_mut().context("Server owner missing")?;
    let initialized = client.rpc("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"owned-latency-test","version":"1"}}))?;
    ensure!(initialized["result"]["capabilities"]["tools"].is_object());
    client.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
    let pid = identity(&client.ok("index_project", json!({"directory":root,"project_name":"Read latency fixture","project_type":"general","wait":true}))?)?;
    let mut reports = Vec::new();
    for (file, method) in [
        (FILE, METHOD),
        ("client/export.ts", "exportRows"),
        ("client/grid-preferences.ts", "saveColumnVisibility"),
    ] {
        let authored = std::fs::read_to_string(root.join(file))?;
        ensure!(authored.contains(&format!("export function {method}(")));
        for (tool, budget_ms, args) in [
            (
                "search_memory",
                250,
                json!({"project_id":pid,"query":method,"semantic":false,"fts_mode":"loose","use_mmr":false,"output_json":true,"include_content":true}),
            ),
            (
                "get_method_info",
                200,
                json!({"project_id":pid,"fqn_or_name":method,"file_path":file,"output_json":true}),
            ),
        ] {
            let mut samples = Vec::new();
            let mut first_ms = 0.0;
            // First request + two warm-ups are reported separately from the
            // forty retained samples. Never retry or discard a timed response.
            let mut warmup_ms = Vec::new();
            for attempt in 0..LATENCY_SAMPLES + 3 {
                let start = Instant::now();
                let text = client.ok(tool, args.clone())?;
                let elapsed = start.elapsed();
                let value: Value = serde_json::from_str(&text)?;
                if tool == "search_memory" {
                    ensure!(
                        value["results"]
                            .as_array()
                            .is_some_and(|hits| hits.iter().any(|hit| hit["path"] == file
                                && hit["content"]
                                    .as_str()
                                    .is_some_and(|body| body.contains(method)))),
                        "Fast search must still recover the authored answer: {value}"
                    );
                } else {
                    let methods = value.as_array().context("Expected method result array")?;
                    ensure!(
                        methods.len() == 1
                            && methods[0]["file_path"] == file
                            && methods[0]["fqn"]
                                .as_str()
                                .is_some_and(|name| name.ends_with(method))
                            && methods[0]["line_start"]
                                .as_u64()
                                .is_some_and(|line| line > 0),
                        "Fast lookup must still identify the authored method: {value}"
                    );
                }
                match attempt {
                    0 => first_ms = elapsed.as_secs_f64() * 1000.0,
                    1 | 2 => warmup_ms.push(elapsed.as_secs_f64() * 1000.0),
                    _ => samples.push(elapsed),
                }
            }
            // Print all measurements before enforcing the gate, retaining the
            // complete evidence even when a timing regression fails the test.
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            let report = json!({"tool":tool,"file":file,"method":method,"samples":samples.len(),
                "samples_ms":samples.iter().map(|d|d.as_secs_f64()*1000.0).collect::<Vec<_>>(),
                "first_request_ms":first_ms,"warmup_ms":warmup_ms,"budget_ms":budget_ms,
                "p95_ms":sorted[37].as_secs_f64()*1000.0,"max_ms":sorted[39].as_secs_f64()*1000.0});
            println!("MCP_LATENCY_CASE {report}");
            latency_gate(&samples, Duration::from_millis(budget_ms))?;
            reports.push(report);
        }
    }
    client.close()?;
    drop(fixture);
    ensure!(!owned.exists(), "Latency fixture leaked");
    println!(
        "MCP_LATENCY_REPORT {}",
        json!({"status":"PASS","cases":reports,
        "server_closed":true,"fixture_removed":true,"provider":"none","optional_sidecar":"absent",
        "scope":"13-file authored fixture, sequential warm stdio requests; not production-scale latency"})
    );
    Ok(())
}

#[test]
fn twenty_tools_through_production_mcp() -> Result<()> {
    let mut fixture = Fixture {
        server: None,
        temporary: Some(tempfile::tempdir()?),
    };
    let owned = fixture
        .temporary
        .as_ref()
        .context("Fixture owner missing")?
        .path()
        .to_owned();
    let root = owned.join("project");
    let empty = owned.join("empty");
    let oversized = owned.join("oversized");
    copy_tree(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/webforms_retrieval/source"),
        &root,
    )?;
    for i in 0..8 {
        std::fs::write(
            root.join(format!("client/bulk{i}.ts")),
            format!(
                "export function bulkentry{i}() {{\n // {}\n return {i};\n}}\n",
                "bounded evidence ".repeat(100)
            ),
        )?;
    }
    std::fs::create_dir(&empty)?;
    std::fs::create_dir(&oversized)?;
    for i in 0..33 {
        std::fs::write(
            oversized.join(format!("item{i}.ts")),
            format!("export const item{i} = {i};\n"),
        )?;
    }
    for path in [&root, &empty, &oversized] {
        commit(path)?;
    }
    let upload = std::fs::read_to_string(root.join(FILE))?;
    fixture.server = Some(Server::start(
        &owned,
        &[root.clone(), empty.clone(), oversized.clone()],
    )?);
    let client = fixture.server.as_mut().context("Server owner missing")?;
    let initialized = client.rpc("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"owned-contract-test","version":"1"}}))?;
    ensure!(initialized["result"]["capabilities"]["tools"].is_object());
    client.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
    ensure!(client.ok("list_projects", json!({}))? == "No projects indexed.");
    let pid = identity(&client.ok("index_project", json!({"directory":root,"project_name":"Wire contracts","project_type":"general","wait":true}))?)?;
    let listed = client.rpc("tools/list", json!({}))?;
    let names: BTreeSet<_> = listed["result"]["tools"]
        .as_array()
        .context("Missing advertised tools")?
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for name in TOOLS {
        ensure!(names.contains(name), "Missing advertised tool {name}");
    }
    ensure!(client.ok("list_projects", json!({}))?.contains(&pid));
    ensure!(
        client
            .ok("project_info", json!({"project_id":pid}))?
            .contains("active_generation: 1")
    );
    let freshness = client.ok("get_index_freshness", json!({"project_id":pid}))?;
    ensure!(freshness.lines().any(|line| line == "active_generation: 1"));
    ensure!(
        freshness.contains("disk_check: no_changes_detected")
            && freshness.contains("generation_complete: true")
    );
    let search_args = json!({"project_id":pid,"query":METHOD,"semantic":false,"fts_mode":"loose","use_mmr":false,"output_json":true,"include_content":true});
    let found: Value = serde_json::from_str(&client.ok("search_memory", search_args.clone())?)?;
    let hit = found["results"]
        .as_array()
        .context("Missing search results")?
        .iter()
        .find(|h| h["path"] == FILE)
        .context("Expected authored upload source")?;
    let doc_id = hit["doc_id"]
        .as_str()
        .context("Missing instance identity")?;
    let chunk_args = json!({"project_id":pid,"doc_id":doc_id,"output_json":true});
    let chunk: Value = serde_json::from_str(&client.ok("get_chunk", chunk_args.clone())?)?;
    ensure!(
        chunk["content"]
            .as_str()
            .is_some_and(|body| body.lines().eq(upload.lines())),
        "Chunk must recover exact authored source: {chunk}"
    );
    let recovered = chunk["content"]
        .as_str()
        .context("Missing recovered source")?;
    ensure!(chunk["metadata"]["path"] == FILE);
    ensure!(
        chunk["raw_content_hash"]
            == format!("blake3-raw-utf8:{}", blake3::hash(recovered.as_bytes()))
    );
    let grep_args = json!({"project_id":pid,"pattern":METHOD,"output_json":true});
    ensure!(client.ok("grep_project", grep_args.clone())?.contains(FILE));
    let body_args = json!({"project_id":pid,"file_path":FILE,"line_start":1,"line_end":5});
    ensure!(
        client
            .ok("get_full_method_body", body_args.clone())?
            .contains("file.size > maxBytes")
    );
    let resolve_args = json!({"project_id":pid,"id":doc_id});
    ensure!(
        client
            .ok("resolve_id", resolve_args.clone())?
            .contains(FILE)
    );
    let method_args = json!({"project_id":pid,"file_path":FILE,"method_name":METHOD,"include_business_logic":false,"include_history":false});
    ensure!(
        client
            .ok("get_method_edit_context", method_args.clone())?
            .contains("file.size > maxBytes")
    );
    let page_args = json!({"project_id":pid,"aspx_file":"Site/MapLayers.aspx","include_house_style":false,"output_json":true});
    let page = client.ok("get_page_context", page_args.clone())?;
    let _: Value = serde_json::from_str(&page)?;
    ensure!(page.contains("layers") && page.contains("Site/MapLayers.aspx"));
    let change_args = json!({"project_id":pid,"story":"Adjust image upload size limit","concepts":["upload"],"include_ui_contract":false,"output_json":true});
    ensure!(
        client
            .ok("get_change_set", change_args.clone())?
            .contains(FILE)
    );
    let review_args = json!({"project_id":pid,"diff":"","output_json":true});
    let review: Value =
        serde_json::from_str(&client.ok("pre_commit_review", review_args.clone())?)?;
    ensure!(review["verdict"] == "NO_CHANGES" && review["findings"] == json!([]));
    let decisions_args = json!({"project_id":pid,"review_id":"owned-review"});
    let decisions: Value =
        serde_json::from_str(&client.ok("get_review_decisions", decisions_args.clone())?)?;
    ensure!(decisions["coverage"] == "not_imported" && decisions["events"] == json!([]));
    let memory_args = json!({"project_id":pid,"section":"absent-section","output_json":true});
    let memory: Value = serde_json::from_str(&client.ok("read_memory_bank", memory_args.clone())?)?;
    ensure!(memory["status"] == "not_found" && memory["content"].is_null());
    let begin_args = json!({"project_id":pid,"session_id":"owned-session","planned_files":[FILE]});
    let opened = client.ok("begin_edit_session", begin_args.clone())?;
    let revision = opened
        .lines()
        .find_map(|l| l.strip_prefix("session_revision: "))
        .context("Missing session revision")?;
    let complete_args = json!({"project_id":pid,"session_id":"owned-session","session_revision":revision,"edited_files":[]});
    let closed = client.ok("complete_edit_session", complete_args.clone())?;
    ensure!(
        closed.contains("Edit session COMPLETE")
            && closed.contains("NOT edited")
            && closed.contains(FILE)
    );
    require_error(&client.call("complete_edit_session", complete_args.clone())?)?;
    ensure!(closed.contains("completeness analysis was not run"));
    let omitted_open = client.ok(
        "begin_edit_session",
        json!({"project_id":pid,"session_id":"omitted-session","planned_files":[FILE]}),
    )?;
    let omitted_revision = omitted_open
        .lines()
        .find_map(|l| l.strip_prefix("session_revision: "))
        .context("Missing omitted-list session revision")?;
    let omitted_close = client.ok("complete_edit_session", json!({"project_id":pid,"session_id":"omitted-session","session_revision":omitted_revision}))?;
    ensure!(
        omitted_close.contains("Edit session COMPLETE")
            && omitted_close.contains(FILE)
            && !omitted_close.contains("NOT edited")
    );

    let safety_args =
        json!({"project_id":pid,"file_path":FILE,"method_name":METHOD,"output_json":true});
    let safety: Value =
        serde_json::from_str(&client.ok("check_edit_safety", safety_args.clone())?)?;
    ensure!(safety["verdict"].as_str().is_some_and(|v| !v.is_empty()));
    let references_args = json!({"project_id":pid,"symbol_name":METHOD});
    let references = client.ok("find_symbol_references", references_args.clone())?;
    ensure!(references.contains(METHOD) && references.contains(FILE));
    let context_args = json!({"project_id":pid,"file_path":FILE,"method_name":METHOD,"include_style_profile":false,"include_pattern_examples":false,"output_json":true});
    ensure!(
        client
            .ok("prepare_implementation_context", context_args.clone())?
            .contains("file.size > maxBytes")
    );
    for (name, args) in [
        ("project_info", json!({"project_id":pid})),
        ("get_index_freshness", json!({"project_id":pid})),
        ("search_memory", search_args),
        ("grep_project", grep_args),
        ("get_chunk", chunk_args),
        ("get_full_method_body", body_args),
        ("resolve_id", resolve_args),
        ("get_method_edit_context", method_args),
        ("get_page_context", page_args),
        ("get_change_set", change_args),
        ("pre_commit_review", review_args),
        ("get_review_decisions", decisions_args),
        ("read_memory_bank", memory_args),
        ("begin_edit_session", begin_args),
        ("complete_edit_session", complete_args),
        ("check_edit_safety", safety_args),
        ("find_symbol_references", references_args),
        ("prepare_implementation_context", context_args),
    ] {
        for invalid in ["../invalid-project", "unknown-project"] {
            let mut bad = args.clone();
            bad["project_id"] = json!(invalid);
            require_error(&client.call(name, bad)?)
                .with_context(|| format!("{name} accepted {invalid}"))?;
        }
    }
    let empty_id = identity(&client.ok(
        "index_project",
        json!({"directory":empty,"project_name":"Empty wire fixture","wait":true}),
    )?)?;
    let empty_results: Value = serde_json::from_str(&client.ok(
        "search_memory",
        json!({"project_id":empty_id,"query":"absent","semantic":false,"output_json":true}),
    )?)?;
    ensure!(
        empty_results["results"] == json!([]),
        "Empty index fabricated source: {empty_results}"
    );
    require_error(&client.call(
        "get_full_method_body",
        json!({"project_id":empty_id,"file_path":FILE,"line_start":1}),
    )?)?;
    let too_many = client.call(
        "index_project",
        json!({"directory":oversized,"project_name":"Over configured file limit","wait":true}),
    )?;
    require_error(&too_many)?;
    ensure!(too_many.to_string().to_lowercase().contains("limit"));
    let bounded = client.ok("search_memory", json!({"project_id":pid,"query":"bulkentry","semantic":false,"fts_mode":"loose","use_mmr":false,"include_content":true,"max_results":8,"output_json":true,"max_tokens":2048}))?;
    ensure!(
        bounded.len() <= 8192,
        "Complete response exceeds requested budget"
    );
    let bounded: Value = serde_json::from_str(&bounded)?;
    ensure!(
        bounded["response_page"]["next_offset"].as_u64().is_some(),
        "Omitted source records need continuation: {bounded}"
    );
    require_error(&client.call(
        "search_memory",
        json!({"project_id":pid,"query":"bulkentry","semantic":false,"max_tokens":1}),
    )?)?;
    ensure!(client.called == TOOLS.into_iter().map(str::to_owned).collect());
    ensure!(std::fs::read_to_string(root.join(FILE))? == upload);
    client.close()?;
    println!(
        "MCP_CONTRACT_REPORT {}",
        json!({"tools":TOOLS,"count":20,"invalid_identity_cases":36,"empty_index":true,"admission_limit":32,"bounded_reply_bytes":8192,"server_closed":true,"optional_sidecar":"absent","provider":"none"})
    );
    Ok(())
}

#[test]
fn malformed_or_error_results_cannot_pass_success_contract() {
    assert!(success_text(&json!({"error":{"code":-32602,"message":"unknown project"}})).is_err());
    assert!(
        success_text(
            &json!({"result":{"isError":true,"content":[{"type":"text","text":"unavailable"}]}})
        )
        .is_err()
    );
    assert!(success_text(&json!({"result":{"content":[]}})).is_err());
    assert!(
        require_error(&json!({"result":{"content":[{"type":"text","text":"looks successful"}]}}))
            .is_err()
    );
}
